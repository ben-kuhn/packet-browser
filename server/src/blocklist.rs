//! Fetches remote hosts-format blocklists on a schedule and holds them in
//! process memory. The filtering proxy (see `crate::proxy`) consults this set
//! before every DNS lookup, so entries here are enforced on every request
//! Firefox issues -- no more coupling the container's /etc/hosts to the block
//! layer.

use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

const BLOCKLIST_FETCH_TIMEOUT_SECS: u64 = 30;
const MAX_BLOCKLIST_BYTES: usize = 16 * 1024 * 1024;
const MAX_DOMAIN_LEN: usize = 253;

/// Global blocklist state. Set once at startup; readers get an Arc clone,
/// the refresh thread holds the write side.
static DOMAIN_BLOCKLIST: OnceLock<Arc<RwLock<HashSet<String>>>> = OnceLock::new();

/// Initialize the (empty) blocklist. Idempotent; safe to call from `main`
/// before any consumer looks at it.
pub fn init_domain_blocklist() {
    let _ = DOMAIN_BLOCKLIST.set(Arc::new(RwLock::new(HashSet::new())));
}

/// Case-insensitive membership check. Returns false if the blocklist has
/// not been initialized yet, so consumers can call this unconditionally.
pub fn is_domain_blocked(host: &str) -> bool {
    let Some(state) = DOMAIN_BLOCKLIST.get() else {
        return false;
    };
    let key = host.to_ascii_lowercase();
    let guard = match state.read() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    guard.contains(&key)
}

// Start background blocklist manager: fetches on startup then refreshes on interval
pub fn start_blocklist_manager(urls: Vec<String>, refresh_hours: u64) {
    if urls.is_empty() {
        return;
    }

    // Ensure the shared state exists so callers of `is_domain_blocked` don't
    // race the initial fetch.
    init_domain_blocklist();

    if let Err(e) = update_blocklist(&urls) {
        eprintln!("Failed to update blocklist: {}", e);
    }

    thread::spawn(move || loop {
        thread::sleep(Duration::from_secs(refresh_hours * 3600));
        if let Err(e) = update_blocklist(&urls) {
            eprintln!("Failed to refresh blocklist: {}", e);
        }
    });
}

fn update_blocklist(urls: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut domains: HashSet<String> = HashSet::new();

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(BLOCKLIST_FETCH_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    for url in urls {
        let body = fetch_capped(&client, url)?;
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Hosts format: "0.0.0.0 domain.com" or "127.0.0.1 domain.com"
            let mut parts = line.split_whitespace();
            if let (Some(_ip), Some(domain)) = (parts.next(), parts.next()) {
                if domain.len() > MAX_DOMAIN_LEN {
                    continue;
                }
                // Skip well-known local entries that should not be blocked.
                if matches!(domain, "localhost" | "localhost.localdomain" | "broadcasthost") {
                    continue;
                }
                domains.insert(domain.to_ascii_lowercase());
            }
        }
    }

    let count = domains.len();
    if let Some(state) = DOMAIN_BLOCKLIST.get() {
        let mut guard = match state.write() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        *guard = domains;
    }
    eprintln!("Blocklist updated: {} entries", count);
    Ok(())
}

fn fetch_capped(client: &reqwest::blocking::Client, url: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut resp = client.get(url).send()?.error_for_status()?;

    // Reject up front if the server announces a body larger than we'll accept.
    if let Some(len) = resp.content_length() {
        if len as usize > MAX_BLOCKLIST_BYTES {
            return Err(format!(
                "Blocklist {} announced {} bytes, exceeds limit {}",
                url, len, MAX_BLOCKLIST_BYTES
            )
            .into());
        }
    }

    // Read in bounded chunks so a server that lies about Content-Length cannot
    // exhaust memory.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = resp.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if buf.len() + n > MAX_BLOCKLIST_BYTES {
            return Err(format!(
                "Blocklist {} exceeded maximum size {} bytes",
                url, MAX_BLOCKLIST_BYTES
            )
            .into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    String::from_utf8(buf).map_err(Into::into)
}
