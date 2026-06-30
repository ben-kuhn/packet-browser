use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::config::FileConfig;

/// Convenience trait that turns mutex-poison panics into "carry on with the
/// previous contents." For a single-user local proxy, blocking forever after
/// any panic anywhere is a worse failure mode than continuing with possibly
/// stale state.
pub trait LockExt<'a, T> {
    fn lock_or_poisoned(&'a self) -> MutexGuard<'a, T>;
}

impl<'a, T> LockExt<'a, T> for Mutex<T> {
    fn lock_or_poisoned(&'a self) -> MutexGuard<'a, T> {
        self.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConnectionState {
    Disconnected,
    AgwpeConnected,
    Connecting,
    Connected,
    Error(String),
}

impl std::fmt::Display for ConnectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionState::Disconnected => write!(f, "Disconnected"),
            ConnectionState::AgwpeConnected => write!(f, "AGWPE Connected"),
            ConnectionState::Connecting => write!(f, "Connecting"),
            ConnectionState::Connected => write!(f, "Connected"),
            ConnectionState::Error(msg) => write!(f, "Error: {}", msg),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LogLevel {
    Info,
    Debug,
    Trace,
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogLevel::Info => write!(f, "INFO"),
            LogLevel::Debug => write!(f, "DEBUG"),
            LogLevel::Trace => write!(f, "TRACE"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Tx,
    Rx,
}

impl std::fmt::Display for Direction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Direction::Tx => write!(f, "TX"),
            Direction::Rx => write!(f, "RX"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugLogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub direction: Option<Direction>,
    pub category: String,
    pub message: String,
    pub details: Option<String>,
}

impl DebugLogEntry {
    pub fn new(level: LogLevel, category: &str, message: &str) -> Self {
        Self {
            timestamp: Utc::now(),
            level,
            direction: None,
            category: category.to_string(),
            message: message.to_string(),
            details: None,
        }
    }

    pub fn with_direction(mut self, direction: Direction) -> Self {
        self.direction = Some(direction);
        self
    }

    pub fn with_details(mut self, details: &str) -> Self {
        self.details = Some(details.to_string());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortInfo {
    pub port_num: u8,
    pub description: String,
}

pub struct AppState {
    pub config: FileConfig,
    pub connection_state: ConnectionState,
    pub debug_log: VecDeque<DebugLogEntry>,
    pub available_ports: Vec<PortInfo>,
    pub agwpe_port_num: Option<u8>,
    log_capacity: usize,
}

impl AppState {
    pub fn new(config: FileConfig) -> Self {
        Self {
            config,
            connection_state: ConnectionState::Disconnected,
            debug_log: VecDeque::new(),
            available_ports: Vec::new(),
            agwpe_port_num: None,
            log_capacity: 1000,
        }
    }

    #[cfg(test)]
    pub fn with_capacity(mut self, capacity: usize) -> Self {
        self.log_capacity = capacity;
        self
    }

    pub fn set_connection_state(&mut self, state: ConnectionState) {
        self.connection_state = state.clone();
        self.add_log(DebugLogEntry::new(
            LogLevel::Info,
            "STATE",
            &format!("State changed to: {}", state),
        ));
    }

    pub fn set_error(&mut self, error: &str) {
        self.connection_state = ConnectionState::Error(error.to_string());
        self.add_log(DebugLogEntry::new(
            LogLevel::Info,
            "ERROR",
            &format!("Error: {}", error),
        ));
    }

    pub fn add_log(&mut self, entry: DebugLogEntry) {
        if self.debug_log.len() >= self.log_capacity {
            self.debug_log.pop_front();
        }
        self.debug_log.push_back(entry);
    }

    pub fn get_logs(&self, min_level: Option<LogLevel>) -> Vec<DebugLogEntry> {
        match min_level {
            None => self.debug_log.iter().cloned().collect(),
            Some(level) => self
                .debug_log
                .iter()
                .filter(|e| e.level <= level)
                .cloned()
                .collect(),
        }
    }

    pub fn set_ports(&mut self, ports: Vec<PortInfo>) {
        self.available_ports = ports;
    }

    pub fn set_agwpe_port(&mut self, port_num: u8) {
        self.agwpe_port_num = Some(port_num);
    }

    pub fn clear_ports(&mut self) {
        self.available_ports.clear();
        self.agwpe_port_num = None;
    }
}

pub type SharedState = Arc<Mutex<AppState>>;

pub fn create_shared_state(config: FileConfig) -> SharedState {
    Arc::new(Mutex::new(AppState::new(config)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_connection_state_display() {
        assert_eq!(ConnectionState::Disconnected.to_string(), "Disconnected");
        assert_eq!(ConnectionState::AgwpeConnected.to_string(), "AGWPE Connected");
        assert_eq!(ConnectionState::Connecting.to_string(), "Connecting");
        assert_eq!(ConnectionState::Connected.to_string(), "Connected");
        assert_eq!(
            ConnectionState::Error("test".to_string()).to_string(),
            "Error: test"
        );
    }

    #[test]
    fn test_debug_log_entry() {
        let entry = DebugLogEntry::new(LogLevel::Info, "TEST", "test message")
            .with_direction(Direction::Tx)
            .with_details("some details");

        assert_eq!(entry.level, LogLevel::Info);
        assert_eq!(entry.category, "TEST");
        assert_eq!(entry.message, "test message");
        assert_eq!(entry.direction, Some(Direction::Tx));
        assert_eq!(entry.details, Some("some details".to_string()));
    }

    #[test]
    fn test_app_state_ring_buffer() {
        let config = FileConfig::default();
        let mut state = AppState::new(config).with_capacity(3);

        state.add_log(DebugLogEntry::new(LogLevel::Info, "TEST", "msg1"));
        state.add_log(DebugLogEntry::new(LogLevel::Info, "TEST", "msg2"));
        state.add_log(DebugLogEntry::new(LogLevel::Info, "TEST", "msg3"));
        state.add_log(DebugLogEntry::new(LogLevel::Info, "TEST", "msg4"));

        assert_eq!(state.debug_log.len(), 3);
        assert_eq!(state.debug_log[0].message, "msg2");
        assert_eq!(state.debug_log[1].message, "msg3");
        assert_eq!(state.debug_log[2].message, "msg4");
    }

    #[test]
    fn test_app_state_log_filtering() {
        let config = FileConfig::default();
        let mut state = AppState::new(config);

        state.add_log(DebugLogEntry::new(LogLevel::Info, "TEST", "info"));
        state.add_log(DebugLogEntry::new(LogLevel::Debug, "TEST", "debug"));
        state.add_log(DebugLogEntry::new(LogLevel::Trace, "TEST", "trace"));

        let all = state.get_logs(None);
        assert_eq!(all.len(), 3);

        let info_only = state.get_logs(Some(LogLevel::Info));
        assert_eq!(info_only.len(), 1);
        assert_eq!(info_only[0].message, "info");

        let info_and_debug = state.get_logs(Some(LogLevel::Debug));
        assert_eq!(info_and_debug.len(), 2);
    }

    #[test]
    fn test_app_state_ports() {
        let config = FileConfig::default();
        let mut state = AppState::new(config);

        assert!(state.available_ports.is_empty());
        assert!(state.agwpe_port_num.is_none());

        state.set_ports(vec![
            PortInfo { port_num: 0, description: "Port 0".to_string() },
            PortInfo { port_num: 1, description: "Port 1".to_string() },
        ]);

        assert_eq!(state.available_ports.len(), 2);

        state.set_agwpe_port(1);
        assert_eq!(state.agwpe_port_num, Some(1));

        state.clear_ports();
        assert!(state.available_ports.is_empty());
        assert!(state.agwpe_port_num.is_none());
    }

    #[test]
    fn test_shared_state() {
        let config = FileConfig::default();
        let state = create_shared_state(config);

        let mut guard = state.lock().unwrap();
        guard.set_connection_state(ConnectionState::Connected);
        drop(guard);

        let guard = state.lock().unwrap();
        assert_eq!(guard.connection_state, ConnectionState::Connected);
    }
}
