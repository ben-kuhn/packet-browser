use packet_browser_server::{
    blocklist::{init_domain_blocklist, start_blocklist_manager},
    browser::{set_proxy_port, BrowserError, BrowserInstance},
    config::Config,
    filter::{validate_url, UrlError},
    logger::{LogEntry, LogStatus, Logger},
    proxy::start_proxy,
    session::{validate_callsign_with_allowlist, Session},
};
use packet_browser_shared::compress::brotli_compress;
use packet_browser_shared::protocol::{Request, Response, Status};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

const VERSION: &str = "0.3.0";
const MAX_CONNECTIONS: usize = 50;
const MAX_CONNECTIONS_PER_IP: usize = 5;
const MAX_LINE_LENGTH: usize = 1024;
const MAX_BODY_SIZE: usize = 1024 * 1024;
const REQUEST_TIMEOUT_SECS: u64 = 300;

type PeerCounts = Arc<Mutex<HashMap<IpAddr, usize>>>;

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
    let peer_counts: PeerCounts = Arc::new(Mutex::new(HashMap::new()));

    // Initialize the domain blocklist state before anything reads it.
    init_domain_blocklist();

    // In-process SSRF filtering proxy. Firefox will be pointed at this port
    // so every subresource load goes through validate_url + a pinned DNS
    // resolution. Fatal if it fails to start — the browser has no other way
    // to enforce the SSRF policy at fetch time.
    let proxy_port = match start_proxy(config.blocked_ranges.clone()) {
        Ok(p) => {
            println!("Filtering proxy listening on 127.0.0.1:{}", p);
            p
        }
        Err(e) => {
            eprintln!("[FATAL] Failed to start filtering proxy: {}", e);
            std::process::exit(1);
        }
    };
    set_proxy_port(proxy_port);

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
                let peer_addr = stream.peer_addr().ok();
                let peer = peer_addr.map(|a| a.to_string()).unwrap_or_else(|| "unknown".to_string());
                eprintln!("[CONNECT] New connection from {}", peer);

                if let Err(e) = stream.set_read_timeout(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))) {
                    eprintln!("[ERROR] Failed to set read timeout: {}", e);
                    continue;
                }

                if let Err(e) = stream.set_write_timeout(Some(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))) {
                    eprintln!("[ERROR] Failed to set write timeout: {}", e);
                    continue;
                }

                // Per-IP cap so a single peer cannot occupy every global slot.
                let peer_ip = peer_addr.map(|a| a.ip());
                if let Some(ip) = peer_ip {
                    let mut map = match peer_counts.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    let entry = map.entry(ip).or_insert(0);
                    if *entry >= MAX_CONNECTIONS_PER_IP {
                        eprintln!("[LIMIT] Per-IP cap reached for {}, rejecting", ip);
                        let _ = stream.shutdown(std::net::Shutdown::Both);
                        continue;
                    }
                    *entry += 1;
                }

                let config = Arc::clone(&config);
                let count = Arc::clone(&connection_count);
                let peers = Arc::clone(&peer_counts);

                // Race-free admission: claim a slot atomically, then release it
                // if we busted the cap.
                let prev = count.fetch_add(1, Ordering::SeqCst);
                if prev >= MAX_CONNECTIONS {
                    count.fetch_sub(1, Ordering::SeqCst);
                    if let Some(ip) = peer_ip {
                        let mut map = match peers.lock() {
                            Ok(g) => g,
                            Err(p) => p.into_inner(),
                        };
                        if let Some(c) = map.get_mut(&ip) {
                            *c = c.saturating_sub(1);
                            if *c == 0 {
                                map.remove(&ip);
                            }
                        }
                    }
                    eprintln!("[LIMIT] Max connections reached, rejecting");
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    continue;
                }

                thread::spawn(move || {
                    // RAII guards so both global and per-IP counts are freed
                    // even if handle_connection panics.
                    struct ConnGuard(Arc<AtomicUsize>);
                    impl Drop for ConnGuard {
                        fn drop(&mut self) {
                            self.0.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    struct PeerGuard(PeerCounts, Option<IpAddr>);
                    impl Drop for PeerGuard {
                        fn drop(&mut self) {
                            if let Some(ip) = self.1 {
                                let mut map = match self.0.lock() {
                                    Ok(g) => g,
                                    Err(p) => p.into_inner(),
                                };
                                if let Some(c) = map.get_mut(&ip) {
                                    *c = c.saturating_sub(1);
                                    if *c == 0 {
                                        map.remove(&ip);
                                    }
                                }
                            }
                        }
                    }

                    let _guard = ConnGuard(count);
                    let _peer_guard = PeerGuard(peers, peer_ip);

                    if let Err(e) = handle_connection(stream, config) {
                        eprintln!("[ERROR] Connection error from {}: {}", peer, e);
                    }
                });
            }
            Err(e) => {
                eprintln!("Failed to accept connection: {}", e);
            }
        }
    }
}

