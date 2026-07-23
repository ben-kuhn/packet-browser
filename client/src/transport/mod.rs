pub mod agwpe;
pub mod manager;
pub mod session;
pub mod vara;

pub use manager::TransportManager;

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

impl std::fmt::Display for VaraMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VaraMode::Fm => "fm",
            VaraMode::Hf => "hf",
        })
    }
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

impl std::fmt::Display for VaraBandwidth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VaraBandwidth::VNarrow => "vnarrow",
            VaraBandwidth::VWide => "vwide",
            VaraBandwidth::Bw250 => "bw250",
            VaraBandwidth::Bw500 => "bw500",
            VaraBandwidth::Bw2300 => "bw2300",
            VaraBandwidth::Bw2750 => "bw2750",
        })
    }
}

#[derive(Debug, Clone)]
pub struct TransportConfig {
    pub kind: TransportKind,
    pub agwpe: AgwpeParams,
    pub vara: VaraParams,
    /// Local station callsign, used for modem registration.  For AGWPE this
    /// is sent in the RegisterCallsign frame; for VARA it becomes the
    /// MYCALL command argument.
    pub local_callsign: String,
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

    /// Send a link-layer OPEN request (AX.25 `Connect` for AGWPE, a no-op for
    /// modems that establish a link implicitly when the modem is connected).
    /// After this call returns, the caller expects to drive the
    /// application-level handshake by reading data frames via `recv`.
    async fn open_ax25_link(&mut self) -> Result<(), TransportError> {
        Ok(())
    }

    /// Tear down the current modem connection and re-establish it using the
    /// same identity (callsign/host/port) that was used for the last
    /// `connect_modem` call. Used by the session-level reconnect flow to
    /// recover from a stalled modem without losing operator context.
    async fn reopen_modem_connection(&mut self) -> Result<(), TransportError> {
        Ok(())
    }

    /// Query the modem for its list of RF ports. AGWPE-style modems override
    /// this; VARA-style modems that expose exactly one channel leave the
    /// default no-op.
    async fn query_ports(&mut self) -> Result<(), TransportError> {
        Ok(())
    }
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

    #[tokio::test]
    async fn agwpe_transport_reports_disconnect_payload_as_disconnected_event() {
        use tokio::io::AsyncWriteExt;
        use super::agwpe::AgwpeTransport;

        let (mut tx_stream, mut transport) = AgwpeTransport::for_test_pair().await;

        tx_stream
            .write_all(&super::agwpe::test_helpers::disconnect_frame_bytes(
                b"*** DISCONNECTED FROM N0CALL-8\r\n",
            ))
            .await
            .unwrap();
        tx_stream.flush().await.unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        let event = transport.recv(deadline).await.unwrap();
        match event {
            TransportEvent::Disconnected { reason } => {
                assert!(reason.contains("DISCONNECTED"), "reason={reason}");
            }
            other => panic!("expected Disconnected, got {other:?}"),
        }
    }
}
