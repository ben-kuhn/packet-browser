use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
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
const MAX_FRAME_DATA_SIZE: usize = 64 * 1024;
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;
const MAX_HANDSHAKE_TEXT: usize = 64 * 1024;

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

pub enum AgwpeCommand {
    ConnectToAgwpe {
        host: String,
        port: u16,
        callsign: String,
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    DisconnectAgwpe {
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    QueryPorts {
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    Ax25Connect {
        target: String,
        port_num: u8,
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    Ax25Disconnect {
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    SendRequest {
        data: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, AgwpeError>>,
    },
    SendRequestWithReconnect {
        data: Vec<u8>,
        reply: oneshot::Sender<Result<Vec<u8>, AgwpeError>>,
    },
}

#[derive(Clone)]
pub struct AgwpeManager {
    command_tx: mpsc::Sender<AgwpeCommand>,
}

impl AgwpeManager {
    pub fn new(
        state: SharedState,
        log_tx: broadcast::Sender<DebugLogEntry>,
        response_timeout_secs: u64,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);

        tokio::spawn(async move {
            background_task(command_rx, state, log_tx, response_timeout_secs).await;
        });

        Self { command_tx }
    }

    pub async fn connect_to_agwpe(
        &self,
        host: String,
        port: u16,
        callsign: String,
    ) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::ConnectToAgwpe {
                host,
                port,
                callsign,
                reply: tx,
            })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn disconnect_agwpe(&self) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::DisconnectAgwpe { reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn query_ports(&self) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::QueryPorts { reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn ax25_connect(&self, target: String, port_num: u8) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::Ax25Connect {
                target,
                port_num,
                reply: tx,
            })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn ax25_disconnect(&self) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::Ax25Disconnect { reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn send_request(&self, data: Vec<u8>) -> Result<Vec<u8>, AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::SendRequest { data, reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn send_request_with_reconnect(&self, data: Vec<u8>) -> Result<Vec<u8>, AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(AgwpeCommand::SendRequestWithReconnect { data, reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }
}

struct BackgroundState {
    stream: Option<TcpStream>,
    local_callsign: String,
    remote_callsign: String,
    agwpe_port: u8,
    read_buf: Vec<u8>,
    response_timeout_secs: u64,
    abort_reconnect: Arc<AtomicBool>,
}

impl BackgroundState {
    fn new() -> Self {
        Self {
            stream: None,
            local_callsign: String::new(),
            remote_callsign: String::new(),
            agwpe_port: 0,
            read_buf: Vec::new(),
            response_timeout_secs: 30,
            abort_reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn push_log(state: &SharedState, log_tx: &broadcast::Sender<DebugLogEntry>, entry: DebugLogEntry) {
        {
            let mut s = state.lock_or_poisoned();
            s.add_log(entry.clone());
        }
        let _ = log_tx.send(entry);
    }

    fn set_state(state: &SharedState, log_tx: &broadcast::Sender<DebugLogEntry>, cs: ConnectionState) {
        let entry = {
            let mut s = state.lock_or_poisoned();
            s.set_connection_state(cs)
        };
        let _ = log_tx.send(entry);
    }

    async fn send_frame(stream: &mut TcpStream, frame: &AgwpeFrame) -> Result<(), AgwpeError> {
        stream.write_all(&frame.encode()).await?;
        stream.flush().await?;
        Ok(())
    }

    async fn read_frame_from_stream(&mut self) -> Result<AgwpeFrame, AgwpeError> {
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
                    // Debug: print raw bytes
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
}

async fn background_task(
    mut command_rx: mpsc::Receiver<AgwpeCommand>,
    state: SharedState,
    log_tx: broadcast::Sender<DebugLogEntry>,
    response_timeout_secs: u64,
) {
    let mut bg = BackgroundState::new();
    // Clamp to at least 1s: a zero-second timeout would fire on every read,
    // instantly SessionDied-ing every request and looping through reconnects.
    bg.response_timeout_secs = response_timeout_secs.max(1);

    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            AgwpeCommand::ConnectToAgwpe {
                host,
                port,
                callsign,
                reply,
            } => {
                let result = handle_connect_to_agwpe(&mut bg, &state, &log_tx, &host, port, &callsign).await;
                let _ = reply.send(result);
            }
            AgwpeCommand::DisconnectAgwpe { reply } => {
                let result = handle_disconnect_agwpe(&mut bg, &state, &log_tx).await;
                let _ = reply.send(result);
            }
            AgwpeCommand::QueryPorts { reply } => {
                let result = handle_query_ports(&mut bg, &state, &log_tx).await;
                let _ = reply.send(result);
            }
            AgwpeCommand::Ax25Connect {
                target,
                port_num,
                reply,
            } => {
                let result = handle_ax25_connect(&mut bg, &state, &log_tx, &target, port_num).await;
                let _ = reply.send(result);
            }
            AgwpeCommand::Ax25Disconnect { reply } => {
                let result = handle_ax25_disconnect(&mut bg, &state, &log_tx).await;
                let _ = reply.send(result);
            }
            AgwpeCommand::SendRequest { data, reply } => {
                let result = handle_send_request(&mut bg, &state, &log_tx, data).await;
                let _ = reply.send(result);
            }
            AgwpeCommand::SendRequestWithReconnect { data, reply } => {
                let result = handle_send_request_with_reconnect(&mut bg, &state, &log_tx, data).await;
                let _ = reply.send(result);
            }
        }
    }

    BackgroundState::push_log(
        &state,
        &log_tx,
        DebugLogEntry::new(LogLevel::Info, "STATE", "Background task shutting down"),
    );
}

async fn handle_connect_to_agwpe(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    host: &str,
    port: u16,
    callsign: &str,
) -> Result<(), AgwpeError> {
    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "AGWPE",
            &format!("Connecting to AGWPE at {}:{}", host, port),
        ),
    );

    let stream = match TcpStream::connect(format!("{}:{}", host, port)).await {
        Ok(s) => {
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Info, "AGWPE", "TCP connection established"),
            );
            s
        }
        Err(e) => {
            let msg = format!("TCP connection failed: {}", e);
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
            );
            BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
            return Err(AgwpeError::ConnectionFailed(msg));
        }
    };

    bg.stream = Some(stream);
    bg.local_callsign = callsign.to_string();
    bg.read_buf.clear();

    let reg_frame = AgwpeFrame::new(
        0,
        FrameType::RegisterCallsign,
        callsign,
        "",
        vec![],
    );

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "AGWPE", &format!("Sending registration for {}", callsign))
            .with_direction(Direction::Tx),
    );

    if let Err(e) = bg.stream.as_mut().unwrap().write_all(&reg_frame.encode()).await {
        bg.stream = None;
        let msg = format!("Registration send failed: {}", e);
        BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
        return Err(AgwpeError::RegistrationFailed(msg));
    }
    let _ = bg.stream.as_mut().unwrap().flush().await;

    match bg.read_frame_with_timeout(5).await {
        Ok(frame) if frame.frame_type == FrameType::RegisterCallsign && frame.data == vec![0x01] => {
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Debug, "AGWPE", "Registration successful")
                    .with_direction(Direction::Rx),
            );
            BackgroundState::set_state(state, log_tx, ConnectionState::AgwpeConnected);
            Ok(())
        }
        Ok(frame) => {
            bg.stream = None;
            let msg = format!("Unexpected response to registration: {:?} (port={}, call_from={}, call_to={}, data={:?})", 
                frame.frame_type, frame.port, frame.call_from, frame.call_to, frame.data);
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Debug, "AGWPE", &msg),
            );
            BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
            Err(AgwpeError::RegistrationFailed(msg))
        }
        Err(e) => {
            bg.stream = None;
            let msg = format!("Registration timeout: {}", e);
            BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
            Err(AgwpeError::RegistrationFailed(msg))
        }
    }
}

