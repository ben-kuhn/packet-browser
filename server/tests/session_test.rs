use packet_browser_server::session::{validate_callsign, Session};

#[test]
fn test_valid_callsigns() {
    assert!(validate_callsign("W1ABC").is_ok());
    assert!(validate_callsign("VE3XYZ").is_ok());
    assert!(validate_callsign("KU0HN").is_ok());
    assert!(validate_callsign("G4ABC").is_ok());
    assert!(validate_callsign("JA1ABC").is_ok());
}

#[test]
fn test_invalid_callsigns() {
    assert!(validate_callsign("").is_err());
    assert!(validate_callsign("123").is_err());
    assert!(validate_callsign("ABCDEF").is_err());
    assert!(validate_callsign("W").is_err());
}

#[test]
fn test_callsign_ssid_stripped() {
    assert_eq!(validate_callsign("W1ABC-1").unwrap(), "W1ABC");
    assert_eq!(validate_callsign("KU0HN-15").unwrap(), "KU0HN");
}

#[test]
fn test_session_creation() {
    let session = Session::new("W1ABC".to_string());
    assert_eq!(session.callsign, "W1ABC");
    assert!(!session.acknowledged);
    assert!(session.current_url.is_none());
}

#[test]
fn test_session_timeout() {
    let session = Session::new("W1ABC".to_string());
    // Should not be timed out immediately
    assert!(!session.is_timed_out(10));
}