// Read up to one line, capped at MAX_LINE_LENGTH bytes including the newline.
// Returns Ok(None) on clean EOF and an InvalidData error if no newline arrives
// before the cap (so a slow attacker cannot stream gigabytes into the buffer).
fn read_bounded_line(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let n = reader
        .by_ref()
        .take((MAX_LINE_LENGTH as u64) + 1)
        .read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    if buf.last() != Some(&b'\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Line exceeded maximum length",
        ));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid UTF-8 in line"))
}

fn handle_connection(mut stream: TcpStream, config: Arc<Config>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);

    // Prompt for the callsign. LinBPQ's HOST driver in `C n HOST 0 S` mode
    // does NOT auto-inject the connecting station's callsign (verified with a
    // packet-capturing bridge in front of the server). The client's BPQ
    // handshake waits for a prompt containing "callsign" before it sends the
    // configured callsign, so a plain "Enter your callsign:" is the trigger.
    // PacketQTH's docs describe auto-injection but that behaviour depends on
    // a different LinBPQ config (CMS mode with user table) that our HOST 0
    // path doesn't opt into.
    write!(stream, "Enter your callsign: ")?;
    stream.flush()?;

    let callsign = match read_bounded_line(&mut reader)? {
        Some(s) => s.trim().to_string(),
        None => return Ok(()),
    };

    let callsign = match validate_callsign_with_allowlist(&callsign, &config.allowed_callsigns) {
        Ok(call) => call,
        Err(_) => {
            eprintln!("[AUTH] Invalid callsign: {:?}", callsign);
            // Send plain text — we're still in the pre-handshake header phase,
            // before the framed request/response protocol starts. A brotli
            // frame here reaches operators (via telnet or an AX.25 terminal)
            // as garbled bytes; plain text at least tells them what's wrong.
            let _ = writeln!(stream, "Invalid callsign format. Disconnecting.");
            let _ = stream.flush();
            return Ok(());
        }
    };
    eprintln!("[AUTH] Callsign validated: {}", callsign);

    let mut session = Session::new(callsign.clone());

    write!(stream, "All activity is logged including your callsign.\nType AGREE to proceed: ")?;
    stream.flush()?;

    // LinBPQ's TELNET driver in HOST 0 S mode delivers the very first line of
    // AX.25 input to the server TWICE: once with telnet-style CRLF conversion
    // and again as the raw payload. That means what should be the AGREE line
    // is often a duplicate of the callsign line. Skip any line that matches
    // the just-validated callsign before checking for AGREE, so the operator
    // consent step doesn't spuriously fail on the demo/off-air setup.
    let input = loop {
        let line = match read_bounded_line(&mut reader)? {
            Some(s) => s,
            None => return Ok(()),
        };
        let trimmed = line.trim().to_uppercase();
        if trimmed == callsign {
            eprintln!(
                "[AUTH] Discarding duplicate callsign line delivered by LinBPQ for {}",
                callsign,
            );
            continue;
        }
        break line;
    };

    if input.trim().to_uppercase() != "AGREE" {
        eprintln!(
            "[AUTH] AGREE rejected for {}. Received bytes ({} chars): {:?} hex: {:02x?}",
            callsign,
            input.len(),
            input,
            input.as_bytes(),
        );
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

    // Everything below AGREE flows through the framed binary protocol
    // (status byte + payload_len + brotli payload). Do NOT write plain text
    // here: a banner or auto-fetched portal response gets interleaved into
    // the byte stream and the next framed Response the client tries to read
    // starts partway through it, so `payload_len` parses as garbage
    // ("Invalid frame: Announced payload_len ..."). If a landing page is
    // desired the client should request it explicitly.
    eprintln!("[BROWSER] Initializing for {}", callsign);
    let mut browser: Option<BrowserInstance> = match BrowserInstance::new(&callsign) {
        Ok(b) => { eprintln!("[BROWSER] Ready for {}", callsign); Some(b) }
        Err(e) => {
            eprintln!("[BROWSER] Failed to initialize: {}", e);
            send_error_response(&mut stream, "Browser initialization failed")?;
            return Ok(());
        }
    };

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
            Request::Get { url, .. } => eprintln!("[CMD] {} GET {}", callsign, url),
            Request::Post { url, body } => eprintln!("[CMD] {} POST {} ({} bytes)", callsign, url, body.len()),
        }

        let (url, if_none_match) = match &request {
            Request::Get { url, if_none_match } => (url.clone(), if_none_match.clone()),
            Request::Post { url, .. } => {
                eprintln!("[CMD] {} POST {} rejected (POST unsupported)", callsign, url);
                if let Err(e) = send_error_response(&mut stream, "POST requests are not supported") {
                    eprintln!("[ERROR] Failed to send POST rejection to {}: {}", callsign, e);
                    break;
                }
                continue;
            }
        };

        if let Err(e) = handle_request(&mut session, &mut browser, &callsign, Arc::clone(&config), &logger, &mut stream, &url, if_none_match.as_deref()) {
            eprintln!("[ERROR] Request error for {}: {}", callsign, e);
        }
    }

    eprintln!("[CONNECT] Session ended for {}", callsign);
    Ok(())
}