async fn handle_disconnect_agwpe(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    bg.stream = None;
    bg.read_buf.clear();

    let state_entry = {
        let mut s = state.lock_or_poisoned();
        s.clear_ports();
        s.set_connection_state(ConnectionState::Disconnected)
    };
    let _ = log_tx.send(state_entry);

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Info, "AGWPE", "Disconnected from AGWPE"),
    );

    Ok(())
}

async fn handle_query_ports(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    if !bg.is_connected() {
        return Err(AgwpeError::NotConnected);
    }

    let query_frame = AgwpeFrame::new(0, FrameType::QueryPorts, &bg.local_callsign, "", vec![]);

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "AGWPE", "Querying ports")
            .with_direction(Direction::Tx),
    );

    BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &query_frame).await?;

    let mut ports = Vec::new();

    loop {
        let frame = bg.read_frame_with_timeout(5).await?;

        BackgroundState::push_log(
            state,
            log_tx,
            DebugLogEntry::new(
                LogLevel::Debug,
                "AGWPE",
                &format!("Port query response: frame_type={:?}, data_len={}, data={:?}", 
                    frame.frame_type, frame.data_len, frame.data),
            )
            .with_direction(Direction::Rx),
        );

        // Port info response can be either 'G' (0x47) or 'g' (0x67) depending on implementation
        if frame.frame_type == FrameType::QueryPorts || frame.frame_type == FrameType::PortInfo {
            if frame.data_len == 0 {
                break;
            }
            if !frame.data.is_empty() {
                // Parse format: "count;name1;name2;...;"
                let data_str = String::from_utf8_lossy(&frame.data);
                let data_str = data_str.trim_end_matches('\0');
                let parts: Vec<&str> = data_str.split(';').collect();
                
                if parts.len() >= 2 {
                    // First part is count, rest are port names
                    if let Ok(_count) = parts[0].parse::<usize>() {
                        for (i, name) in parts[1..].iter().enumerate() {
                            if !name.is_empty() {
                                BackgroundState::push_log(
                                    state,
                                    log_tx,
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
            // After receiving port info, we're done
            break;
        }
    }

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "AGWPE",
            &format!("Discovered {} port(s)", ports.len()),
        ),
    );

    {
        let mut s = state.lock_or_poisoned();
        s.set_ports(ports);
    }

    Ok(())
}

/// Send the BPQ application command (e.g. "WEB\n") over the current AX.25
/// session.  Called only when `skip_bpq_app` is false.
async fn bpq_send_app_command(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    let bpq_command = {
        let s = state.lock_or_poisoned();
        s.config.bpq_command.clone()
    };

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "BPQ",
            &format!("Starting BPQ handshake with command: {}", bpq_command),
        ),
    );

    let cmd_data = format!("{}\n", bpq_command);

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", &format!("Sending BPQ command: {:?}", cmd_data))
            .with_direction(Direction::Tx),
    );

    let cmd_frame = AgwpeFrame::new(
        bg.agwpe_port,
        FrameType::SendData,
        &bg.local_callsign,
        &bg.remote_callsign,
        cmd_data.into_bytes(),
    );

    BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &cmd_frame).await?;
    tracing::trace!("[BPQ] Sent BPQ command frame");
    Ok(())
}

