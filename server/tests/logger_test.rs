use packet_browser_server::logger::{LogEntry, LogStatus};

#[test]
fn test_log_entry_serialization() {
    let entry = LogEntry::new(
        "W1ABC".to_string(),
        "https://example.com".to_string(),
        LogStatus::Ok,
        None,
    );

    let json = entry.to_json();
    assert!(json.contains("\"call\":\"W1ABC\""));
    assert!(json.contains("\"url\":\"https://example.com\""));
    assert!(json.contains("\"status\":\"ok\""));
}

#[test]
fn test_log_entry_with_reason() {
    let entry = LogEntry::new(
        "W1ABC".to_string(),
        "https://blocked.com".to_string(),
        LogStatus::Blocked,
        Some("dns_filter".to_string()),
    );

    let json = entry.to_json();
    assert!(json.contains("\"status\":\"blocked\""));
    assert!(json.contains("\"reason\":\"dns_filter\""));
}
