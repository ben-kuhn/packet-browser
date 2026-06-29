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
pub struct FileConfig {
    pub agwpe_host: String,
    pub agwpe_port: u16,
    pub my_callsign: String,
    pub target_callsign: String,
    pub bpq_command: String,
    pub skip_bpq_app: bool,
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

        Ok(Self {
            agwpe_host,
            agwpe_port,
            my_callsign,
            target_callsign,
            bpq_command,
            skip_bpq_app,
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
        }

        let args = Args::parse();

        Self {
            config_path: args.config,
            agwpe_host: args.agwpe_host,
            agwpe_port: args.agwpe_port,
            listen_addr: args.listen_addr,
            bpq_command: Some(args.bpq_command),
            verbosity: args.verbose,
        }
    }

    pub fn resolve_config(&self) -> Result<FileConfig, ConfigError> {
        let path = self.config_path.clone()
            .unwrap_or_else(|| FileConfig::default_path().unwrap());

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
        };

        let resolved = cli.resolve_config().unwrap();
        assert_eq!(resolved.agwpe_host, "192.168.1.1");
        assert_eq!(resolved.agwpe_port, 7000);
    }
}
