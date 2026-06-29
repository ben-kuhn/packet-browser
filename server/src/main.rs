use packet_browser_server::{
    blocklist::start_blocklist_manager,
    browser::BrowserInstance,
    config::Config,
    filter::validate_url,
    logger::{LogEntry, LogStatus, Logger},
    session::{validate_callsign, Session},
};
use packet_browser_shared::compress::brotli_compress;
use packet_browser_shared::protocol::{Request, Response, Status};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

const VERSION: &str = "0.2.0";
const MAX_CONNECTIONS: usize = 50;
const MAX_LINE_LENGTH: usize = 1024;
const MAX_BODY_SIZE: usize = 1024 * 1024;
const REQUEST_TIMEOUT_SECS: u64 = 300;

fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        let port = std::env::var("LISTEN_PORT")
            .ok()
            .and_then(|v| v.parse::<u16>().ok())
            .unwrap_or(63004);
        match TcpStream::connect(format!("127.0.0.1:{}", port)) {
            Ok(_) => std::process::exit(0),
            Err(_) => std::process::exit(1),
        }
    }

    let config = Arc::new(Config::from_env());
    let connection_count = Arc::new(AtomicUsize::new(0));

    println!("Starting packet-browser-server v{}", VERSION);
    println!("Listening on port {}", config.listen_port);

    if config.blocklist_enabled && !config.blocklist_urls.is_empty() {
        start_blocklist_manager(config.blocklist_urls.clone(), config.blocklist_refresh_hours);
    }

    let listener = TcpListener::bind(format!("0.0.0.0:{}", config.listen_port))
        .expect("Failed to bind to port");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if connection_count.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                    eprintln!("[LIMIT] Max connections reached, rejecting");
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }

                let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".to_string());
                eprintln!("[CONNECT] New connection from {}", peer);

                if let Err(e) = stream.set_read_timeout(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))) {
                    eprintln!("[ERROR] Failed to set read timeout: {}", e);
                    continue;
                }

                let config = Arc::clone(&config);
                let count = Arc::clone(&connection_count);
                count.fetch_add(1, Ordering::SeqCst);

                thread::spawn(move || {
                    if let Err(e) = handle_connection(stream, config) {
                        eprintln!("[ERROR] Connection error from {}: {}", peer, e);
                    }
                    count.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {}", e);
            }
        }
    }
}

fn handle_connection(mut stream: TcpStream, config: Arc<Config>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    let callsign = {
        let mut input = String::new();
        let bytes_read = reader.read_line(&mut input)?;
        if bytes_read == 0 {
            return Ok(());
        }
        if bytes_read > MAX_LINE_LENGTH {
            eprintln!("[AUTH] Callsign too long");
            return Ok(());
        }
        input.trim().to_string()
    };

    let callsign = match validate_callsign(&callsign) {
        Ok(call) => call,
        Err(_) => {
            eprintln!("[AUTH] Invalid callsign: {:?}", callsign);
            send_error_response(&mut stream, "Invalid callsign format.")?;
            return Ok(());
        }
    };
    eprintln!("[AUTH] Callsign validated: {}", callsign);

    let mut session = Session::new(callsign.clone());

    write!(stream, "All activity is logged including your callsign.\nType AGREE to proceed: ")?;
    stream.flush()?;

    let mut input = String::new();
    let bytes_read = reader.read_line(&mut input)?;
    if bytes_read == 0 || bytes_read > MAX_LINE_LENGTH {
        return Ok(());
    }

    if input.trim().to_uppercase() != "AGREE" {
        writeln!(stream, "Acknowledgment required. Goodbye.")?;
        return Ok(());
    }

    eprintln!("[AUTH] {} agreed to terms", callsign);
    session.acknowledge();

    let logger = Logger::new("/var/log/packet-browser/access.log");
    let log_entry = LogEntry::new(
        callsign.clone(),
        "AGREED".to_string(),
        LogStatus::Agreed,
        None,
    );
    let _ = logger.log(&log_entry);

    writeln!(stream, "\nWelcome {}! Packet browser v{} ready.\n", callsign, VERSION)?;

    eprintln!("[BROWSER] Initializing for {}", callsign);
    let mut browser: Option<BrowserInstance> = match BrowserInstance::new(&callsign) {
        Ok(b) => { eprintln!("[BROWSER] Ready for {}", callsign); Some(b) }
        Err(e) => {
            eprintln!("[BROWSER] Failed to initialize: {}", e);
            send_error_response(&mut stream, "Browser initialization failed")?;
            return Ok(());
        }
    };

    eprintln!("[PORTAL] Loading {} for {}", config.portal_url, callsign);
    if let Err(e) = handle_request(&mut session, &mut browser, &callsign, &config, &logger, &mut stream, &config.portal_url, None) {
        eprintln!("[PORTAL] Failed for {}: {}", callsign, e);
    }

    loop {
        if session.is_timed_out(config.idle_timeout_minutes) {
            writeln!(stream, "\nSession timed out due to inactivity.")?;
            break;
        }

        let request = match read_request(&mut reader) {
            Ok(Some(req)) => req,
            Ok(None) => break,
            Err(e) => {
                eprintln!("[PROTO] Read error from {}: {}", callsign, e);
                break;
            }
        };

        session.touch();

        match &request {
            Request::Get { url } => eprintln!("[CMD] {} GET {}", callsign, url),
            Request::Post { url, body } => eprintln!("[CMD] {} POST {} ({} bytes)", callsign, url, body.len()),
        }

        let url = match &request {
            Request::Get { url } | Request::Post { url, .. } => url.clone(),
        };

        let body = match &request {
            Request::Post { body, .. } => Some(body.clone()),
            _ => None,
        };

        if let Err(e) = handle_request(&mut session, &mut browser, &callsign, &config, &logger, &mut stream, &url, body.as_deref()) {
            eprintln!("[ERROR] Request error for {}: {}", callsign, e);
        }
    }

    eprintln!("[CONNECT] Session ended for {}", callsign);
    Ok(())
}

fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut header_line = String::new();
    let bytes_read = reader.read_line(&mut header_line)?;
    if bytes_read == 0 {
        return Ok(None);
    }
    if bytes_read > MAX_LINE_LENGTH {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "URL too long"));
    }

    let trimmed = header_line.trim();

    if trimmed.starts_with("GET ") {
        let url = trimmed[4..].to_string();
        if url.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Empty URL"));
        }
        Ok(Some(Request::Get { url }))
    } else if trimmed.starts_with("POST ") {
        let url = trimmed[5..].to_string();
        if url.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Empty URL"));
        }

        let mut len_buf = [0u8; 4];
        reader.read_exact(&mut len_buf)?;
        let body_len = u32::from_be_bytes(len_buf) as usize;

        if body_len > MAX_BODY_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Body too large: {} bytes (max {})", body_len, MAX_BODY_SIZE),
            ));
        }

        let mut body = vec![0u8; body_len];
        reader.read_exact(&mut body)?;

        Ok(Some(Request::Post { url, body }))
    } else {
        Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid request format"))
    }
}

fn handle_request(
    session: &mut Session,
    browser: &mut Option<BrowserInstance>,
    callsign: &str,
    config: &Config,
    logger: &Logger,
    stream: &mut TcpStream,
    url: &str,
    _body: Option<&[u8]>,
) -> std::io::Result<()> {
    if let Err(e) = validate_url(url, &config.blocked_ranges) {
        eprintln!("[FILTER] Blocked URL {} for {}: {}", url, callsign, e);
        let log_entry = LogEntry::new(
            session.callsign.clone(),
            url.to_string(),
            LogStatus::Blocked,
            Some(e.to_string()),
        );
        let _ = logger.log(&log_entry);
        send_error_response(stream, "URL blocked")?;
        return Ok(());
    }

    eprintln!("[FETCH] Loading {} for {}", url, callsign);

    let html = loop {
        let b = match browser.as_ref() {
            Some(b) => b,
            None => {
                eprintln!("[BROWSER] No browser instance, creating for {}", callsign);
                *browser = BrowserInstance::new(callsign).ok();
                if browser.is_none() {
                    send_error_response(stream, "Browser unavailable")?;
                    return Ok(());
                }
                continue;
            }
        };

        match b.fetch_page(url) {
            Ok(html) => break html,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("connection is closed") || err_str.contains("BrowserCrashed") {
                    eprintln!("[BROWSER] Chrome crashed, restarting for {}", callsign);
                    *browser = BrowserInstance::new(callsign).ok();
                    if browser.is_none() {
                        send_error_response(stream, "Browser unavailable")?;
                        return Ok(());
                    }
                    continue;
                }
                eprintln!("[FETCH] Error loading {} for {}: {}", url, callsign, e);
                let log_entry = LogEntry::new(
                    session.callsign.clone(),
                    url.to_string(),
                    LogStatus::Error,
                    Some(e.to_string()),
                );
                let _ = logger.log(&log_entry);
                send_error_response(stream, "Failed to load page")?;
                return Ok(());
            }
        }
    };

    let log_entry = LogEntry::new(
        session.callsign.clone(),
        url.to_string(),
        LogStatus::Ok,
        None,
    );
    let _ = logger.log(&log_entry);

    session.current_url = Some(url.to_string());

    let compressed = match brotli_compress(html.as_bytes(), config.brotli_quality) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("[COMPRESS] Error compressing for {}: {}", callsign, e);
            send_error_response(stream, "Compression error")?;
            return Ok(());
        }
    };

    eprintln!("[SEND] {} bytes -> {} bytes compressed for {}", html.len(), compressed.len(), callsign);

    let response = Response {
        status: Status::Ok,
        payload: compressed,
    };

    stream.write_all(&response.encode())?;
    stream.flush()?;

    Ok(())
}

fn send_error_response(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    let compressed = brotli_compress(message.as_bytes(), 11)
        .unwrap_or_else(|_| message.as_bytes().to_vec());

    let response = Response {
        status: Status::Err,
        payload: compressed,
    };

    stream.write_all(&response.encode())?;
    stream.flush()?;
    Ok(())
}