/// Wait for the server's callsign prompt and send our local callsign in
/// response.  The prompt is identified by the presence of the word "callsign"
/// (or "AGREE", which some deployments include in the same banner line).
async fn bpq_await_callsign_prompt_and_send_callsign(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    let callsign = bg.local_callsign.clone();

    // The far side (packet-browser-server, via LinBPQ HOST 0) does NOT
    // auto-inject the callsign in this deployment — LinBPQ opens the TCP
    // bridge silently and waits for actual data to flow. The server prompts
    // for the callsign, and we send ours in response. Match the prompt on
    // "callsign" (or "AGREE", which some pre-existing setups fold into a
    // single banner) and send once we see it.
    let mut received_text = String::new();
    let mut callsign_prompt_found = false;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Waiting for callsign prompt...")
            .with_direction(Direction::Rx),
    );
    tracing::trace!("[BPQ] Waiting for callsign prompt...");

    loop {
        let frame = bg.read_frame_with_timeout(30).await?;
        tracing::trace!(
            "[BPQ] Received frame type {:?} (0x{:02X}), data_len={}, data={:?}",
            frame.frame_type,
            frame.frame_type as u8,
            frame.data_len,
            String::from_utf8_lossy(&frame.data[..frame.data_len.min(100) as usize])
        );

        match frame.frame_type {
            FrameType::Disconnect | FrameType::SendData => {
                let text = String::from_utf8_lossy(&frame.data).to_string();
                if received_text.len() + text.len() > MAX_HANDSHAKE_TEXT {
                    return Err(AgwpeError::ConnectionFailed(
                        "Handshake text exceeded maximum size".to_string(),
                    ));
                }
                received_text.push_str(&text);
                if received_text.contains("callsign") || received_text.contains("AGREE") {
                    callsign_prompt_found = true;
                    break;
                }
            }
            FrameType::ConnectionRejected => {
                return Err(AgwpeError::ConnectionFailed(
                    "Connection rejected during BPQ handshake".to_string(),
                ));
            }
            _ => {
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Debug,
                        "BPQ",
                        &format!("Ignoring frame during handshake: {:?}", frame.frame_type),
                    ),
                );
            }
        }
    }

    if !callsign_prompt_found {
        return Err(AgwpeError::ConnectionFailed(
            "Callsign prompt not received".to_string(),
        ));
    }

    // Send our callsign.
    let call_data = format!("{}\n", callsign);
    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", &format!("Sending callsign: {:?}", call_data))
            .with_direction(Direction::Tx),
    );
    let call_frame = AgwpeFrame::new(
        bg.agwpe_port,
        FrameType::SendData,
        &bg.local_callsign,
        &bg.remote_callsign,
        call_data.into_bytes(),
    );
    BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &call_frame).await?;
    tracing::trace!("[BPQ] Sent callsign frame");
    Ok(())
}

