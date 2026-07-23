use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;

use crate::state::{
    ConnectionState, DebugLogEntry, Direction, LockExt, LogLevel, PortInfo, SharedState,
};

#[derive(Error, Debug)]
pub enum AgwpeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),
    #[error("Registration failed: {0}")]
    RegistrationFailed(String),
    #[error("Not connected")]
    NotConnected,
    #[error("Invalid frame: {0}")]
    InvalidFrame(String),
    #[error("Timeout")]
    Timeout,
    #[error("Background task stopped")]
    TaskStopped,
    #[error("Session died: {reason}")]
    SessionDied { reason: String },
    #[error("Session dropped and requires re-consent")]
    NeedsReconsent,
    #[error("Disconnected by operator")]
    DisconnectedByOperator,
}

// Defensive caps against a hostile or buggy AGWPE peer sending oversized lengths.
pub(crate) const MAX_FRAME_DATA_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    RegisterCallsign = 0x58,
    RegistrationResponse = 0x78,
    Connect = 0x43,
    Connected = 0x63,
    // AGWPE-P 'd' 0x64. Bidirectional:
    //   client→server: "please disconnect from the remote station"
    //   server→client: "we have been disconnected", payload starts with `*** DISCONNECTED FROM ...`
    // The name matches the AGWPE-P spec; the payload check at
    // `is_session_dead_payload` distinguishes disconnect-notification bytes
    // from any other data that lands here.
    Disconnect = 0x64,
    SendData = 0x44,
    ConnectionRejected = 0x52,
    QueryPorts = 0x47,
    PortInfo = 0x67,
}

impl TryFrom<u8> for FrameType {
    type Error = AgwpeError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x58 => Ok(FrameType::RegisterCallsign),
            0x78 => Ok(FrameType::RegistrationResponse),
            0x43 => Ok(FrameType::Connect),
            0x63 => Ok(FrameType::Connected),
            0x64 => Ok(FrameType::Disconnect),
            0x44 => Ok(FrameType::SendData),
            0x52 => Ok(FrameType::ConnectionRejected),
            0x47 => Ok(FrameType::QueryPorts),
            0x67 => Ok(FrameType::PortInfo),
            _ => Err(AgwpeError::InvalidFrame(format!("Unknown frame type: 0x{:02X}", value))),
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgwpeFrame {
    pub port: u8,
    pub frame_type: FrameType,
    pub pid: u8,
    pub call_from: String,
    pub call_to: String,
    pub data_len: u32,
    pub user_data: u32,
    pub data: Vec<u8>,
}

impl AgwpeFrame {
    pub const HEADER_SIZE: usize = 36;

