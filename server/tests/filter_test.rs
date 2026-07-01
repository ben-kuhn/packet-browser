use packet_browser_server::filter::{resolve_and_pin, validate_url, UrlError};
use std::net::IpAddr;

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

#[test]
fn test_userinfo_does_not_smuggle_host() {
    // The host must come from URL parsing, not from naive string splitting:
    // `example.com@127.0.0.1` is the userinfo + host, the real host is 127.0.0.1.
    let blocked = vec!["127.0.0.0/8".to_string()];
    let result = validate_url("http://example.com@127.0.0.1/admin", &blocked);
    assert!(
        matches!(result, Err(UrlError::BlockedHost(_))),
        "userinfo bypass not closed: {:?}",
        result
    );
}

#[test]
fn test_ipv6_loopback_and_mapped_blocked() {
    let blocked = vec!["127.0.0.0/8".to_string()];
    assert!(validate_url("http://[::1]/", &blocked).is_err());
    // ::ffff:127.0.0.1 is the IPv4-mapped form of 127.0.0.1 and must inherit
    // the IPv4 block.
    assert!(validate_url("http://[::ffff:127.0.0.1]/", &blocked).is_err());
}

#[test]
fn test_ipv6_ula_and_link_local_blocked() {
    assert!(validate_url("http://[fc00::1]/", &[]).is_err());
    assert!(validate_url("http://[fd12:3456:789a::1]/", &[]).is_err());
    assert!(validate_url("http://[fe80::1]/", &[]).is_err());
    // Public IPv6 (Cloudflare 2606:4700::1) should remain allowed.
    assert!(validate_url("http://[2606:4700::1]/", &[]).is_ok());
}

#[test]
fn test_zero_address_blocked_by_default_range() {
    // `connect(0.0.0.0)` resolves to localhost on Linux, so 0.0.0.0/8 must be
    // covered or the blocklist's 0.0.0.0 sinkhole entries become an SSRF vector.
    let blocked = vec!["0.0.0.0/8".to_string()];
    assert!(validate_url("http://0.0.0.0/", &blocked).is_err());
}

#[test]
fn resolve_and_pin_returns_ip_for_literal() {
    let ip = resolve_and_pin("8.8.8.8", 443, &[]).unwrap();
    assert_eq!(ip, "8.8.8.8".parse::<IpAddr>().unwrap());
}

#[test]
fn resolve_and_pin_rejects_blocked_ip_literal() {
    let blocked = vec!["127.0.0.0/8".to_string()];
    assert!(resolve_and_pin("127.0.0.1", 80, &blocked).is_err());
}

#[test]
fn resolve_and_pin_rejects_localhost_by_name() {
    // The name-blocklist is orthogonal to blocked_ranges: "localhost" is
    // rejected without even doing DNS.
    assert!(resolve_and_pin("localhost", 80, &[]).is_err());
}

#[test]
fn resolve_and_pin_handles_bracketed_v6() {
    let ip = resolve_and_pin("[::1]", 80, &[]);
    // ::1 is loopback, universally blocked in resolve_and_pin's v6 check.
    assert!(matches!(ip, Err(UrlError::BlockedHost(_))));

    let ip = resolve_and_pin("[2606:4700::1]", 80, &[]).unwrap();
    assert_eq!(ip, "2606:4700::1".parse::<IpAddr>().unwrap());
}

#[test]
fn resolve_and_pin_rejects_ipv4_mapped_v6_loopback() {
    let blocked = vec!["127.0.0.0/8".to_string()];
    let result = resolve_and_pin("[::ffff:127.0.0.1]", 80, &blocked);
    assert!(matches!(result, Err(UrlError::BlockedHost(_))));
}
