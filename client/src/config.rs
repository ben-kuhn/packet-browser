use configparser::ini::Ini;
use std::fs;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Parse error: {0}")]
    Parse(String),
    #[error("Config directory not found")]
    NoConfigDir,
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig {
    pub response_timeout_secs: u64,
    pub auto_reconnect: bool,
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            response_timeout_secs: 30,
            auto_reconnect: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CacheSection {
    pub enabled: bool,
    pub max_bytes: u64,
    pub max_ttl_seconds: u64,
    pub dir: Option<PathBuf>,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 209_715_200, // 200 MiB
            max_ttl_seconds: 86_400,
            dir: None,
        }
    }
}

impl CacheSection {
    pub fn effective_dir(&self) -> Result<PathBuf, ConfigError> {
        if let Some(d) = &self.dir {
            return Ok(d.clone());
        }
        let cache_root = dirs::cache_dir().ok_or(ConfigError::NoConfigDir)?;
        Ok(cache_root.join("packet-browser"))
    }
}

#[derive(Debug, Clone)]
pub struct FileConfig {
    pub agwpe_host: String,
    pub agwpe_port: u16,
    pub my_callsign: String,
    pub target_callsign: String,
    pub bpq_command: String,
    pub skip_bpq_app: bool,
    pub cache: CacheSection,
    pub connection: ConnectionConfig,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            agwpe_host: "127.0.0.1".to_string(),
            agwpe_port: 8000,
            my_callsign: String::new(),
            target_callsign: String::new(),
            bpq_command: "WEB".to_string(),
            skip_bpq_app: false,
            cache: CacheSection::default(),
            connection: ConnectionConfig::default(),
        }
    }
}

impl FileConfig {
    pub fn default_path() -> Result<PathBuf, ConfigError> {
        let config_dir = dirs::config_dir().ok_or(ConfigError::NoConfigDir)?;
        Ok(config_dir.join("packet-browser").join("config.ini"))
    }

    pub fn load(path: &PathBuf) -> Result<Self, ConfigError> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let mut ini = Ini::new();
        ini.load(path).map_err(|e| ConfigError::Parse(e))?;

        let agwpe_host = ini
            .get("server", "agwpe_host")
            .unwrap_or_else(|| "127.0.0.1".to_string());

        let agwpe_port = ini
            .get("server", "agwpe_port")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8000);

        let my_callsign = ini
            .get("session", "my_callsign")
            .unwrap_or_default();

        let target_callsign = ini
            .get("session", "target_callsign")
            .unwrap_or_default();

        let bpq_command = ini
            .get("session", "bpq_command")
            .unwrap_or_else(|| "WEB".to_string());

        let skip_bpq_app = ini
            .get("session", "skip_bpq_app")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        let cache_enabled = ini
            .get("cache", "enabled")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes" | "on"))
            .unwrap_or(true);
        let cache_max_bytes = ini
            .get("cache", "max_bytes")
            .and_then(|v| v.parse().ok())
            .unwrap_or(209_715_200);
        let cache_max_ttl_seconds = ini
            .get("cache", "max_ttl_seconds")
            .and_then(|v| v.parse().ok())
            .unwrap_or(86_400);
        let cache_dir = ini
            .get("cache", "dir")
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from);

        let response_timeout_secs = ini
            .get("connection", "response_timeout_secs")
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        let auto_reconnect = ini
            .get("connection", "auto_reconnect")
            .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes" | "on"))
            .unwrap_or(true);

        Ok(Self {
            agwpe_host,
            agwpe_port,
            my_callsign,
            target_callsign,
            bpq_command,
            skip_bpq_app,
            cache: CacheSection {
                enabled: cache_enabled,
                max_bytes: cache_max_bytes,
                max_ttl_seconds: cache_max_ttl_seconds,
                dir: cache_dir,
            },
            connection: ConnectionConfig {
                response_timeout_secs,
                auto_reconnect,
            },
        })
    }

    pub fn save(&self, path: &PathBuf) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut ini = Ini::new();

        ini.set("server", "agwpe_host", Some(self.agwpe_host.clone()));
        ini.set("server", "agwpe_port", Some(self.agwpe_port.to_string()));
        ini.set("session", "my_callsign", Some(self.my_callsign.clone()));
        ini.set("session", "target_callsign", Some(self.target_callsign.clone()));
        ini.set("session", "bpq_command", Some(self.bpq_command.clone()));
        ini.set("session", "skip_bpq_app", Some(self.skip_bpq_app.to_string()));

        ini.set("cache", "enabled", Some(self.cache.enabled.to_string()));
        ini.set("cache", "max_bytes", Some(self.cache.max_bytes.to_string()));
        ini.set("cache", "max_ttl_seconds", Some(self.cache.max_ttl_seconds.to_string()));
        if let Some(d) = &self.cache.dir {
            ini.set("cache", "dir", Some(d.to_string_lossy().into_owned()));
        }

        ini.set("connection", "response_timeout_secs", Some(self.connection.response_timeout_secs.to_string()));
        ini.set("connection", "auto_reconnect", Some(self.connection.auto_reconnect.to_string()));

        ini.write(path).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Ok(())
    }

    pub fn update_target(&mut self, target: &str) {
        self.target_callsign = target.to_string();
    }
}

