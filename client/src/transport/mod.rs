pub mod agwpe;

use async_trait::async_trait;
use std::str::FromStr;
use std::time::Instant;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("transport not connected")]
    NotConnected,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("modem error: {0}")]
    ModemError(String),
    #[error("session rejected: {0}")]
    SessionRejected(String),
    #[error("timed out")]
    Timeout,
}

#[derive(Debug, Clone)]
pub enum TransportEvent {
    Data(Vec<u8>),
    Disconnected { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Ax25,
    VaraFm,
    VaraHf,
}

impl FromStr for TransportKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "ax25" => Ok(TransportKind::Ax25),
            "vara_fm" => Ok(TransportKind::VaraFm),
            "vara_hf" => Ok(TransportKind::VaraHf),
            other => Err(format!("unknown transport: {other}")),
        }
    }
}

impl std::fmt::Display for TransportKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            TransportKind::Ax25 => "ax25",
            TransportKind::VaraFm => "vara_fm",
            TransportKind::VaraHf => "vara_hf",
        })
    }
}

#[derive(Debug, Clone)]
pub struct AgwpeParams {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct VaraParams {
    pub cmd_host: String,
    pub cmd_port: u16,
    pub data_host: String,
    pub data_port: u16,
    pub mode: VaraMode,
    pub bandwidth: VaraBandwidth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaraMode {
    Fm,
    Hf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaraBandwidth {
    VNarrow,
    VWide,
    Bw250,
    Bw500,
    Bw2300,
    Bw2750,
}

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub kind: TransportKind,
    pub agwpe: AgwpeParams,
    pub vara: VaraParams,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub local_callsign: String,
    pub remote_callsign: String,
    pub bpq_command: String,
    pub skip_bpq_app: bool,
    pub agwpe_port: u8,
}

#[async_trait]
pub trait Transport: Send {
    async fn connect_modem(
        &mut self,
        cfg: &TransportConfig,
    ) -> Result<(), TransportError>;

    async fn disconnect_modem(&mut self) -> Result<(), TransportError>;

    async fn open_session(
        &mut self,
        cfg: &SessionConfig,
    ) -> Result<(), TransportError>;

    async fn close_session(&mut self) -> Result<(), TransportError>;

    async fn send(&mut self, data: &[u8]) -> Result<(), TransportError>;

    async fn recv(
        &mut self,
        deadline: Instant,
    ) -> Result<TransportEvent, TransportError>;

    fn port_query_supported(&self) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_kind_roundtrips_config_strings() {
        assert_eq!("ax25".parse::<TransportKind>().unwrap(), TransportKind::Ax25);
        assert_eq!("vara_fm".parse::<TransportKind>().unwrap(), TransportKind::VaraFm);
        assert_eq!("vara_hf".parse::<TransportKind>().unwrap(), TransportKind::VaraHf);
        assert_eq!(TransportKind::Ax25.to_string(), "ax25");
        assert_eq!(TransportKind::VaraFm.to_string(), "vara_fm");
        assert_eq!(TransportKind::VaraHf.to_string(), "vara_hf");
    }

    #[test]
    fn transport_kind_rejects_unknown() {
        assert!("carrier-pigeon".parse::<TransportKind>().is_err());
    }
}
