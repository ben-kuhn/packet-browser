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
use std::time::{Duration, Instant};

const VERSION: &str = "0.4.0";
const MAX_CONNECTIONS: usize = 50;
const MAX_CONNECTIONS_PER_IP: usize = 5;
const MAX_LINE_LENGTH: usize = 1024;
const MAX_BODY_SIZE: usize = 1024 * 1024;
// Ambient socket write timeout. Applied to the whole session; guards against
// a stalled peer refusing to drain the kernel send buffer.
const WRITE_TIMEOUT_SECS: u64 = 300;
// Ambient socket read timeout. Deliberately short so a stalled read wakes up
// often enough to re-check the wall-clock idle deadline; NOT itself the
// session-death cutoff.
const READ_POLL_SECS: u64 = 30;
// Wall-clock deadline for the pre-AGREE handshake reads.
const PRE_AUTH_TIMEOUT_SECS: u64 = 300;

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

                if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(READ_POLL_SECS))) {
                    eprintln!("[ERROR] Failed to set read timeout: {}", e);
                    continue;
                }

                if let Err(e) = stream.set_write_timeout(Some(Duration::from_secs(WRITE_TIMEOUT_SECS))) {
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
//
// The ambient socket read timeout (READ_POLL_SECS) is used only as a wakeup
// tick: WouldBlock/TimedOut errors are retried until the caller-supplied
// wall-clock `deadline` expires, at which point TimedOut is returned. This
// lets the session-level idle timeout — not the SO_RCVTIMEO knob — decide
// when to close an idle connection.
fn read_bounded_line_until(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
) -> std::io::Result<Option<String>> {
    let mut buf: Vec<u8> = Vec::new();
    let cap = (MAX_LINE_LENGTH as u64) + 1;
    loop {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Idle deadline exceeded",
            ));
        }
        let remaining = cap.saturating_sub(buf.len() as u64);
        if remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Line exceeded maximum length",
            ));
        }
        match reader.by_ref().take(remaining).read_until(b'\n', &mut buf) {
            Ok(0) if buf.is_empty() => return Ok(None),
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "EOF mid-line",
                ))
            }
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    return String::from_utf8(buf).map(Some).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Invalid UTF-8 in line",
                        )
                    });
                }
                if buf.len() as u64 >= cap {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "Line exceeded maximum length",
                    ));
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                // read_until may have appended bytes before the timeout;
                // if that included the delimiter we're already done.
                if buf.last() == Some(&b'\n') {
                    return String::from_utf8(buf).map(Some).map_err(|_| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "Invalid UTF-8 in line",
                        )
                    });
                }
                continue;
            }
            Err(e) => return Err(e),
        }
    }
}

