use regex::Regex;
use thiserror::Error;
use std::sync::LazyLock;
use std::time::Instant;

static CALLSIGN_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^[a-zA-Z0-9]{1,3}[0-9][a-zA-Z0-9]{0,3}[a-zA-Z]$").unwrap()
});

#[derive(Error, Debug)]
pub enum SessionError {
    #[error("Invalid callsign format")]
    InvalidCallsign,
}

pub struct Session {
    pub callsign: String,
    pub acknowledged: bool,
    pub current_url: Option<String>,
    pub last_activity: Instant,
}

impl Session {
    pub fn new(callsign: String) -> Self {
        Self {
            callsign,
            acknowledged: false,
            current_url: None,
            last_activity: Instant::now(),
        }
    }

    pub fn acknowledge(&mut self) {
        self.acknowledged = true;
        self.touch();
    }

    pub fn touch(&mut self) {
        self.last_activity = Instant::now();
    }

    pub fn is_timed_out(&self, timeout_minutes: u64) -> bool {
        self.last_activity.elapsed().as_secs() > timeout_minutes * 60
    }
}

pub fn validate_callsign(callsign: &str) -> Result<String, SessionError> {
    validate_callsign_with_allowlist(callsign, &[])
}

/// Accept a callsign if either it matches the strict ITU-shape regex OR its
/// base (without any -N SSID suffix) appears in the operator-supplied
/// allowlist. Meant for off-air testing where LinBPQ auto-injects synthetic
/// identifiers like DEMOUSR that could not be issued on air. The allowlist
/// entries are compared case-insensitively.
pub fn validate_callsign_with_allowlist(
    callsign: &str,
    allowlist: &[String],
) -> Result<String, SessionError> {
    let call = callsign.split('-').next().unwrap_or(callsign);
    let upper = call.to_uppercase();

    if CALLSIGN_REGEX.is_match(call) {
        return Ok(upper);
    }
    if allowlist.iter().any(|entry| entry.eq_ignore_ascii_case(call)) {
        return Ok(upper);
    }
    Err(SessionError::InvalidCallsign)
}