/// Wait for the server's logging-disclaimer / AGREE prompt and return the raw
/// disclaimer text (byte-for-byte, no trim or normalisation).  The caller
/// decides what to do with it — either surface it for operator consent or
/// compare it against a stored value for auto-consent.
async fn bpq_await_disclaimer(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<String, AgwpeError> {
    let mut received_text = String::new();
    let mut agree_prompt_found = false;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Waiting for AGREE prompt...")
            .with_direction(Direction::Rx),
    );
    tracing::trace!("[BPQ] Waiting for AGREE prompt...");

    loop {
        let frame = bg.read_frame_with_timeout(30).await?;

        tracing::trace!("[BPQ] Received frame type {:?} (0x{:02X}), data_len={}, data={:?}",
            frame.frame_type, frame.frame_type as u8, frame.data_len,
            String::from_utf8_lossy(&frame.data[..frame.data_len.min(100) as usize]));

        match frame.frame_type {
            // Direwolf reports received data with frame type 'D' (0x44) — the
            // same byte we use to send data — so we accept both here.
            FrameType::Disconnect | FrameType::SendData => {
                let text = String::from_utf8_lossy(&frame.data).to_string();
                if received_text.len() + text.len() > MAX_HANDSHAKE_TEXT {
                    return Err(AgwpeError::ConnectionFailed(
                        "Handshake text exceeded maximum size".to_string(),
                    ));
                }
                received_text.push_str(&text);

                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Trace,
                        "BPQ",
                        &format!("Received text: {:?}", text),
                    )
                    .with_direction(Direction::Rx),
                );

                if received_text.to_uppercase().contains("AGREE") {
                    agree_prompt_found = true;
                    break;
                }
            }
            FrameType::ConnectionRejected => {
                return Err(AgwpeError::ConnectionFailed(
                    "Connection rejected during BPQ handshake".to_string(),
                ));
            }
            _ => {
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Debug,
                        "BPQ",
                        &format!("Ignoring frame during handshake: {:?}", frame.frame_type),
                    ),
                );
            }
        }
    }

    if !agree_prompt_found {
        return Err(AgwpeError::ConnectionFailed(
            "AGREE prompt not received".to_string(),
        ));
    }

    Ok(received_text)
}

async fn perform_bpq_handshake(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    let skip_bpq_app = {
        let s = state.lock_or_poisoned();
        s.config.skip_bpq_app
    };

    if skip_bpq_app {
        BackgroundState::push_log(
            state,
            log_tx,
            DebugLogEntry::new(
                LogLevel::Info,
                "BPQ",
                "Skipping BPQ application command (direct connection mode)",
            ),
        );
    } else {
        bpq_send_app_command(bg, state, log_tx).await?;
    }

    bpq_await_callsign_prompt_and_send_callsign(bg, state, log_tx).await?;

    let received_text = bpq_await_disclaimer(bg, state, log_tx).await?;

    // Park the handshake in `AwaitingConsent` and hand the disclaimer to the
    // UI. We do not send "AGREE\n" until the operator explicitly clicks
    // through /api/consent. This is the real consent step — auto-sending
    // "AGREE" from code would sign a logging disclaimer no human ever saw.
    let (consent_tx, consent_rx) = oneshot::channel::<bool>();
    let state_entry = {
        let mut s = state.lock_or_poisoned();
        s.pending_consent = Some(consent_tx);
        s.set_connection_state(ConnectionState::AwaitingConsent {
            disclaimer: received_text.clone(),
        })
    };
    // Broadcast the STATE transition so the browser's SSE listener flips the
    // UI into "Awaiting consent" and opens the consent modal. Without this
    // the handshake blocks on `consent_rx` with no way for the operator to
    // reply.
    let _ = log_tx.send(state_entry);
    let _ = log_tx.send(DebugLogEntry::new(
        LogLevel::Info,
        "BPQ",
        "Waiting for operator consent",
    ));
    tracing::trace!("[BPQ] Waiting for operator consent");

    // Long timeout so a user who wanders off doesn't wedge the handshake
    // forever, but generous enough for a real consent read.
    let accepted = match tokio::time::timeout(
        std::time::Duration::from_secs(300),
        consent_rx,
    )
    .await
    {
        Ok(Ok(accepted)) => accepted,
        // Sender dropped (e.g. new connect started, or explicit disconnect).
        Ok(Err(_)) => false,
        Err(_) => {
            // Best-effort: clear the pending slot so a stale sender doesn't
            // linger and mislead a later /api/consent call.
            let mut s = state.lock_or_poisoned();
            s.pending_consent = None;
            return Err(AgwpeError::ConnectionFailed(
                "Consent timeout: operator did not respond".to_string(),
            ));
        }
    };

    if !accepted {
        return Err(AgwpeError::ConnectionFailed(
            "Operator declined the logging disclaimer".to_string(),
        ));
    }

    // Consent granted — now send "AGREE\n".
    let agree_data = b"AGREE\n".to_vec();
    let agree_frame = AgwpeFrame::new(
        bg.agwpe_port,
        FrameType::SendData,
        &bg.local_callsign,
        &bg.remote_callsign,
        agree_data,
    );

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Sending AGREE")
            .with_direction(Direction::Tx),
    );

    BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &agree_frame).await?;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Info, "BPQ", "BPQ handshake completed successfully"),
    );

    Ok(())
}