// Read exactly `buf.len()` bytes, retrying WouldBlock/TimedOut from the
// ambient socket read timeout until the caller-supplied deadline expires.
fn read_exact_until(
    reader: &mut BufReader<TcpStream>,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Body read deadline exceeded",
            ));
        }
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "Short body",
                ))
            }
            Ok(n) => filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                continue
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
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

    let pre_auth_deadline = Instant::now() + Duration::from_secs(PRE_AUTH_TIMEOUT_SECS);

    let callsign = match read_bounded_line_until(&mut reader, pre_auth_deadline)? {
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
    // and again as the raw payload, so what should be the AGREE line is often
    // a duplicate of the callsign line. LinBPQ also prefixes new sessions
    // with node-status text such as `*** Disconnected from Stream N` — the
    // trailing housekeeping notice from a prior session's teardown — before
    // the real user input arrives. Skip both classes of noise before we
    // decide whether the next line is AGREE.
    let input = loop {
        let line = match read_bounded_line_until(&mut reader, pre_auth_deadline)? {
            Some(s) => s,
            None => return Ok(()),
        };
        match classify_preauth_line(&line, &callsign) {
            PreAuthLine::DuplicateCallsign => {
                eprintln!(
                    "[AUTH] Discarding duplicate callsign line delivered by LinBPQ for {}",
                    callsign,
                );
                continue;
            }
            PreAuthLine::LinbpqStatus => {
                eprintln!(
                    "[AUTH] Discarding LinBPQ status line while awaiting AGREE from {}: {:?}",
                    callsign,
                    line.trim(),
                );
                continue;
            }
            PreAuthLine::Candidate => break line,
        }
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
        // Wall-clock idle deadline, measured from the last time we heard from
        // or sent to this peer. The socket read timeout fires every
        // READ_POLL_SECS but is not itself the session-death cutoff; it only
        // wakes us to re-check this deadline.
        let idle_deadline = session.last_activity
            + Duration::from_secs(config.idle_timeout_minutes * 60);

        let request = match read_request(&mut reader, idle_deadline) {
            Ok(Some(req)) => req,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                eprintln!(
                    "[IDLE] {} idle > {} min, closing session",
                    callsign, config.idle_timeout_minutes
                );
                let _ = writeln!(stream, "\nSession timed out due to inactivity.");
                break;
            }
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
                // Reset the idle clock: we successfully sent a response and
                // any delivery latency on that response should not eat into
                // the operator's think time.
                session.touch();
                continue;
            }
        };

        if let Err(e) = handle_request(&mut session, &mut browser, &callsign, Arc::clone(&config), &logger, &mut stream, &url, if_none_match.as_deref()) {
            eprintln!("[ERROR] Request error for {}: {}", callsign, e);
        }

        // Start the idle countdown from response-send time, not from when
        // the request arrived. A slow AX.25 downlink can take minutes to
        // deliver even a small page, and the operator still needs a full
        // idle window to read and click.
        session.touch();
    }

    eprintln!("[CONNECT] Session ended for {}", callsign);
    Ok(())
}

fn read_request(
    reader: &mut BufReader<TcpStream>,
    deadline: Instant,
) -> std::io::Result<Option<Request>> {
    let header_line = match read_bounded_line_until(reader, deadline)? {
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
        read_exact_until(reader, &mut len_buf, deadline)?;
        let body_len = u32::from_be_bytes(len_buf) as usize;

        if body_len > MAX_BODY_SIZE {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("Body too large: {} bytes (max {})", body_len, MAX_BODY_SIZE),
            ));
        }

        let mut body = vec![0u8; body_len];
        read_exact_until(reader, &mut body, deadline)?;

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

#[derive(Debug, PartialEq, Eq)]
enum PreAuthLine {
    DuplicateCallsign,
    LinbpqStatus,
    Candidate,
}

