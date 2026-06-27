use packet_browser_server::config::Config;

#[test]
fn test_config_defaults() {
    let env_vars = vec![
        "LISTEN_PORT",
        "PORTAL_URL",
        "IDLE_TIMEOUT_MINUTES",
        "BROTLI_QUALITY",
        "BLOCKED_RANGES",
        "BLOCKLIST_URLS",
        "BLOCKLIST_REFRESH_HOURS",
        "BLOCKLIST_ENABLED",
        "LOG_ROTATE_ENABLED",
        "LOG_RETAIN_DAYS",
        "SYSLOG_ENABLED",
        "SYSLOG_HOST",
        "SYSLOG_PORT",
    ];

    for var in &env_vars {
        std::env::remove_var(var);
    }

    let config = Config::from_env();

    assert_eq!(config.listen_port, 63004);
    assert_eq!(config.portal_url, "https://www.zeroretries.radio");
    assert_eq!(config.idle_timeout_minutes, 10);
    assert_eq!(config.brotli_quality, 11);
    assert_eq!(
        config.blocked_ranges,
        vec![
            "127.0.0.0/8",
            "10.0.0.0/8",
            "172.16.0.0/12",
            "192.168.0.0/16",
            "169.254.0.0/16"
        ]
    );
    assert!(config.blocklist_urls.is_empty());
    assert_eq!(config.blocklist_refresh_hours, 24);
    assert!(config.blocklist_enabled);
    assert!(config.log_rotate_enabled);
    assert_eq!(config.log_retain_days, 30);
    assert!(!config.syslog_enabled);
    assert_eq!(config.syslog_host, None);
    assert_eq!(config.syslog_port, 514);
}

#[test]
fn test_config_env_override() {
    let env_vars = vec![
        "LISTEN_PORT",
        "PORTAL_URL",
        "IDLE_TIMEOUT_MINUTES",
        "BROTLI_QUALITY",
        "BLOCKED_RANGES",
        "BLOCKLIST_URLS",
        "BLOCKLIST_REFRESH_HOURS",
        "BLOCKLIST_ENABLED",
        "LOG_ROTATE_ENABLED",
        "LOG_RETAIN_DAYS",
        "SYSLOG_ENABLED",
        "SYSLOG_HOST",
        "SYSLOG_PORT",
    ];

    for var in &env_vars {
        std::env::remove_var(var);
    }

    std::env::set_var("LISTEN_PORT", "8080");
    std::env::set_var("PORTAL_URL", "http://custom.example.com/");
    std::env::set_var("IDLE_TIMEOUT_MINUTES", "30");
    std::env::set_var("BROTLI_QUALITY", "6");
    std::env::set_var("BLOCKED_RANGES", "192.168.1.0/24, 10.0.0.0/8");
    std::env::set_var("BLOCKLIST_URLS", "http://example.com/list1, http://example.com/list2");
    std::env::set_var("BLOCKLIST_REFRESH_HOURS", "48");
    std::env::set_var("BLOCKLIST_ENABLED", "false");
    std::env::set_var("LOG_ROTATE_ENABLED", "false");
    std::env::set_var("LOG_RETAIN_DAYS", "60");
    std::env::set_var("SYSLOG_ENABLED", "true");
    std::env::set_var("SYSLOG_HOST", "localhost");
    std::env::set_var("SYSLOG_PORT", "1514");

    let config = Config::from_env();

    assert_eq!(config.listen_port, 8080);
    assert_eq!(config.portal_url, "http://custom.example.com/");
    assert_eq!(config.idle_timeout_minutes, 30);
    assert_eq!(config.brotli_quality, 6);
    assert_eq!(
        config.blocked_ranges,
        vec!["192.168.1.0/24", "10.0.0.0/8"]
    );
    assert_eq!(
        config.blocklist_urls,
        vec!["http://example.com/list1", "http://example.com/list2"]
    );
    assert_eq!(config.blocklist_refresh_hours, 48);
    assert!(!config.blocklist_enabled);
    assert!(!config.log_rotate_enabled);
    assert_eq!(config.log_retain_days, 60);
    assert!(config.syslog_enabled);
    assert_eq!(config.syslog_host, Some("localhost".to_string()));
    assert_eq!(config.syslog_port, 1514);

    for var in &env_vars {
        std::env::remove_var(var);
    }
}

#[test]
fn test_config_bool_parsing() {
    std::env::remove_var("BLOCKLIST_ENABLED");

    std::env::set_var("BLOCKLIST_ENABLED", "true");
    let config = Config::from_env();
    assert!(config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "1");
    let config = Config::from_env();
    assert!(config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "yes");
    let config = Config::from_env();
    assert!(config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "on");
    let config = Config::from_env();
    assert!(config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "false");
    let config = Config::from_env();
    assert!(!config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "0");
    let config = Config::from_env();
    assert!(!config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "no");
    let config = Config::from_env();
    assert!(!config.blocklist_enabled);

    std::env::set_var("BLOCKLIST_ENABLED", "off");
    let config = Config::from_env();
    assert!(!config.blocklist_enabled);

    std::env::remove_var("BLOCKLIST_ENABLED");
}

#[test]
fn test_config_invalid_env_values_use_defaults() {
    std::env::remove_var("LISTEN_PORT");
    std::env::remove_var("IDLE_TIMEOUT_MINUTES");
    std::env::remove_var("SYSLOG_PORT");
    std::env::remove_var("BROTLI_QUALITY");

    std::env::set_var("LISTEN_PORT", "not_a_number");
    std::env::set_var("IDLE_TIMEOUT_MINUTES", "invalid");
    std::env::set_var("SYSLOG_PORT", "not_a_port");
    std::env::set_var("BROTLI_QUALITY", "not_a_number");

    let config = Config::from_env();

    assert_eq!(config.listen_port, 63004);
    assert_eq!(config.idle_timeout_minutes, 10);
    assert_eq!(config.syslog_port, 514);
    assert_eq!(config.brotli_quality, 11);

    std::env::remove_var("LISTEN_PORT");
    std::env::remove_var("IDLE_TIMEOUT_MINUTES");
    std::env::remove_var("SYSLOG_PORT");
    std::env::remove_var("BROTLI_QUALITY");
}
