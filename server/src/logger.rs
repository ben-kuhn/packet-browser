use chrono::{DateTime, Utc};
use serde::{Serialize, Deserialize};
use std::fs::OpenOptions;
use std::io::Write;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogStatus {
    Ok,
    Blocked,
    Error,
    Agreed,
}

#[derive(Debug, Serialize)]
pub struct LogEntry {
    pub ts: DateTime<Utc>,
    pub call: String,
    pub url: String,
    pub status: LogStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl LogEntry {
    pub fn new(call: String, url: String, status: LogStatus, reason: Option<String>) -> Self {
        Self {
            ts: Utc::now(),
            call,
            url,
            status,
            reason,
        }
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

pub struct Logger {
    log_path: String,
}

impl Logger {
    pub fn new(log_path: &str) -> Self {
        Self {
            log_path: log_path.to_string(),
        }
    }

    pub fn log(&self, entry: &LogEntry) -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;

        writeln!(file, "{}", entry.to_json())?;
        Ok(())
    }
}
