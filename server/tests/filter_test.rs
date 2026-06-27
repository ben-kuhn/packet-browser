use packet_browser_server::filter::{validate_url, UrlError};

#[test]
fn test_blocked_protocols() {
    assert!(matches!(validate_url("file:///etc/passwd", &[]), Err(UrlError::BlockedProtocol(_))));
    assert!(matches!(validate_url("ftp://example.com", &[]), Err(UrlError::BlockedProtocol(_))));
    assert!(matches!(validate_url("gopher://example.com", &[]), Err(UrlError::BlockedProtocol(_))));
    assert!(matches!(validate_url("mailto:test@example.com", &[]), Err(UrlError::BlockedProtocol(_))));
}

#[test]
fn test_allowed_protocols() {
    assert!(validate_url("http://example.com", &[]).is_ok());
    assert!(validate_url("https://example.com", &[]).is_ok());
    assert!(validate_url("HTTP://EXAMPLE.COM", &[]).is_ok());
}

#[test]
fn test_blocked_localhost() {
    let blocked = vec!["127.0.0.0/8".to_string()];
    assert!(matches!(validate_url("http://127.0.0.1/admin", &blocked), Err(UrlError::BlockedHost(_))));
    assert!(matches!(validate_url("http://localhost/admin", &blocked), Err(UrlError::BlockedHost(_))));
}

#[test]
fn test_blocked_private_ranges() {
    let blocked = vec![
        "10.0.0.0/8".to_string(),
        "172.16.0.0/12".to_string(),
        "192.168.0.0/16".to_string(),
    ];
    assert!(validate_url("http://10.0.0.1/", &blocked).is_err());
    assert!(validate_url("http://172.16.0.1/", &blocked).is_err());
    assert!(validate_url("http://192.168.1.1/", &blocked).is_err());
    // Public IPs should be allowed
    assert!(validate_url("http://8.8.8.8/", &blocked).is_ok());
}