// Decide whether a line delivered by LinBPQ during the pre-AGREE window is
// housekeeping noise we should silently skip or a genuine input candidate
// (which the caller will then compare against the literal "AGREE").
fn classify_preauth_line(line: &str, callsign: &str) -> PreAuthLine {
    let trimmed = line.trim().to_uppercase();
    if trimmed == callsign {
        return PreAuthLine::DuplicateCallsign;
    }
    if trimmed.starts_with("***") {
        return PreAuthLine::LinbpqStatus;
    }
    PreAuthLine::Candidate
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // Pair a client socket with a server-side BufReader whose SO_RCVTIMEO is
    // deliberately short — mirrors production wiring so tests exercise the
    // real WouldBlock/TimedOut retry path.
    fn socket_pair(read_poll: Duration) -> (TcpStream, BufReader<TcpStream>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        server.set_read_timeout(Some(read_poll)).unwrap();
        (client, BufReader::new(server))
    }

    // Regression: before the fix, a socket-level read timeout firing while
    // we waited between requests was surfaced as a fatal WouldBlock error
    // and killed the browsing session. The new helper must retry across
    // several SO_RCVTIMEO wakeups and successfully assemble the line once
    // the peer eventually writes.
    #[test]
    fn read_bounded_line_survives_socket_timeouts_before_first_byte() {
        let (mut client, mut reader) = socket_pair(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_secs(5);

        let writer = thread::spawn(move || {
            // Sleep long enough that the server-side read_until fires
            // WouldBlock several times before any bytes arrive.
            thread::sleep(Duration::from_millis(300));
            client.write_all(b"HELLO\n").unwrap();
            client.flush().unwrap();
            client
        });

        let line = read_bounded_line_until(&mut reader, deadline).unwrap();
        assert_eq!(line.as_deref(), Some("HELLO\n"));
        drop(writer.join().unwrap());
    }

    // The wall-clock deadline is the only thing that terminates an idle
    // wait; the loop must eventually give up with TimedOut when the peer
    // never speaks, and it must not falsely report a protocol error.
    #[test]
    fn read_bounded_line_returns_timed_out_at_deadline() {
        let (_client, mut reader) = socket_pair(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_millis(250);

        let err = read_bounded_line_until(&mut reader, deadline).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    // Byte-order-preserving: a slow line delivered a chunk at a time across
    // multiple SO_RCVTIMEO wakeups must still reassemble correctly.
    #[test]
    fn read_bounded_line_reassembles_across_partial_reads() {
        let (mut client, mut reader) = socket_pair(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_secs(5);

        let writer = thread::spawn(move || {
            for chunk in ["GET ", "https://exa", "mple.com\n"] {
                thread::sleep(Duration::from_millis(120));
                client.write_all(chunk.as_bytes()).unwrap();
                client.flush().unwrap();
            }
            client
        });

        let line = read_bounded_line_until(&mut reader, deadline).unwrap();
        assert_eq!(line.as_deref(), Some("GET https://example.com\n"));
        drop(writer.join().unwrap());
    }

    #[test]
    fn read_exact_until_survives_socket_timeouts() {
        let (mut client, mut reader) = socket_pair(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_secs(5);

        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            client.write_all(&[1u8, 2, 3, 4]).unwrap();
            client.flush().unwrap();
            client
        });

        let mut buf = [0u8; 4];
        read_exact_until(&mut reader, &mut buf, deadline).unwrap();
        assert_eq!(buf, [1, 2, 3, 4]);
        drop(writer.join().unwrap());
    }

    #[test]
    fn read_exact_until_returns_timed_out_at_deadline() {
        let (_client, mut reader) = socket_pair(Duration::from_millis(50));
        let deadline = Instant::now() + Duration::from_millis(250);

        let mut buf = [0u8; 4];
        let err = read_exact_until(&mut reader, &mut buf, deadline).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn preauth_classifier_treats_duplicate_callsign_as_skippable() {
        assert_eq!(
            classify_preauth_line("W1TEST\n", "W1TEST"),
            PreAuthLine::DuplicateCallsign,
        );
        assert_eq!(
            classify_preauth_line("  w1test\r\n", "W1TEST"),
            PreAuthLine::DuplicateCallsign,
        );
    }

    // Regression: after graceful-reconnect, LinBPQ prefixes the freshly-spawned
    // WEB session's input stream with a housekeeping line like
    // "*** Disconnected from Stream N" from the prior session's teardown. The
    // server previously took that as the AGREE reply and rejected the session
    // — and the AX.25 client's auto-reconnect then bounced against the closed
    // TCP forever. Any line beginning with `***` must be treated as LinBPQ
    // noise and skipped.
    #[test]
    fn preauth_classifier_skips_linbpq_status_lines() {
        assert_eq!(
            classify_preauth_line("*** Disconnected from Stream 10\r\n", "W1TEST"),
            PreAuthLine::LinbpqStatus,
        );
        assert_eq!(
            classify_preauth_line("*** Connected to WEB         \r\n", "W1TEST"),
            PreAuthLine::LinbpqStatus,
        );
    }

    #[test]
    fn preauth_classifier_passes_real_input_through() {
        assert_eq!(
            classify_preauth_line("AGREE\n", "W1TEST"),
            PreAuthLine::Candidate,
        );
        assert_eq!(
            classify_preauth_line("DENY\n", "W1TEST"),
            PreAuthLine::Candidate,
        );
        assert_eq!(
            classify_preauth_line("\n", "W1TEST"),
            PreAuthLine::Candidate,
        );
    }
}
