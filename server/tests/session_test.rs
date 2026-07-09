use packet_browser_server::session::{
    validate_callsign, validate_callsign_with_allowlist, Session,
};

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

#[test]
fn test_allowlist_accepts_non_shape_callsigns() {
    // Off-air test identifiers that couldn't be issued on air. Without the
    // allowlist they'd fail (no digit at position 4 in DEMOUSR).
    let allow = vec!["DEMOUSR".to_string(), "TESTOP".to_string()];
    assert_eq!(
        validate_callsign_with_allowlist("DEMOUSR", &allow).unwrap(),
        "DEMOUSR"
    );
    // Case-insensitive match on the allowlist entry.
    assert_eq!(
        validate_callsign_with_allowlist("demousr", &allow).unwrap(),
        "DEMOUSR"
    );
    // SSID suffix stripped before comparing.
    assert_eq!(
        validate_callsign_with_allowlist("DEMOUSR-3", &allow).unwrap(),
        "DEMOUSR"
    );
    // Still rejects things that are neither shape-valid nor allowlisted.
    assert!(validate_callsign_with_allowlist("HELLO", &allow).is_err());
}

#[test]
fn test_allowlist_does_not_break_shape_validation() {
    // Regex-valid callsigns pass even when the allowlist is empty (this is
    // the default behaviour and the wrapper must not regress it).
    assert_eq!(
        validate_callsign_with_allowlist("W1TEST", &[]).unwrap(),
        "W1TEST"
    );
    // And also with a populated allowlist.
    let allow = vec!["DEMOUSR".to_string()];
    assert_eq!(
        validate_callsign_with_allowlist("VE3XYZ", &allow).unwrap(),
        "VE3XYZ"
    );
}