/// Send an AX.25 Connect frame and loop until the peer acknowledges the
/// connection (either via a `Connected` frame or LinBPQ's `Connect` + `***
/// CONNECTED` banner).  On success `bg.remote_callsign` and `bg.agwpe_port`
/// are already set by the caller (`handle_ax25_connect`) or were preserved
/// from the previous session (`handle_reconnect`).
async fn ax25_open_and_await_connected(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    let target = bg.remote_callsign.clone();
    let port_num = bg.agwpe_port;

    let connect_frame = AgwpeFrame::new(
        port_num,
        FrameType::Connect,
        &bg.local_callsign,
        &target,
        vec![],
    );

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "PROTOCOL",
            &format!("AX.25 connect to {} on port {}", target, port_num),
        )
        .with_direction(Direction::Tx),
    );

    BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &connect_frame).await?;

    loop {
        let frame = bg.read_frame_with_timeout(30).await?;

        tracing::trace!("[AGWPE] AX.25 connect: received frame type {:?} (0x{:02X}), data_len={}, data={:?}",
            frame.frame_type, frame.frame_type as u8, frame.data_len,
            String::from_utf8_lossy(&frame.data[..frame.data_len.min(50) as usize]));

        match frame.frame_type {
            FrameType::Connected => {
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Info,
                        "PROTOCOL",
                        &format!("AX.25 connected to {}", target),
                    )
                    .with_direction(Direction::Rx),
                );
                return Ok(());
            }
            // LinBPQ sends 0x43 ('C') for both connect request AND connected notification.
            // Check if this is actually a connected notification by looking at the data.
            FrameType::Connect if frame.data.starts_with(b"***") => {
                let text = String::from_utf8_lossy(&frame.data);
                if text.contains("CONNECTED") {
                    BackgroundState::push_log(
                        state,
                        log_tx,
                        DebugLogEntry::new(
                            LogLevel::Info,
                            "PROTOCOL",
                            &format!("AX.25 connected to {} (LinBPQ style)", target),
                        )
                        .with_direction(Direction::Rx),
                    );
                    return Ok(());
                } else {
                    BackgroundState::push_log(
                        state,
                        log_tx,
                        DebugLogEntry::new(
                            LogLevel::Debug,
                            "PROTOCOL",
                            &format!("Ignoring frame while connecting: {:?}", frame.frame_type),
                        ),
                    );
                }
            }
            FrameType::ConnectionRejected => {
                let msg = format!("AX.25 connection to {} rejected", target);
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Info, "ERROR", &msg)
                        .with_direction(Direction::Rx),
                );
                return Err(AgwpeError::ConnectionFailed(msg));
            }
            _ => {
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Debug,
                        "PROTOCOL",
                        &format!("Ignoring frame while connecting: {:?}", frame.frame_type),
                    ),
                );
            }
        }
    }
}

async fn handle_ax25_connect(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    target: &str,
    port_num: u8,
) -> Result<(), AgwpeError> {
    bg.abort_reconnect.store(false, Ordering::SeqCst);

    if !bg.is_connected() {
        return Err(AgwpeError::NotConnected);
    }

    bg.remote_callsign = target.to_string();
    bg.agwpe_port = port_num;

    let state_entry = {
        let mut s = state.lock_or_poisoned();
        let entry = s.set_connection_state(ConnectionState::Connecting);
        s.set_agwpe_port(port_num);
        entry
    };
    let _ = log_tx.send(state_entry);

    // Update stored target so subsequent reconnects know where to aim.
    {
        let mut s = state.lock_or_poisoned();
        s.config.update_target(target);
    }

    ax25_open_and_await_connected(bg, state, log_tx).await?;

    // Perform BPQ handshake
    match perform_bpq_handshake(bg, state, log_tx).await {
        Ok(()) => {
            BackgroundState::set_state(state, log_tx, ConnectionState::Connected);
            Ok(())
        }
        Err(e) => {
            let msg = format!("BPQ handshake failed: {}", e);
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
            );
            BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
            Err(AgwpeError::ConnectionFailed(msg))
        }
    }
}

