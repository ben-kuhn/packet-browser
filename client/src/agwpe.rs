use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::sync::broadcast;

use crate::state::{
    ConnectionState, DebugLogEntry, Direction, LogLevel, PortInfo, SharedState,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    RegisterCallsign = 0x58,
    RegistrationResponse = 0x78,
    Connect = 0x43,
    Connected = 0x63,
    DataReceived = 0x64,
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
            0x64 => Ok(FrameType::DataReceived),
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
}

#[derive(Clone)]
pub struct AgwpeManager {
    command_tx: mpsc::Sender<AgwpeCommand>,
}

impl AgwpeManager {
    pub fn new(
        state: SharedState,
        log_tx: broadcast::Sender<DebugLogEntry>,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);

        tokio::spawn(async move {
            background_task(command_rx, state, log_tx).await;
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
}

struct BackgroundState {
    stream: Option<TcpStream>,
    local_callsign: String,
    remote_callsign: String,
    agwpe_port: u8,
    read_buf: Vec<u8>,
}

impl BackgroundState {
    fn new() -> Self {
        Self {
            stream: None,
            local_callsign: String::new(),
            remote_callsign: String::new(),
            agwpe_port: 0,
            read_buf: Vec::new(),
        }
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn push_log(state: &SharedState, log_tx: &broadcast::Sender<DebugLogEntry>, entry: DebugLogEntry) {
        {
            let mut s = state.lock().unwrap();
            s.add_log(entry.clone());
        }
        let _ = log_tx.send(entry);
    }

    fn set_state(state: &SharedState, _log_tx: &broadcast::Sender<DebugLogEntry>, cs: ConnectionState) {
        {
            let mut s = state.lock().unwrap();
            s.set_connection_state(cs);
        }
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
                let total = AgwpeFrame::HEADER_SIZE + data_len;
                if self.read_buf.len() >= total {
                    // Debug: print raw bytes
                    eprintln!("[AGWPE] Raw frame bytes (first 36): {:?}", &self.read_buf[..36.min(self.read_buf.len())]);
                    eprintln!("[AGWPE] Frame type byte at offset 4: 0x{:02X} ('{}')", 
                        self.read_buf[4], 
                        if self.read_buf[4] >= 32 && self.read_buf[4] < 127 { self.read_buf[4] as char } else { '?' });
                    if data_len > 0 {
                        eprintln!("[AGWPE] Frame data ({} bytes): {:?}", data_len, &self.read_buf[36..36+data_len.min(self.read_buf.len()-36)]);
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
            eprintln!("[AGWPE] Read {} bytes from stream", n);
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
) {
    let mut bg = BackgroundState::new();

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

    {
        let mut s = state.lock().unwrap();
        s.clear_ports();
        s.set_connection_state(ConnectionState::Disconnected);
    }

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
        let mut s = state.lock().unwrap();
        s.set_ports(ports);
    }

    Ok(())
}

async fn perform_bpq_handshake(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    let (bpq_command, skip_bpq_app) = {
        let s = state.lock().unwrap();
        (s.config.bpq_command.clone(), s.config.skip_bpq_app)
    };

    let callsign = bg.local_callsign.clone();

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
        BackgroundState::push_log(
            state,
            log_tx,
            DebugLogEntry::new(
                LogLevel::Info,
                "BPQ",
                &format!("Starting BPQ handshake with command: {}", bpq_command),
            ),
        );

        // Send BPQ command immediately (e.g., "WEB\n")
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
        eprintln!("[BPQ] Sent BPQ command frame");
    }

    // Wait for callsign prompt
    let mut received_text = String::new();
    let mut callsign_prompt_found = false;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Waiting for callsign prompt...")
            .with_direction(Direction::Rx),
    );
    eprintln!("[BPQ] Waiting for callsign prompt...");

    loop {
        let frame = bg.read_frame_with_timeout(30).await?;

        eprintln!("[BPQ] Received frame type {:?} (0x{:02X}), data_len={}, data={:?}", 
            frame.frame_type, frame.frame_type as u8, frame.data_len, 
            String::from_utf8_lossy(&frame.data[..frame.data_len.min(100) as usize]));

        match frame.frame_type {
            FrameType::DataReceived => {
                let text = String::from_utf8_lossy(&frame.data).to_string();
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

                // Look for callsign prompt (server sends "All activity is logged...")
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

    // Send callsign
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
    eprintln!("[BPQ] Sent callsign frame");

    // Wait for AGREE prompt
    received_text.clear();
    let mut agree_prompt_found = false;

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Waiting for AGREE prompt...")
            .with_direction(Direction::Rx),
    );
    eprintln!("[BPQ] Waiting for AGREE prompt...");

    loop {
        let frame = bg.read_frame_with_timeout(30).await?;

        eprintln!("[BPQ] Received frame type {:?} (0x{:02X}), data_len={}, data={:?}", 
            frame.frame_type, frame.frame_type as u8, frame.data_len, 
            String::from_utf8_lossy(&frame.data[..frame.data_len.min(100) as usize]));

        match frame.frame_type {
            FrameType::DataReceived => {
                let text = String::from_utf8_lossy(&frame.data).to_string();
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

    // Send "AGREE\n"
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

async fn handle_ax25_connect(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    target: &str,
    port_num: u8,
) -> Result<(), AgwpeError> {
    if !bg.is_connected() {
        return Err(AgwpeError::NotConnected);
    }

    bg.remote_callsign = target.to_string();
    bg.agwpe_port = port_num;

    {
        let mut s = state.lock().unwrap();
        s.set_connection_state(ConnectionState::Connecting);
        s.set_agwpe_port(port_num);
    }

    let connect_frame = AgwpeFrame::new(
        port_num,
        FrameType::Connect,
        &bg.local_callsign,
        target,
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

        eprintln!("[AGWPE] AX.25 connect: received frame type {:?} (0x{:02X}), data_len={}, data={:?}", 
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

                {
                    let mut s = state.lock().unwrap();
                    s.config.update_target(target);
                }

                // Perform BPQ handshake
                match perform_bpq_handshake(bg, state, log_tx).await {
                    Ok(()) => {
                        BackgroundState::set_state(state, log_tx, ConnectionState::Connected);
                        return Ok(());
                    }
                    Err(e) => {
                        let msg = format!("BPQ handshake failed: {}", e);
                        BackgroundState::push_log(
                            state,
                            log_tx,
                            DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
                        );
                        BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
                        return Err(AgwpeError::ConnectionFailed(msg));
                    }
                }
            }
            // LinBPQ sends 0x43 ('C') for both connect request AND connected notification
            // Check if this is actually a connected notification by looking at the data
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

                    {
                        let mut s = state.lock().unwrap();
                        s.config.update_target(target);
                    }

                    // Perform BPQ handshake
                    match perform_bpq_handshake(bg, state, log_tx).await {
                        Ok(()) => {
                            BackgroundState::set_state(state, log_tx, ConnectionState::Connected);
                            return Ok(());
                        }
                        Err(e) => {
                            let msg = format!("BPQ handshake failed: {}", e);
                            BackgroundState::push_log(
                                state,
                                log_tx,
                                DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
                            );
                            BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
                            return Err(AgwpeError::ConnectionFailed(msg));
                        }
                    }
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
                BackgroundState::set_state(state, log_tx, ConnectionState::Error(msg.clone()));
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

async fn handle_ax25_disconnect(
    bg: &mut BackgroundState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    if !bg.is_connected() {
        return Err(AgwpeError::NotConnected);
    }

    let disconnect_frame = AgwpeFrame::new(
        bg.agwpe_port,
        FrameType::SendData,
        &bg.local_callsign,
        &bg.remote_callsign,
        vec![],
    );

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Info, "PROTOCOL", "AX.25 disconnect")
            .with_direction(Direction::Tx),
    );

    let _ = BackgroundState::send_frame(bg.stream.as_mut().unwrap(), &disconnect_frame).await;

    BackgroundState::set_state(state, log_tx, ConnectionState::AgwpeConnected);

    Ok(())
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

    BackgroundState::push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "PROTOCOL", "Waiting for response...")
            .with_direction(Direction::Rx),
    );

    loop {
        let frame = bg.read_frame_with_timeout(120).await?;

        match frame.frame_type {
            FrameType::DataReceived => {
                response_data.extend_from_slice(&frame.data);

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

                if expected_len.is_none() && response_data.len() >= 5 {
                    let payload_len = u32::from_be_bytes([
                        response_data[1],
                        response_data[2],
                        response_data[3],
                        response_data[4],
                    ]);
                    expected_len = Some(payload_len);

                    BackgroundState::push_log(
                        state,
                        log_tx,
                        DebugLogEntry::new(
                            LogLevel::Debug,
                            "PROTOCOL",
                            &format!("Response header: status=0x{:02x}, payload_size={}", response_data[0], payload_len),
                        ),
                    );
                }

                if let Some(len) = expected_len {
                    if response_data.len() >= 5 + len as usize {
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
    fn test_frame_type_conversion() {
        assert_eq!(FrameType::try_from(0x58).unwrap(), FrameType::RegisterCallsign);
        assert_eq!(FrameType::try_from(0x78).unwrap(), FrameType::RegistrationResponse);
        assert_eq!(FrameType::try_from(0x43).unwrap(), FrameType::Connect);
        assert_eq!(FrameType::try_from(0x63).unwrap(), FrameType::Connected);
        assert_eq!(FrameType::try_from(0x64).unwrap(), FrameType::DataReceived);
        assert_eq!(FrameType::try_from(0x44).unwrap(), FrameType::SendData);
        assert_eq!(FrameType::try_from(0x52).unwrap(), FrameType::ConnectionRejected);
        assert_eq!(FrameType::try_from(0x47).unwrap(), FrameType::QueryPorts);
        assert_eq!(FrameType::try_from(0x67).unwrap(), FrameType::PortInfo);
        assert!(FrameType::try_from(0xFF).is_err());
    }
}