#[derive(Debug, Clone)]
pub struct CliArgs {
    pub config_path: Option<PathBuf>,
    pub agwpe_host: Option<String>,
    pub agwpe_port: Option<u16>,
    pub listen_addr: String,
    pub bpq_command: Option<String>,
    pub verbosity: u8,
    pub allowed_hosts: Vec<String>,
}

impl CliArgs {
    pub fn parse() -> Self {
        use clap::Parser;

        #[derive(Parser)]
        #[command(name = "packet-browser-client")]
        #[command(about = "Packet radio web browser client")]
        struct Args {
            #[arg(short, long, help = "Configuration file (INI format)")]
            config: Option<PathBuf>,

            #[arg(long, help = "AGWPE host (default: 127.0.0.1)")]
            agwpe_host: Option<String>,

            #[arg(long, help = "AGWPE port (default: 8000)")]
            agwpe_port: Option<u16>,

            #[arg(long, default_value = "127.0.0.1:8080", help = "Web proxy listen address")]
            listen_addr: String,

            #[arg(long, default_value = "WEB", help = "BPQ APPLICATION command")]
            bpq_command: String,

            #[arg(short, long, action = clap::ArgAction::Count, help = "Verbosity level (-v, -vv, -vvv)")]
            verbose: u8,

            #[arg(
                long,
                value_delimiter = ',',
                help = "Extra hostnames to accept in the Host header (comma-separated). Useful for mDNS names like 'raspberrypi.local' when binding to a LAN interface. Loopback and LAN IP literals are already accepted based on --listen-addr."
            )]
            allowed_hosts: Vec<String>,
        }

        let args = Args::parse();

        Self {
            config_path: args.config,
            agwpe_host: args.agwpe_host,
            agwpe_port: args.agwpe_port,
            listen_addr: args.listen_addr,
            bpq_command: Some(args.bpq_command),
            verbosity: args.verbose,
            allowed_hosts: args
                .allowed_hosts
                .into_iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    pub fn resolve_config(&self) -> Result<FileConfig, ConfigError> {
        let path = match self.config_path.clone() {
            Some(p) => p,
            None => FileConfig::default_path()?,
        };

        let mut config = FileConfig::load(&path)?;

        if let Some(host) = &self.agwpe_host {
            config.agwpe_host = host.clone();
        }

        if let Some(port) = self.agwpe_port {
            config.agwpe_port = port;
        }

        if let Some(cmd) = &self.bpq_command {
            config.bpq_command = cmd.clone();
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_config_default() {
        let config = FileConfig::default();
        assert_eq!(config.agwpe_host, "127.0.0.1");
        assert_eq!(config.agwpe_port, 8000);
        assert_eq!(config.my_callsign, "");
        assert_eq!(config.target_callsign, "");
        assert_eq!(config.bpq_command, "WEB");
    }

    #[test]
    fn test_config_save_load_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.ini");

        let config = FileConfig {
            agwpe_host: "192.168.1.100".to_string(),
            agwpe_port: 9000,
            my_callsign: "N0CALL".to_string(),
            target_callsign: "NODE1".to_string(),
            bpq_command: "BROWSE".to_string(),
            skip_bpq_app: false,
            cache: CacheSection::default(),
            connection: ConnectionConfig::default(),
        };

        config.save(&path).unwrap();
        let loaded = FileConfig::load(&path).unwrap();

        assert_eq!(loaded.agwpe_host, "192.168.1.100");
        assert_eq!(loaded.agwpe_port, 9000);
        assert_eq!(loaded.my_callsign, "N0CALL");
        assert_eq!(loaded.target_callsign, "NODE1");
        assert_eq!(loaded.bpq_command, "BROWSE");
    }

    #[test]
    fn test_config_load_missing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.ini");

        let config = FileConfig::load(&path).unwrap();
        assert_eq!(config.agwpe_host, "127.0.0.1");
        assert_eq!(config.agwpe_port, 8000);
    }

    #[test]
    fn test_config_update_target() {
        let mut config = FileConfig::default();
        assert_eq!(config.target_callsign, "");

        config.update_target("NEWNODE");
        assert_eq!(config.target_callsign, "NEWNODE");
    }

    #[test]
    fn test_cli_args_override() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.ini");

        let mut config = FileConfig::default();
        config.agwpe_host = "10.0.0.1".to_string();
        config.agwpe_port = 7000;
        config.save(&path).unwrap();

        let cli = CliArgs {
            config_path: Some(path.clone()),
            agwpe_host: Some("192.168.1.1".to_string()),
            agwpe_port: None,
            listen_addr: "127.0.0.1:8080".to_string(),
            bpq_command: Some("WEB".to_string()),
            verbosity: 0,
            allowed_hosts: vec![],
        };

        let resolved = cli.resolve_config().unwrap();
        assert_eq!(resolved.agwpe_host, "192.168.1.1");
        assert_eq!(resolved.agwpe_port, 7000);
    }

    #[test]
    fn cache_defaults_are_applied_when_section_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.ini");
        let cfg = FileConfig::default();
        cfg.save(&path).unwrap();
        let loaded = FileConfig::load(&path).unwrap();
        assert!(loaded.cache.enabled);
        assert_eq!(loaded.cache.max_bytes, 209_715_200);
        assert_eq!(loaded.cache.max_ttl_seconds, 86_400);
        assert!(loaded.cache.dir.is_none());
    }

    #[test]
    fn cache_section_roundtrip() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.ini");
        let cfg = FileConfig {
            cache: CacheSection {
                enabled: false,
                max_bytes: 42,
                max_ttl_seconds: 7,
                dir: Some(std::path::PathBuf::from("/tmp/pb-cache")),
            },
            ..FileConfig::default()
        };
        cfg.save(&path).unwrap();
        let loaded = FileConfig::load(&path).unwrap();
        assert!(!loaded.cache.enabled);
        assert_eq!(loaded.cache.max_bytes, 42);
        assert_eq!(loaded.cache.max_ttl_seconds, 7);
        assert_eq!(
            loaded.cache.dir.as_deref().map(|p| p.to_string_lossy().into_owned()),
            Some("/tmp/pb-cache".to_string())
        );
    }

    #[test]
    fn test_connection_config_defaults() {
        let cfg = FileConfig::default();
        assert_eq!(cfg.connection.response_timeout_secs, 30);
        assert!(cfg.connection.auto_reconnect);
    }

    #[test]
    fn test_connection_config_overrides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.ini");

        let mut ini = Ini::new();
        ini.set("server", "agwpe_host", Some("127.0.0.1".to_string()));
        ini.set("server", "agwpe_port", Some("8000".to_string()));
        ini.set("session", "my_callsign", Some("W1TEST".to_string()));
        ini.set("session", "target_callsign", Some("N0CALL".to_string()));
        ini.set("session", "bpq_command", Some("WEB".to_string()));
        ini.set("session", "skip_bpq_app", Some("false".to_string()));
        ini.set("connection", "response_timeout_secs", Some("15".to_string()));
        ini.set("connection", "auto_reconnect", Some("false".to_string()));
        ini.write(&path).unwrap();

        let loaded = FileConfig::load(&path).unwrap();
        assert_eq!(loaded.connection.response_timeout_secs, 15);
        assert!(!loaded.connection.auto_reconnect);
    }
}