fn read_request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let header_line = match read_bounded_line(reader)? {
        Some(s) => s,
        None => return Ok(None),
    };

    let trimmed = header_line.trim();

    if trimmed.starts_with("GET ") {
        let rest = &trimmed[4..];
        if rest.is_empty() {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Empty URL"));
        }
        // Sentinel split mirrors packet_browser_shared::protocol::Request::decode.
        // We keep our own copy because read_request is BufReader-incremental (POST
        // binary body follows) and can't call decode(), which requires the full
        // byte slice buffered up-front.
        let (url, if_none_match) = if let Some((u, e)) = rest.split_once(" IF-NONE-MATCH ") {
            (u.to_string(), Some(e.to_string()))
        } else {
            (rest.to_string(), None)
        };
        Ok(Some(Request::Get { url, if_none_match }))
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
    config: Arc<Config>,
    logger: &Logger,
    stream: &mut TcpStream,
    url: &str,
    if_none_match: Option<&str>,
) -> std::io::Result<()> {
    use packet_browser_shared::protocol::sanitized_html_etag;
    use packet_browser_server::origin_cc::probe_origin_cc;

    if let Err(e) = validate_url(url, &config.blocked_ranges) {
        eprintln!("[FILTER] Rejected URL {} for {}: {}", url, callsign, e);
        let (status, log_status) = match e {
            UrlError::BlockedProtocol(_) | UrlError::BlockedHost(_) => {
                (Status::Blocked, LogStatus::Blocked)
            }
            UrlError::UnresolvableHost(_) | UrlError::InvalidUrl => {
                (Status::Err, LogStatus::Error)
            }
        };
        let message = e.to_string();
        let log_entry = LogEntry::new(
            session.callsign.clone(),
            url.to_string(),
            log_status,
            Some(message.clone()),
        );
        let _ = logger.log(&log_entry);
        send_status_response(stream, status, &message)?;
        return Ok(());
    }

    eprintln!("[FETCH] Loading {} for {}", url, callsign);

    // Kick off the origin cache-control probe concurrently with the Firefox fetch.
    let probe_url = url.to_string();
    let probe_config = Arc::clone(&config);
    let probe_handle = std::thread::spawn(move || probe_origin_cc(&probe_url, &probe_config));

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
            Err(BrowserError::BrowserCrashed) => {
                eprintln!("[BROWSER] Firefox session lost, restarting for {}", callsign);
                *browser = BrowserInstance::new(callsign).ok();
                if browser.is_none() {
                    send_error_response(stream, "Browser unavailable")?;
                    return Ok(());
                }
                continue;
            }
            Err(e) => {
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

    let etag = sanitized_html_etag(&html);
    let directives = probe_handle
        .join()
        .unwrap_or_else(|_| packet_browser_server::origin_cc::OriginDirectives {
            max_age: config.default_max_age_seconds,
        });

    let log_entry = LogEntry::new(
        session.callsign.clone(),
        url.to_string(),
        LogStatus::Ok,
        None,
    );
    let _ = logger.log(&log_entry);

    session.current_url = Some(url.to_string());

    if if_none_match.map(|e| e == etag).unwrap_or(false) {
        eprintln!(
            "[CACHE] {} etag {} matched, sending NotModified for {}",
            callsign, etag, url
        );
        let response = Response {
            status: Status::NotModified,
            etag: etag.clone(),
            max_age: directives.max_age,
            payload: Vec::new(),
        };
        stream.write_all(&response.encode())?;
        stream.flush()?;
        return Ok(());
    }

    let compressed = match brotli_compress(html.as_bytes(), config.brotli_quality) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("[COMPRESS] Error compressing for {}: {}", callsign, e);
            send_error_response(stream, "Compression error")?;
            return Ok(());
        }
    };

    eprintln!(
        "[SEND] {} bytes -> {} bytes compressed (etag={}, max_age={}) for {}",
        html.len(), compressed.len(), etag, directives.max_age, callsign,
    );

    let response = Response {
        status: Status::Ok,
        etag,
        max_age: directives.max_age,
        payload: compressed,
    };
    stream.write_all(&response.encode())?;
    stream.flush()?;

    Ok(())
}

fn send_error_response(stream: &mut TcpStream, message: &str) -> std::io::Result<()> {
    send_status_response(stream, Status::Err, message)
}

fn send_status_response(
    stream: &mut TcpStream,
    status: Status,
    message: &str,
) -> std::io::Result<()> {
    let compressed = brotli_compress(message.as_bytes(), 11)
        .unwrap_or_else(|_| message.as_bytes().to_vec());
    let response = Response {
        status,
        etag: "-".to_string(),
        max_age: -1,
        payload: compressed,
    };
    stream.write_all(&response.encode())?;
    stream.flush()?;
    Ok(())
}
