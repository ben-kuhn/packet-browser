use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub listen_port: u16,
    pub portal_url: String,
    pub idle_timeout_minutes: u64,
    pub brotli_quality: u32,
    pub blocked_ranges: Vec<String>,
    pub blocklist_urls: Vec<String>,
    pub blocklist_refresh_hours: u64,
    pub blocklist_enabled: bool,
    pub log_rotate_enabled: bool,
    pub log_retain_days: u32,
    pub syslog_enabled: bool,
    pub syslog_host: Option<String>,
    pub syslog_port: u16,
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            listen_port: parse_env_u16("LISTEN_PORT", 63004),
            portal_url: env::var("PORTAL_URL")
                .unwrap_or_else(|_| "https://www.zeroretries.radio".to_string()),
            idle_timeout_minutes: parse_env_u64("IDLE_TIMEOUT_MINUTES", 10),
            brotli_quality: parse_env_u32("BROTLI_QUALITY", 11),
            blocked_ranges: parse_env_vec(
                "BLOCKED_RANGES",
                vec![
                    "127.0.0.0/8".to_string(),
                    "10.0.0.0/8".to_string(),
                    "172.16.0.0/12".to_string(),
                    "192.168.0.0/16".to_string(),
                    "169.254.0.0/16".to_string(),
                ],
            ),
            blocklist_urls: parse_env_vec("BLOCKLIST_URLS", vec![]),
            blocklist_refresh_hours: parse_env_u64("BLOCKLIST_REFRESH_HOURS", 24),
            blocklist_enabled: parse_env_bool("BLOCKLIST_ENABLED", true),
            log_rotate_enabled: parse_env_bool("LOG_ROTATE_ENABLED", true),
            log_retain_days: parse_env_u32("LOG_RETAIN_DAYS", 30),
            syslog_enabled: parse_env_bool("SYSLOG_ENABLED", false),
            syslog_host: env::var("SYSLOG_HOST").ok(),
            syslog_port: parse_env_u16("SYSLOG_PORT", 514),
        }
    }
}

fn parse_env_u16(key: &str, default: u16) -> u16 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(default)
}

fn parse_env_u32(key: &str, default: u32) -> u32 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(default)
}

fn parse_env_u64(key: &str, default: u64) -> u64 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

fn parse_env_bool(key: &str, default: bool) -> bool {
    env::var(key)
        .ok()
        .and_then(|v| match v.to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn parse_env_vec(key: &str, default: Vec<String>) -> Vec<String> {
    env::var(key)
        .ok()
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or(default)
}
