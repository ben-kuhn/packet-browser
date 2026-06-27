use std::fs;
use std::io;
use std::thread;
use std::time::Duration;

const BLOCKLIST_START: &str = "# BLOCKLIST-MANAGED START";
const BLOCKLIST_END: &str = "# BLOCKLIST-MANAGED END";
const HOSTS_PATH: &str = "/etc/hosts";

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

    for url in urls {
        let body = reqwest::blocking::get(url)?.text()?;
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            // Hosts format: "0.0.0.0 domain.com" or "127.0.0.1 domain.com"
            let mut parts = line.split_whitespace();
            if let (Some(_ip), Some(domain)) = (parts.next(), parts.next()) {
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

    fs::write(HOSTS_PATH, content)
}
