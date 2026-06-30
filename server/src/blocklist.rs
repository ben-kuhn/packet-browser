use std::fs;
use std::io::{self, Read};
use std::thread;
use std::time::Duration;

const BLOCKLIST_START: &str = "# BLOCKLIST-MANAGED START";
const BLOCKLIST_END: &str = "# BLOCKLIST-MANAGED END";
const HOSTS_PATH: &str = "/etc/hosts";
const BLOCKLIST_FETCH_TIMEOUT_SECS: u64 = 30;
const MAX_BLOCKLIST_BYTES: usize = 16 * 1024 * 1024;
const MAX_DOMAIN_LEN: usize = 253;

// Start background blocklist manager: fetches on startup then refreshes on interval
pub fn start_blocklist_manager(urls: Vec<String>, refresh_hours: u64) {
    if urls.is_empty() {
        return;
    }

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
    let mut domains: Vec<String> = Vec::new();

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
                // Skip well-known local entries that should not be blocked
                if matches!(domain, "localhost" | "localhost.localdomain" | "broadcasthost") {
                    continue;
                }
                domains.push(format!("0.0.0.0 {}", domain));
            }
        }
    }

    write_hosts_file(&domains)?;
    eprintln!("Blocklist updated: {} entries", domains.len());
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

fn write_hosts_file(domains: &[String]) -> io::Result<()> {
    let existing = fs::read_to_string(HOSTS_PATH).unwrap_or_default();

    // Collect lines that are outside the managed block
    let mut custom_lines: Vec<&str> = Vec::new();
    let mut in_managed = false;
    for line in existing.lines() {
        if line == BLOCKLIST_START {
            in_managed = true;
        } else if line == BLOCKLIST_END {
            in_managed = false;
        } else if !in_managed {
            custom_lines.push(line);
        }
    }

    let mut content = custom_lines.join("\n");
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(BLOCKLIST_START);
    content.push('\n');
    for entry in domains {
        content.push_str(entry);
        content.push('\n');
    }
    content.push_str(BLOCKLIST_END);
    content.push('\n');

    // Prefer atomic write via rename. Fall back to in-place write if the
    // rename crosses a mount boundary (e.g. /etc/hosts mounted in via Docker
    // bind-mount): an EXDEV/EBUSY rename would leave the file untouched.
    let temp_path = format!("{}.tmp", HOSTS_PATH);
    fs::write(&temp_path, &content)?;
    match fs::rename(&temp_path, HOSTS_PATH) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Clean up the temp file we cannot rename, then write in place.
            let _ = fs::remove_file(&temp_path);
            fs::write(HOSTS_PATH, content)
        }
    }
}