async fn handle_ax25_disconnect(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    bg.abort_reconnect.store(true, Ordering::SeqCst);

    if !bg.is_connected() {
        return Err(AgwpeError::NotConnected);
    }

    send_agwpe_disconnect_and_drain(bg, state, log_tx).await;

    BackgroundState::set_state(state, log_tx, ConnectionState::AgwpeConnected);

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
async fn send_agwpe_disconnect_and_drain(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) {
    let disc = AgwpeFrame::new(
        bg.agwpe_port,
        FrameType::Disconnect,
        &bg.local_callsign,
        &bg.remote_callsign,
        Vec::new(),
    );
    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Info, "PROTOCOL", "AX.25 disconnect (AGWPE 'd')")
            .with_direction(Direction::Tx),
    );
    if let Some(stream) = bg.stream.as_mut() {
        let _ = BackgroundState::send_frame(stream, &disc).await;
    } else {
        return;
    }

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match tokio::time::timeout_at(deadline, bg.read_frame_from_stream()).await {
            Ok(Ok(frame)) if matches!(frame.frame_type, FrameType::Disconnect) => {
                BackgroundState::push_log(
                    state,
                    log_tx,
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
fn is_session_dead_payload(data: &[u8]) -> bool {
    contains_slice(data, b"*** DISCONNECTED") || contains_slice(data, b"Returned to Node")
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Returns `true` iff `server_text` is byte-for-byte equal to the stored
/// disclaimer. `None` stored means we have never seen a consent, so we always
/// return `false` — the operator must consent explicitly.
fn matches_stored_disclaimer(server_text: &str, stored: Option<&str>) -> bool {
    match stored {
        Some(s) => s == server_text,
        None => false,
    }
}

/// Re-run the full AX.25 + BPQ + AGREE handshake for a session that died
/// unexpectedly.  Transitions to `Reconnecting` at entry and `Connected` on
/// successful auto-consent.  Returns `Err(NeedsReconsent)` when the server's
/// disclaimer text differs from the stored consent, leaving the state as
/// `AwaitingConsent` so the UI can open the consent modal on the operator's
/// next visit.
async fn handle_reconnect(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    reason: String,
) -> Result<(), AgwpeError> {
    BackgroundState::set_state(
        state,
        log_tx,
        ConnectionState::Reconnecting { reason: reason.clone() },
    );
    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "PROTOCOL",
            &format!("Session lost ({}); attempting reconnect", reason),
        ),
    );

    // Tear down the AX.25 link on our side before re-opening so the peer
    // (e.g. LinBPQ) actually closes the dead application session and will
    // spawn a fresh one on our next Connect.  See
    // `send_agwpe_disconnect_and_drain` for why an empty SendData frame is
    // insufficient.
    if bg.stream.is_some() {
        send_agwpe_disconnect_and_drain(bg, state, log_tx).await;
    }

    // Snapshot AGWPE endpoint + our callsign before we drop the socket so
    // we can reopen with the same identity below.
    let (agwpe_host, agwpe_tcp_port, callsign) = {
        let s = state.lock_or_poisoned();
        (
            s.config.agwpe_host.clone(),
            s.config.agwpe_port,
            bg.local_callsign.clone(),
        )
    };

    // Drop and re-establish the AGWPE TCP session.  Direwolf's AX.25 reader
    // thread can freeze when the PipeWire audio pipeline stalls — observed
    // as "Received frame queue is out of control. Length=N. Reader thread
    // is probably frozen" in direwolf.log — after which Direwolf silently
    // drops all subsequent AGWPE frames from us, including Connect.  A
    // fresh TCP session forces Direwolf to reset the per-client state on
    // our side of the pipe; combined with the proper AX.25 Disconnect we
    // just drained, this is the strongest recovery the client can do
    // without operator intervention.  If Direwolf's audio-side is still
    // stuck, the subsequent Connect will time out and surface a clean
    // Error via the outer catch below.
    bg.stream = None;
    bg.read_buf.clear();

    // Run the full handshake in an inner block so we can intercept any error
    // and transition to Error state before returning.  The NeedsReconsent path
    // is the only exit that must NOT transition to Error — it sets
    // AwaitingConsent explicitly and is handled via a dedicated early-return
    // above the inner block.
    let result: Result<(), AgwpeError> = async {
        handle_connect_to_agwpe(bg, state, log_tx, &agwpe_host, agwpe_tcp_port, &callsign).await?;
        // handle_connect_to_agwpe flips state to AgwpeConnected on success;
        // put us back in Reconnecting so the UI doesn't flicker while we
        // drive the rest of the handshake.
        BackgroundState::set_state(
            state,
            log_tx,
            ConnectionState::Reconnecting { reason: reason.clone() },
        );
        if bg.abort_reconnect.load(Ordering::SeqCst) {
            return Err(AgwpeError::DisconnectedByOperator);
        }

        // Re-run the AX.25 handshake using the same helpers as handle_ax25_connect.
        ax25_open_and_await_connected(bg, state, log_tx).await?;
        if bg.abort_reconnect.load(Ordering::SeqCst) {
            return Err(AgwpeError::DisconnectedByOperator);
        }

        let skip_bpq_app = {
            let s = state.lock_or_poisoned();
            s.config.skip_bpq_app
        };
        if !skip_bpq_app {
            bpq_send_app_command(bg, state, log_tx).await?;
            if bg.abort_reconnect.load(Ordering::SeqCst) {
                return Err(AgwpeError::DisconnectedByOperator);
            }
        } else {
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(
                    LogLevel::Info,
                    "BPQ",
                    "Skipping BPQ application command (direct connection mode)",
                ),
            );
        }
        bpq_await_callsign_prompt_and_send_callsign(bg, state, log_tx).await?;
        if bg.abort_reconnect.load(Ordering::SeqCst) {
            return Err(AgwpeError::DisconnectedByOperator);
        }
        let disclaimer = bpq_await_disclaimer(bg, state, log_tx).await?;
        if bg.abort_reconnect.load(Ordering::SeqCst) {
            return Err(AgwpeError::DisconnectedByOperator);
        }

        // Auto-consent check — exact-string equality only.
        let stored = {
            let s = state.lock_or_poisoned();
            s.last_agreed_disclaimer.clone()
        };
        if !matches_stored_disclaimer(&disclaimer, stored.as_deref()) {
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(
                    LogLevel::Info,
                    "PROTOCOL",
                    "Server disclaimer differs from stored consent; re-consent required",
                ),
            );
            BackgroundState::set_state(
                state,
                log_tx,
                ConnectionState::AwaitingConsent { disclaimer },
            );
            return Err(AgwpeError::NeedsReconsent);
        }

        // Disclaimer matches — send AGREE on the wire so the server logs it.
        // Disclaimer suppression is purely UI; the wire protocol always requires AGREE.
        if bg.abort_reconnect.load(Ordering::SeqCst) {
            return Err(AgwpeError::DisconnectedByOperator);
        }
        let agree_frame = AgwpeFrame::new(
            bg.agwpe_port,
            FrameType::SendData,
            &bg.local_callsign,
            &bg.remote_callsign,
            b"AGREE\n".to_vec(),
        );
        BackgroundState::push_log(
            state,
            log_tx,
            DebugLogEntry::new(LogLevel::Info, "BPQ", "Auto-sending AGREE (matches stored consent)")
                .with_direction(Direction::Tx),
        );
        BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &agree_frame).await?;

        BackgroundState::set_state(state, log_tx, ConnectionState::Connected);
        BackgroundState::push_log(
            state,
            log_tx,
            DebugLogEntry::new(LogLevel::Info, "PROTOCOL", "Reconnect successful"),
        );
        Ok(())
    }
    .await;

    match result {
        Ok(()) => Ok(()),
        // NeedsReconsent already transitioned to AwaitingConsent — pass through.
        Err(AgwpeError::NeedsReconsent) => Err(AgwpeError::NeedsReconsent),
        // Any other error: transition to Error state so the UI shows a failure
        // instead of a spinner stuck on Reconnecting.
        Err(e) => {
            let msg = format!("Reconnect failed: {}", e);
            BackgroundState::push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
            );
            BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg));
            Err(e)
        }
    }
}