    pub fn new(
        port: u8,
        frame_type: FrameType,
        call_from: &str,
        call_to: &str,
        data: Vec<u8>,
    ) -> Self {
        Self {
            port,
            frame_type,
            pid: 0x00,
            call_from: call_from.to_string(),
            call_to: call_to.to_string(),
            data_len: data.len() as u32,
            user_data: 0,
            data,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut frame = Vec::with_capacity(Self::HEADER_SIZE + self.data.len());

        // offset 0: port
        frame.push(self.port);
        // offset 1-3: reserved
        frame.extend_from_slice(&[0u8; 3]);
        // offset 4: datakind/frame_type
        frame.push(self.frame_type as u8);
        // offset 5-7: reserved
        frame.extend_from_slice(&[0u8; 3]);

        // offset 8-17: call_from (10 bytes)
        let mut call_from_bytes = [0u8; 10];
        let call_from_str = self.call_from.as_bytes();
        let len = call_from_str.len().min(9);
        call_from_bytes[..len].copy_from_slice(&call_from_str[..len]);
        frame.extend_from_slice(&call_from_bytes);

        // offset 18-27: call_to (10 bytes)
        let mut call_to_bytes = [0u8; 10];
        let call_to_str = self.call_to.as_bytes();
        let len = call_to_str.len().min(9);
        call_to_bytes[..len].copy_from_slice(&call_to_str[..len]);
        frame.extend_from_slice(&call_to_bytes);

        // offset 28-31: data_len (4 bytes, little-endian)
        frame.extend_from_slice(&self.data_len.to_le_bytes());
        // offset 32-35: user_data (4 bytes, little-endian)
        frame.extend_from_slice(&self.user_data.to_le_bytes());

        frame.extend_from_slice(&self.data);

        frame
    }

    pub fn decode(data: &[u8]) -> Result<Self, AgwpeError> {
        if data.len() < Self::HEADER_SIZE {
            return Err(AgwpeError::InvalidFrame("Frame too short".to_string()));
        }

        // offset 0: port
        let port = data[0];
        // offset 1-3: reserved
        // offset 4: datakind/frame_type
        let frame_type = FrameType::try_from(data[4])?;
        // offset 5-7: reserved

        // offset 8-17: call_from (10 bytes)
        let call_from = Self::extract_callsign(&data[8..18])?;
        // offset 18-27: call_to (10 bytes)
        let call_to = Self::extract_callsign(&data[18..28])?;

        // offset 28-31: data_len (4 bytes, little-endian)
        let data_len = u32::from_le_bytes([data[28], data[29], data[30], data[31]]);
        // offset 32-35: user_data (4 bytes, little-endian)
        let user_data = u32::from_le_bytes([data[32], data[33], data[34], data[35]]);

        if data_len as usize > MAX_FRAME_DATA_SIZE {
            return Err(AgwpeError::InvalidFrame(format!(
                "Frame data_len {} exceeds maximum {}",
                data_len, MAX_FRAME_DATA_SIZE
            )));
        }

        if data.len() < Self::HEADER_SIZE + data_len as usize {
            return Err(AgwpeError::InvalidFrame("Frame data truncated".to_string()));
        }

        let payload = data[Self::HEADER_SIZE..Self::HEADER_SIZE + data_len as usize].to_vec();

        Ok(Self {
            port,
            frame_type,
            pid: 0,
            call_from,
            call_to,
            data_len,
            user_data,
            data: payload,
        })
    }

    fn extract_callsign(bytes: &[u8]) -> Result<String, AgwpeError> {
        let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8(bytes[..len].to_vec())
            .map_err(|_| AgwpeError::InvalidFrame("Invalid callsign encoding".to_string()))
    }
}

/// Detects control-plane text that signals the AX.25 session is dead.
///
/// Different node stacks emit different tear-down text:
///   Direwolf / AGWPE convention: `*** DISCONNECTED FROM Station <call>`
///   LinBPQ WEB-app exit:         `Returned to Node <alias>:<call>`
///
/// The markers may arrive in the first frame OR appear mid-buffer after a
/// prior in-flight response has already accumulated bytes, so we scan the
/// whole accumulated buffer, not just its prefix. False positives inside a
/// legitimate RESP body are impossible because the payload is base64
/// (`[A-Za-z0-9+/=]`) and can't contain spaces or asterisks.
pub(crate) fn is_session_dead_payload(data: &[u8]) -> bool {
    contains_slice(data, b"*** DISCONNECTED") || contains_slice(data, b"Returned to Node")
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// AgwpeTransport – owns the TCP stream + read buffer and implements the
// Transport trait against the AGWPE-P wire protocol.
// ---------------------------------------------------------------------------

pub struct AgwpeTransport {
    pub(crate) stream: Option<TcpStream>,
    pub(crate) read_buf: Vec<u8>,
    pub local_callsign: String,
    pub remote_callsign: String,
    pub agwpe_port: u8,
    // SharedState + log channel are held so trait methods can log/state-mutate
    // without threading them through every call. Set via `attach_state`.
    pub(crate) state: Option<SharedState>,
    pub(crate) log_tx: Option<broadcast::Sender<DebugLogEntry>>,
}

impl AgwpeTransport {
    pub fn new() -> Self {
        Self {
            stream: None,
            read_buf: Vec::new(),
            local_callsign: String::new(),
            remote_callsign: String::new(),
            agwpe_port: 0,
            state: None,
            log_tx: None,
        }
    }

    /// Attach the shared state + log broadcaster. Called once by the
    /// TransportManager background task before driving any commands.
    pub fn attach_state(
        &mut self,
        state: SharedState,
        log_tx: broadcast::Sender<DebugLogEntry>,
    ) {
        self.state = Some(state);
        self.log_tx = Some(log_tx);
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn push_log(&self, entry: DebugLogEntry) {
        if let Some(state) = &self.state {
            let mut s = state.lock_or_poisoned();
            s.add_log(entry.clone());
        }
        if let Some(tx) = &self.log_tx {
            let _ = tx.send(entry);
        }
    }

    fn set_state(&self, cs: ConnectionState) {
        let entry = if let Some(state) = &self.state {
            let mut s = state.lock_or_poisoned();
            Some(s.set_connection_state(cs))
        } else {
            None
        };
        if let (Some(tx), Some(e)) = (&self.log_tx, entry) {
            let _ = tx.send(e);
        }
    }

    pub(crate) async fn send_frame_internal(
        &mut self,
        frame: &AgwpeFrame,
    ) -> Result<(), AgwpeError> {
        let stream = self.stream.as_mut().ok_or(AgwpeError::NotConnected)?;
        stream.write_all(&frame.encode()).await?;
        stream.flush().await?;
        Ok(())
    }

    pub(crate) async fn read_frame_from_stream(&mut self) -> Result<AgwpeFrame, AgwpeError> {
        loop {
            if self.read_buf.len() >= AgwpeFrame::HEADER_SIZE {
                let data_len = u32::from_le_bytes([
                    self.read_buf[28],
                    self.read_buf[29],
                    self.read_buf[30],
                    self.read_buf[31],
                ]) as usize;
                if data_len > MAX_FRAME_DATA_SIZE {
                    return Err(AgwpeError::InvalidFrame(format!(
                        "Peer announced frame data_len {} exceeds maximum {}",
                        data_len, MAX_FRAME_DATA_SIZE
                    )));
                }
                let total = AgwpeFrame::HEADER_SIZE + data_len;
                if self.read_buf.len() >= total {
                    tracing::trace!("[AGWPE] Raw frame bytes (first 36): {:?}", &self.read_buf[..36.min(self.read_buf.len())]);
                    tracing::trace!("[AGWPE] Frame type byte at offset 4: 0x{:02X} ('{}')",
                        self.read_buf[4],
                        if self.read_buf[4] >= 32 && self.read_buf[4] < 127 { self.read_buf[4] as char } else { '?' });
                    if data_len > 0 {
                        tracing::trace!("[AGWPE] Frame data ({} bytes): {:?}", data_len, &self.read_buf[36..36+data_len.min(self.read_buf.len()-36)]);
                    }
                    let frame = AgwpeFrame::decode(&self.read_buf[..total])?;
                    self.read_buf.drain(..total);
                    return Ok(frame);
                }
            }
            let mut tmp = [0u8; 4096];
            let stream = self.stream.as_mut().ok_or(AgwpeError::NotConnected)?;
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(AgwpeError::ConnectionFailed("Connection closed".to_string()));
            }
            tracing::trace!("[AGWPE] Read {} bytes from stream", n);
            self.read_buf.extend_from_slice(&tmp[..n]);
        }
    }

    pub(crate) async fn read_frame_with_timeout_deadline(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<AgwpeFrame, AgwpeError> {
        let now = std::time::Instant::now();
        if now >= deadline {
            return Err(AgwpeError::Timeout);
        }
        let duration = deadline - now;
        match tokio::time::timeout(duration, self.read_frame_from_stream()).await {
            Ok(result) => result,
            Err(_) => Err(AgwpeError::Timeout),
        }
    }

    async fn read_frame_with_timeout(&mut self, timeout_secs: u64) -> Result<AgwpeFrame, AgwpeError> {
        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            self.read_frame_from_stream(),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(AgwpeError::Timeout),
        }
    }

    /// Perform the AGWPE TCP connect + callsign registration.  On success the
    /// modem is in `AgwpeConnected`; on failure `stream` is cleared.
    pub(crate) async fn connect_modem_internal(
        &mut self,
        host: &str,
        port: u16,
        callsign: &str,
    ) -> Result<(), AgwpeError> {
        self.push_log(DebugLogEntry::new(
            LogLevel::Info,
            "AGWPE",
            &format!("Connecting to AGWPE at {}:{}", host, port),
        ));

        let stream = match TcpStream::connect(format!("{}:{}", host, port)).await {
            Ok(s) => {
                self.push_log(DebugLogEntry::new(
                    LogLevel::Info,
                    "AGWPE",
                    "TCP connection established",
                ));
                s
            }
            Err(e) => {
                let msg = format!("TCP connection failed: {}", e);
                self.push_log(DebugLogEntry::new(LogLevel::Info, "ERROR", &msg));
                self.set_state(ConnectionState::Error(msg.clone()));
                return Err(AgwpeError::ConnectionFailed(msg));
            }
        };

        self.stream = Some(stream);
        self.local_callsign = callsign.to_string();
        self.read_buf.clear();

        let reg_frame = AgwpeFrame::new(
            0,
            FrameType::RegisterCallsign,
            callsign,
            "",
            vec![],
        );

        self.push_log(
            DebugLogEntry::new(
                LogLevel::Debug,
                "AGWPE",
                &format!("Sending registration for {}", callsign),
            )
            .with_direction(Direction::Tx),
        );

        if let Err(e) = self.stream.as_mut().unwrap().write_all(&reg_frame.encode()).await {
            self.stream = None;
            let msg = format!("Registration send failed: {}", e);
            self.set_state(ConnectionState::Error(msg.clone()));
            return Err(AgwpeError::RegistrationFailed(msg));
        }
        let _ = self.stream.as_mut().unwrap().flush().await;

        match self.read_frame_with_timeout(5).await {
            Ok(frame) if frame.frame_type == FrameType::RegisterCallsign && frame.data == vec![0x01] => {
                self.push_log(
                    DebugLogEntry::new(LogLevel::Debug, "AGWPE", "Registration successful")
                        .with_direction(Direction::Rx),
                );
                self.set_state(ConnectionState::AgwpeConnected);
                Ok(())
            }
            Ok(frame) => {
                self.stream = None;
                let msg = format!(
                    "Unexpected response to registration: {:?} (port={}, call_from={}, call_to={}, data={:?})",
                    frame.frame_type, frame.port, frame.call_from, frame.call_to, frame.data
                );
                self.push_log(DebugLogEntry::new(LogLevel::Debug, "AGWPE", &msg));
                self.set_state(ConnectionState::Error(msg.clone()));
                Err(AgwpeError::RegistrationFailed(msg))
            }
            Err(e) => {
                self.stream = None;
                let msg = format!("Registration timeout: {}", e);
                self.set_state(ConnectionState::Error(msg.clone()));
                Err(AgwpeError::RegistrationFailed(msg))
            }
        }
    }

    /// Send the AGWPE-P Query-Ports frame and populate SharedState with the
    /// returned port list.  Requires the modem to be connected.
    pub(crate) async fn query_ports_internal(&mut self) -> Result<(), AgwpeError> {
        if !self.is_connected() {
            return Err(AgwpeError::NotConnected);
        }

        let query_frame = AgwpeFrame::new(0, FrameType::QueryPorts, &self.local_callsign, "", vec![]);

        self.push_log(
            DebugLogEntry::new(LogLevel::Debug, "AGWPE", "Querying ports")
                .with_direction(Direction::Tx),
        );

        self.send_frame_internal(&query_frame).await?;

        let mut ports = Vec::new();

        loop {
            let frame = self.read_frame_with_timeout(5).await?;

            self.push_log(
                DebugLogEntry::new(
                    LogLevel::Debug,
                    "AGWPE",
                    &format!(
                        "Port query response: frame_type={:?}, data_len={}, data={:?}",
                        frame.frame_type, frame.data_len, frame.data
                    ),
                )
                .with_direction(Direction::Rx),
            );

            if frame.frame_type == FrameType::QueryPorts || frame.frame_type == FrameType::PortInfo {
                if frame.data_len == 0 {
                    break;
                }
                if !frame.data.is_empty() {
                    let data_str = String::from_utf8_lossy(&frame.data);
                    let data_str = data_str.trim_end_matches('\0');
                    let parts: Vec<&str> = data_str.split(';').collect();

                    if parts.len() >= 2 {
                        if let Ok(_count) = parts[0].parse::<usize>() {
                            for (i, name) in parts[1..].iter().enumerate() {
                                if !name.is_empty() {
                                    self.push_log(
                                        DebugLogEntry::new(
                                            LogLevel::Debug,
                                            "AGWPE",
                                            &format!("Port {}: {}", i, name),
                                        )
                                        .with_direction(Direction::Rx),
                                    );
                                    ports.push(PortInfo {
                                        port_num: i as u8,
                                        description: name.to_string(),
                                    });
                                }
                            }
                        }
                    }
                }
                break;
            }
        }

        self.push_log(DebugLogEntry::new(
            LogLevel::Info,
            "AGWPE",
            &format!("Discovered {} port(s)", ports.len()),
        ));

        if let Some(state) = &self.state {
            let mut s = state.lock_or_poisoned();
            s.set_ports(ports);
        }

        Ok(())
    }

    // Send an AGWPE-P 'd' (0x64) Disconnect and wait briefly for the peer's
    // "*** DISCONNECTED FROM ..." confirmation frame so the AX.25 link tears
    // down cleanly.  Without this, LinBPQ keeps the layer-2 link up and treats
    // a subsequent Connect as a sequence-number reset on the *existing*
    // session — so it never spawns a fresh application instance and any
    // reconnect attempt hangs waiting for a callsign prompt that will never
    // come.  A missing confirmation must not stall the caller, so the drain
    // has a hard 3s deadline.
    pub(crate) async fn send_disconnect_and_drain(&mut self) {
        let disc = AgwpeFrame::new(
            self.agwpe_port,
            FrameType::Disconnect,
            &self.local_callsign,
            &self.remote_callsign,
            Vec::new(),
        );
        self.push_log(
            DebugLogEntry::new(LogLevel::Info, "PROTOCOL", "AX.25 disconnect (AGWPE 'd')")
                .with_direction(Direction::Tx),
        );
        if self.stream.is_none() {
            return;
        }
        let _ = self.send_frame_internal(&disc).await;

        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            match tokio::time::timeout_at(deadline, self.read_frame_from_stream()).await {
                Ok(Ok(frame)) if matches!(frame.frame_type, FrameType::Disconnect) => {
                    self.push_log(
                        DebugLogEntry::new(
                            LogLevel::Debug,
                            "PROTOCOL",
                            "Received AX.25 disconnect confirmation",
                        )
                        .with_direction(Direction::Rx),
                    );
                    break;
                }
                // Any other frame is stale in-flight traffic from the dying
                // session — discard and keep draining until the confirmation
                // or the deadline.
                Ok(Ok(_)) => continue,
                _ => break,
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::transport::Transport for AgwpeTransport {
    async fn connect_modem(
        &mut self,
        cfg: &crate::transport::TransportConfig,
    ) -> Result<(), crate::transport::TransportError> {
        self.connect_modem_internal(&cfg.agwpe.host, cfg.agwpe.port, &cfg.local_callsign)
            .await
            .map_err(agwpe_to_transport_err)
    }

    async fn disconnect_modem(&mut self) -> Result<(), crate::transport::TransportError> {
        self.stream = None;
        self.read_buf.clear();

        if let Some(state) = &self.state {
            let entry = {
                let mut s = state.lock_or_poisoned();
                s.clear_ports();
                s.set_connection_state(ConnectionState::Disconnected)
            };
            if let Some(tx) = &self.log_tx {
                let _ = tx.send(entry);
            }
        }
        self.push_log(DebugLogEntry::new(
            LogLevel::Info,
            "AGWPE",
            "Disconnected from AGWPE",
        ));

        Ok(())
    }

    async fn open_session(
        &mut self,
        cfg: &crate::transport::SessionConfig,
    ) -> Result<(), crate::transport::TransportError> {
        // The manager owns the connect+handshake state machine (via
        // session::ax25_open_and_await_connected + perform_bpq_handshake);
        // this method just records the target so subsequent Transport calls
        // (send/recv/open_ax25_link) address the right peer.
        self.remote_callsign = cfg.remote_callsign.clone();
        self.agwpe_port = cfg.agwpe_port;
        self.local_callsign = cfg.local_callsign.clone();
        Ok(())
    }

    async fn close_session(&mut self) -> Result<(), crate::transport::TransportError> {
        if !self.is_connected() {
            return Err(crate::transport::TransportError::NotConnected);
        }
        self.send_disconnect_and_drain().await;
        Ok(())
    }

    async fn send(&mut self, data: &[u8]) -> Result<(), crate::transport::TransportError> {
        if self.stream.is_none() {
            return Err(crate::transport::TransportError::NotConnected);
        }
        let chunk_size = 256;
        for chunk in data.chunks(chunk_size) {
            let frame = AgwpeFrame::new(
                self.agwpe_port,
                FrameType::SendData,
                &self.local_callsign,
                &self.remote_callsign,
                chunk.to_vec(),
            );
            let stream = self.stream.as_mut().unwrap();
            stream
                .write_all(&frame.encode())
                .await
                .map_err(crate::transport::TransportError::Io)?;
        }
        if let Some(stream) = self.stream.as_mut() {
            stream
                .flush()
                .await
                .map_err(crate::transport::TransportError::Io)?;
        }
        Ok(())
    }

    async fn recv(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<crate::transport::TransportEvent, crate::transport::TransportError> {
        let frame = self
            .read_frame_with_timeout_deadline(deadline)
            .await
            .map_err(|e| match e {
                AgwpeError::Timeout => crate::transport::TransportError::Timeout,
                AgwpeError::NotConnected => crate::transport::TransportError::NotConnected,
                AgwpeError::Io(e) => crate::transport::TransportError::Io(e),
                other => crate::transport::TransportError::ModemError(other.to_string()),
            })?;

        match frame.frame_type {
            FrameType::Disconnect => {
                let reason = String::from_utf8_lossy(&frame.data).to_string();
                Ok(crate::transport::TransportEvent::Disconnected { reason })
            }
            FrameType::SendData => {
                if is_session_dead_payload(&frame.data) {
                    let reason = String::from_utf8_lossy(&frame.data).to_string();
                    Ok(crate::transport::TransportEvent::Disconnected { reason })
                } else {
                    Ok(crate::transport::TransportEvent::Data(frame.data))
                }
            }
            // AGWPE-standard 'c' (0x63) — should have been consumed by
            // `open_ax25_link`, but may arrive stale mid-session (e.g. if the
            // modem echoes a re-connect confirmation).  Treat as empty data so
            // callers skip it gracefully rather than blocking.
            FrameType::Connected => Ok(crate::transport::TransportEvent::Data(vec![])),
            // LinBPQ sends 'C' (0x43) for both connect-request and connected
            // notification.  `open_ax25_link` already consumed the first one.
            // Any stray Connect frame that arrives during a session is
            // in-band data (LinBPQ occasionally echoes control text into the
            // stream), so surface the payload as-is and let the upper layers
            // scan for RESP headers or the session-death markers.
            FrameType::Connect => Ok(crate::transport::TransportEvent::Data(frame.data)),
            // ConnectionRejected during a live session signals the peer tore
            // down the link — surface as a Disconnected event.
            FrameType::ConnectionRejected => {
                let reason = format!(
                    "connection rejected by peer: {}",
                    String::from_utf8_lossy(&frame.data)
                );
                Ok(crate::transport::TransportEvent::Disconnected { reason })
            }
            _ => Ok(crate::transport::TransportEvent::Data(vec![])),
        }
    }

    fn port_query_supported(&self) -> bool {
        true
    }

    async fn open_ax25_link(&mut self) -> Result<(), crate::transport::TransportError> {
        if !self.is_connected() {
            return Err(crate::transport::TransportError::NotConnected);
        }
        let connect_frame = AgwpeFrame::new(
            self.agwpe_port,
            FrameType::Connect,
            &self.local_callsign,
            &self.remote_callsign,
            vec![],
        );
        self.push_log(
            DebugLogEntry::new(
                LogLevel::Info,
                "PROTOCOL",
                &format!(
                    "AX.25 connect to {} on port {}",
                    self.remote_callsign, self.agwpe_port
                ),
            )
            .with_direction(Direction::Tx),
        );
        self.send_frame_internal(&connect_frame)
            .await
            .map_err(agwpe_to_transport_err)?;

        // Block until the peer sends a Connected (0x63) or LinBPQ-style
        // Connect (0x43) with `*** CONNECTED` payload, or a rejection.  This
        // makes the trait method self-contained: callers no longer need a
        // post-call recv loop to discover whether the link came up.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let frame = self
                .read_frame_with_timeout_deadline(deadline)
                .await
                .map_err(|e| match e {
                    AgwpeError::Timeout => crate::transport::TransportError::Timeout,
                    AgwpeError::NotConnected => crate::transport::TransportError::NotConnected,
                    AgwpeError::Io(io) => crate::transport::TransportError::Io(io),
                    other => crate::transport::TransportError::ModemError(other.to_string()),
                })?;

            match frame.frame_type {
                // Standard AGWPE-P 'c' (0x63) — connected.
                FrameType::Connected => {
                    self.push_log(
                        DebugLogEntry::new(
                            LogLevel::Info,
                            "PROTOCOL",
                            "AX.25 connected (AGWPE 'c')",
                        )
                        .with_direction(Direction::Rx),
                    );
                    return Ok(());
                }
                // LinBPQ sends 'C' (0x43) for both outgoing connect-request
                // AND the connected notification.  Distinguish by payload.
                FrameType::Connect
                    if frame.data.starts_with(b"***")
                        && frame
                            .data
                            .windows(b"CONNECTED".len())
                            .any(|w| w == b"CONNECTED") =>
                {
                    self.push_log(
                        DebugLogEntry::new(
                            LogLevel::Info,
                            "PROTOCOL",
                            "AX.25 connected (LinBPQ '*** CONNECTED')",
                        )
                        .with_direction(Direction::Rx),
                    );
                    return Ok(());
                }
                FrameType::ConnectionRejected => {
                    let reason = format!(
                        "connection rejected by peer: {}",
                        String::from_utf8_lossy(&frame.data)
                    );
                    self.push_log(
                        DebugLogEntry::new(LogLevel::Info, "ERROR", &reason)
                            .with_direction(Direction::Rx),
                    );
                    return Err(crate::transport::TransportError::SessionRejected(reason));
                }
                // Any other frame (data, stale notifications) — discard and
                // keep waiting for the connection confirmation.
                _ => {
                    self.push_log(DebugLogEntry::new(
                        LogLevel::Debug,
                        "PROTOCOL",
                        &format!(
                            "Ignoring {:?} frame while awaiting AX.25 connect confirmation",
                            frame.frame_type
                        ),
                    ));
                }
            }
        }
    }

    async fn reopen_modem_connection(&mut self) -> Result<(), crate::transport::TransportError> {
        // Snapshot AGWPE endpoint + our callsign before we drop the socket so
        // we can reopen with the same identity below.
        let (agwpe_host, agwpe_tcp_port) = if let Some(state) = &self.state {
            let s = state.lock_or_poisoned();
            (s.config.agwpe_host.clone(), s.config.agwpe_port)
        } else {
            return Err(crate::transport::TransportError::ModemError(
                "reopen_modem_connection called without attached state".to_string(),
            ));
        };
        let callsign = self.local_callsign.clone();

        // Tear down the AX.25 link on our side before re-opening so the peer
        // (e.g. LinBPQ) actually closes the dead application session and will
        // spawn a fresh one on our next Connect.  See
        // `send_disconnect_and_drain` for why an empty SendData frame is
        // insufficient.
        if self.stream.is_some() {
            self.send_disconnect_and_drain().await;
        }

        // Drop and re-establish the AGWPE TCP session.  Direwolf's AX.25 reader
        // thread can freeze when the PipeWire audio pipeline stalls — a fresh
        // TCP session forces Direwolf to reset per-client state on our side of
        // the pipe.  If Direwolf's audio-side is still stuck, the subsequent
        // Connect will time out and surface a clean Error via the outer catch.
        self.stream = None;
        self.read_buf.clear();

        self.connect_modem_internal(&agwpe_host, agwpe_tcp_port, &callsign)
            .await
            .map_err(agwpe_to_transport_err)
    }

    async fn query_ports(&mut self) -> Result<(), crate::transport::TransportError> {
        self.query_ports_internal()
            .await
            .map_err(agwpe_to_transport_err)
    }
}

fn agwpe_to_transport_err(e: AgwpeError) -> crate::transport::TransportError {
    match e {
        AgwpeError::Timeout => crate::transport::TransportError::Timeout,
        AgwpeError::NotConnected => crate::transport::TransportError::NotConnected,
        AgwpeError::Io(e) => crate::transport::TransportError::Io(e),
        other => crate::transport::TransportError::ModemError(other.to_string()),
    }
}

/// Test helpers for unit tests that need to construct raw AGWPE frames.
/// Gated by `#[cfg(test)]` — no production code or integration test consumes
/// this module; it existed as public API only to support the `mod.rs` unit
/// test which already lives under `#[cfg(test)]`.
#[cfg(test)]
pub mod test_helpers {
    use super::*;

    /// Build the bytes of an AGWPE Disconnect frame with the given payload.
    pub fn disconnect_frame_bytes(payload: &[u8]) -> Vec<u8> {
        let frame = AgwpeFrame::new(
            0,
            FrameType::Disconnect,
            "N0CALL",
            "N0CALL-8",
            payload.to_vec(),
        );
        frame.encode()
    }
}

#[cfg(test)]
impl AgwpeTransport {
    /// Create a loopback TCP pair for tests.
    /// Returns `(client_side_stream, AgwpeTransport_wrapping_server_side)`.
    pub async fn for_test_pair() -> (tokio::net::TcpStream, Self) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server_side, _) = listener.accept().await.unwrap();
        let mut t = AgwpeTransport::new();
        t.stream = Some(server_side);
        t.local_callsign = "N0CALL".to_string();
        t.remote_callsign = "N0CALL-8".to_string();
        t.agwpe_port = 0;
        (client, t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_encode_decode() {
        let frame = AgwpeFrame::new(
            0,
            FrameType::RegisterCallsign,
            "N0CALL",
            "",
            vec![],
        );

        let encoded = frame.encode();
        let decoded = AgwpeFrame::decode(&encoded).unwrap();

        assert_eq!(decoded.port, 0);
        assert_eq!(decoded.frame_type, FrameType::RegisterCallsign);
        assert_eq!(decoded.call_from, "N0CALL");
        assert_eq!(decoded.call_to, "");
        assert_eq!(decoded.data_len, 0);
    }

    #[test]
    fn test_frame_with_data() {
        let data = b"Hello, world!".to_vec();
        let frame = AgwpeFrame::new(
            1,
            FrameType::SendData,
            "N0CALL",
            "NODE1",
            data.clone(),
        );

        let encoded = frame.encode();
        let decoded = AgwpeFrame::decode(&encoded).unwrap();

        assert_eq!(decoded.port, 1);
        assert_eq!(decoded.frame_type, FrameType::SendData);
        assert_eq!(decoded.call_from, "N0CALL");
        assert_eq!(decoded.call_to, "NODE1");
        assert_eq!(decoded.data_len, 13);
        assert_eq!(decoded.data, data);
    }

    #[test]
    fn test_callsign_truncation() {
        let frame = AgwpeFrame::new(
            0,
            FrameType::RegisterCallsign,
            "VERYLONGCALLSIGN",
            "ANOTHERLONGONE",
            vec![],
        );

        let encoded = frame.encode();
        let decoded = AgwpeFrame::decode(&encoded).unwrap();

        assert_eq!(decoded.call_from.len(), 9);
        assert_eq!(decoded.call_to.len(), 9);
    }

    #[test]
    fn test_decode_rejects_oversized_data_len() {
        // Hand-build a header that claims a data_len far above MAX_FRAME_DATA_SIZE.
        let mut buf = vec![0u8; AgwpeFrame::HEADER_SIZE];
        buf[4] = FrameType::Disconnect as u8;
        let huge = (MAX_FRAME_DATA_SIZE as u32 + 1).to_le_bytes();
        buf[28..32].copy_from_slice(&huge);
        let result = AgwpeFrame::decode(&buf);
        assert!(matches!(result, Err(AgwpeError::InvalidFrame(_))));
    }

    #[test]
    fn test_frame_type_conversion() {
        assert_eq!(FrameType::try_from(0x58).unwrap(), FrameType::RegisterCallsign);
        assert_eq!(FrameType::try_from(0x78).unwrap(), FrameType::RegistrationResponse);
        assert_eq!(FrameType::try_from(0x43).unwrap(), FrameType::Connect);
        assert_eq!(FrameType::try_from(0x63).unwrap(), FrameType::Connected);
        assert_eq!(FrameType::try_from(0x64).unwrap(), FrameType::Disconnect);
        assert_eq!(FrameType::try_from(0x44).unwrap(), FrameType::SendData);
        assert_eq!(FrameType::try_from(0x52).unwrap(), FrameType::ConnectionRejected);
        assert_eq!(FrameType::try_from(0x47).unwrap(), FrameType::QueryPorts);
        assert_eq!(FrameType::try_from(0x67).unwrap(), FrameType::PortInfo);
        assert!(FrameType::try_from(0xFF).is_err());
    }

    #[test]
    fn test_new_error_variants_display() {
        let e = AgwpeError::SessionDied { reason: "no response after 30s".to_string() };
        assert_eq!(e.to_string(), "Session died: no response after 30s");

        let e = AgwpeError::NeedsReconsent;
        assert_eq!(e.to_string(), "Session dropped and requires re-consent");

        let e = AgwpeError::DisconnectedByOperator;
        assert_eq!(e.to_string(), "Disconnected by operator");
    }

    #[test]
    fn test_is_session_dead_payload() {
        // Direwolf-style disconnect notification, at start of buffer.
        assert!(is_session_dead_payload(b"*** DISCONNECTED FROM Station N0CALL\r"));
        assert!(is_session_dead_payload(b"*** DISCONNECTED"));

        // LinBPQ "Returned to Node" message when the WEB app exits.
        assert!(is_session_dead_payload(b"Returned to Node DEMO:N0CALL-7\r"));

        // Markers embedded mid-buffer (arriving after some already-appended bytes
        // from a prior frame) must still be detected.
        assert!(is_session_dead_payload(b"partial response...\r\n*** DISCONNECTED FROM X"));
        assert!(is_session_dead_payload(b"garbage bytes\rReturned to Node DEMO:X"));

        // Negatives — must not match on happy-path or unrelated control text.
        assert!(!is_session_dead_payload(b"RESP0 300 abc123 3600\r"));
        assert!(!is_session_dead_payload(b""));
        assert!(!is_session_dead_payload(b"*** CONNECTED WITH N0CALL"));
        // Substring must include the space; a raw "Returned" alone shouldn't trigger.
        assert!(!is_session_dead_payload(b"Returned"));
    }
}