async fn handle_send_request(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    data: Vec<u8>,
) -> Result<Vec<u8>, AgwpeError> {
    if !bg.is_connected() {
        return Err(AgwpeError::NotConnected);
    }

    let chunk_size = 256;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Debug,
            "PROTOCOL",
            &format!("Sending {} bytes in {} byte chunks", data.len(), chunk_size),
        )
        .with_direction(Direction::Tx),
    );

    for (i, chunk) in data.chunks(chunk_size).enumerate() {
        let frame = AgwpeFrame::new(
            bg.agwpe_port,
            FrameType::SendData,
            &bg.local_callsign,
            &bg.remote_callsign,
            chunk.to_vec(),
        );

        BackgroundState::push_log(
            state,
            log_tx,
            DebugLogEntry::new(
                LogLevel::Trace,
                "PROTOCOL",
                &format!("Chunk {}/{}: {} bytes", i + 1, (data.len() + chunk_size - 1) / chunk_size, chunk.len()),
            )
            .with_direction(Direction::Tx),
        );

        BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &frame).await?;
    }

    let mut response_data = Vec::new();
    let mut expected_len: Option<u32> = None;
    let mut frame_start: usize = 0;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "PROTOCOL", "Waiting for response...")
            .with_direction(Direction::Rx),
    );

    let timeout_secs = bg.response_timeout_secs;
    loop {
        let frame = match bg.read_frame_with_timeout(timeout_secs).await {
            Ok(f) => f,
            Err(AgwpeError::Timeout) => {
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Info,
                        "PROTOCOL",
                        &format!("Response timed out after {}s — treating as SessionDied", timeout_secs),
                    )
                    .with_direction(Direction::Rx),
                );
                return Err(AgwpeError::SessionDied {
                    reason: format!("no response after {}s", timeout_secs),
                });
            }
            Err(e) => return Err(e),
        };

        match frame.frame_type {
            // Direwolf reports received data with frame type 'D' (0x44) — the
            // same byte we use to send data — so we accept both here.
            FrameType::Disconnect | FrameType::SendData => {
                if response_data.len() + frame.data.len() > MAX_RESPONSE_SIZE {
                    return Err(AgwpeError::InvalidFrame(format!(
                        "Response exceeded maximum size of {} bytes",
                        MAX_RESPONSE_SIZE
                    )));
                }
                response_data.extend_from_slice(&frame.data);

                if is_session_dead_payload(&response_data) {
                    BackgroundState::push_log(
                        state,
                        log_tx,
                        DebugLogEntry::new(
                            LogLevel::Info,
                            "PROTOCOL",
                            "Received AX.25 disconnect notification — treating as SessionDied",
                        )
                        .with_direction(Direction::Rx),
                    );
                    return Err(AgwpeError::SessionDied {
                        reason: "remote sent AX.25 disconnect notification".to_string(),
                    });
                }

                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Trace,
                        "PROTOCOL",
                        &format!("Received {} bytes (total: {})", frame.data_len, response_data.len()),
                    )
                    .with_direction(Direction::Rx),
                );

                // Look for the text-framed response header ("RESP<digit>
                // <base64_len>\n"). The Response::decode_header scans past
                // any leading garbage (banner text, echoed prompts) to find
                // the RESP magic.
                if expected_len.is_none() {
                    match packet_browser_shared::protocol::Response::decode_header(&response_data) {
                        Ok(Some((_status, b64_len, _etag, _max_age, header_end))) => {
                            if header_end > 0 {
                                let preview_len = header_end.min(128);
                                BackgroundState::push_log(
                                    state,
                                    log_tx,
                                    DebugLogEntry::new(
                                        LogLevel::Info,
                                        "PROTOCOL",
                                        &format!(
                                            "Framed header found at offset {}. Preceding bytes: {:?}",
                                            header_end,
                                            String::from_utf8_lossy(&response_data[..preview_len]),
                                        ),
                                    ),
                                );
                            }
                            expected_len = Some(b64_len);
                            frame_start = header_end;
                            BackgroundState::push_log(
                                state,
                                log_tx,
                                DebugLogEntry::new(
                                    LogLevel::Debug,
                                    "PROTOCOL",
                                    &format!(
                                        "Response header: base64_payload_size={}",
                                        b64_len,
                                    ),
                                ),
                            );
                        }
                        Ok(None) => {
                            // No complete header yet. If we've already seen
                            // the RESP magic, keep reading (the header
                            // terminator or length just hasn't arrived). If
                            // there's no magic and the buffer is huge, bail
                            // with a diagnostic dump.
                            let has_magic = response_data
                                .windows(packet_browser_shared::protocol::Response::MAGIC.len())
                                .any(|w| w == packet_browser_shared::protocol::Response::MAGIC);
                            if !has_magic && response_data.len() > 32 * 1024 {
                                let preview = response_data.len().min(256);
                                return Err(AgwpeError::SessionDied {
                                    reason: format!(
                                        "malformed response ({} bytes with no RESP magic: {:?})",
                                        response_data.len(),
                                        String::from_utf8_lossy(&response_data[..preview]),
                                    ),
                                });
                            }
                        }
                        Err(e) => {
                            return Err(AgwpeError::SessionDied {
                                reason: format!("malformed response header: {:?}", e),
                            });
                        }
                    }
                }

                if let Some(len) = expected_len {
                    if response_data.len() >= frame_start + len as usize {
                        BackgroundState::push_log(
                            state,
                            log_tx,
                            DebugLogEntry::new(
                                LogLevel::Debug,
                                "PROTOCOL",
                                &format!("Response complete: {} bytes", response_data.len()),
                            ),
                        );
                        return Ok(response_data);
                    }
                }
            }
            FrameType::ConnectionRejected => {
                return Err(AgwpeError::ConnectionFailed(
                    "Connection rejected during request".to_string(),
                ));
            }
            _ => {
                BackgroundState::push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Debug,
                        "PROTOCOL",
                        &format!("Ignoring frame during request: {:?}", frame.frame_type),
                    ),
                );
            }
        }
    }
}

async fn handle_send_request_with_reconnect(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    data: Vec<u8>,
) -> Result<Vec<u8>, AgwpeError> {
    match handle_send_request(bg, state, log_tx, data.clone()).await {
        Ok(bytes) => Ok(bytes),
        Err(AgwpeError::SessionDied { reason }) => {
            // Auto-reconnect kill-switch is enforced by the caller (proxy.rs)
            // choosing between send_request and send_request_with_reconnect,
            // so if we're here, retry is authorized.
            handle_reconnect(bg, state, log_tx, reason).await?;
            handle_send_request(bg, state, log_tx, data).await
        }
        Err(e) => Err(e),
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

    #[test]
    fn test_matches_stored_disclaimer() {
        let text = "All activity is logged including your callsign.\rType AGREE to proceed: ";
        assert!(matches_stored_disclaimer(text, Some(text)));

        // Different whitespace does NOT match.
        let differs_by_space = "All activity is logged including your callsign. \rType AGREE to proceed: ";
        assert!(!matches_stored_disclaimer(differs_by_space, Some(text)));

        // None never matches.
        assert!(!matches_stored_disclaimer(text, None));

        // Empty strings compare equal.
        assert!(matches_stored_disclaimer("", Some("")));
    }
}
