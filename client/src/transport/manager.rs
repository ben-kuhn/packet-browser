use tokio::sync::{broadcast, mpsc, oneshot};

use crate::state::{ConnectionState, DebugLogEntry, LockExt, LogLevel, SharedState};
use crate::transport::agwpe::{AgwpeError, AgwpeTransport};
use crate::transport::session::{
    self, ax25_open_and_await_connected, handle_send_request,
    handle_send_request_with_reconnect, perform_bpq_handshake, push_log, set_state, SessionState,
};
use crate::transport::Transport;

enum TransportCommand {
    ConnectModem {
        host: String,
        port: u16,
        callsign: String,
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    DisconnectModem {
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    QueryPorts {
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    OpenSession {
        target: String,
        port_num: u8,
        reply: oneshot::Sender<Result<(), AgwpeError>>,
    },
    CloseSession {
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

/// `TransportManager` is the public-facing actor handle used by main.rs and
/// proxy.rs.  Internally it owns a `Box<dyn Transport>` and drives the
/// session-level state machine via the free functions in `session.rs`.
#[derive(Clone)]
pub struct TransportManager {
    command_tx: mpsc::Sender<TransportCommand>,
}

impl TransportManager {
    pub fn new(
        state: SharedState,
        log_tx: broadcast::Sender<DebugLogEntry>,
        response_timeout_secs: u64,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);

        // For now we only build AgwpeTransport; a future ConfigureTransport
        // command will let main.rs pick between AGWPE and VARA at runtime.
        let mut transport = AgwpeTransport::new();
        transport.attach_state(state.clone(), log_tx.clone());
        let boxed: Box<dyn Transport> = Box::new(transport);

        tokio::spawn(async move {
            background_task(command_rx, boxed, state, log_tx, response_timeout_secs).await;
        });

        Self { command_tx }
    }

    pub async fn connect_modem(
        &self,
        host: String,
        port: u16,
        callsign: String,
    ) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::ConnectModem {
                host,
                port,
                callsign,
                reply: tx,
            })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn disconnect_modem(&self) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::DisconnectModem { reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn query_ports(&self) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::QueryPorts { reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn open_session(&self, target: String, port_num: u8) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::OpenSession {
                target,
                port_num,
                reply: tx,
            })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn close_session(&self) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::CloseSession { reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn send_request(&self, data: Vec<u8>) -> Result<Vec<u8>, AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::SendRequest { data, reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn send_request_with_reconnect(&self, data: Vec<u8>) -> Result<Vec<u8>, AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::SendRequestWithReconnect { data, reply: tx })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }
}

// ---------------------------------------------------------------------------
// Background task
// ---------------------------------------------------------------------------

async fn background_task(
    mut command_rx: mpsc::Receiver<TransportCommand>,
    mut transport: Box<dyn Transport>,
    state: SharedState,
    log_tx: broadcast::Sender<DebugLogEntry>,
    response_timeout_secs: u64,
) {
    // Clamp to at least 1s: a zero-second timeout would fire on every read,
    // instantly SessionDied-ing every request and looping through reconnects.
    let mut session_state = SessionState::new(response_timeout_secs);

    // Cache the local callsign so session::handle_reconnect and
    // handle_send_request_with_reconnect can pass it through the handshake
    // without re-reading it from SharedState on every request.
    let mut local_callsign = String::new();

    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            TransportCommand::ConnectModem {
                host,
                port,
                callsign,
                reply,
            } => {
                let result = handle_connect_modem(
                    &mut *transport,
                    &state,
                    &log_tx,
                    &host,
                    port,
                    &callsign,
                )
                .await;
                if result.is_ok() {
                    local_callsign = callsign;
                }
                let _ = reply.send(result);
            }
            TransportCommand::DisconnectModem { reply } => {
                let result = handle_disconnect_modem(&mut *transport, &state, &log_tx).await;
                let _ = reply.send(result);
            }
            TransportCommand::QueryPorts { reply } => {
                let result = handle_query_ports(&mut *transport, &state, &log_tx).await;
                let _ = reply.send(result);
            }
            TransportCommand::OpenSession {
                target,
                port_num,
                reply,
            } => {
                let result = handle_open_session(
                    &mut *transport,
                    &mut session_state,
                    &state,
                    &log_tx,
                    &target,
                    port_num,
                    &local_callsign,
                )
                .await;
                let _ = reply.send(result);
            }
            TransportCommand::CloseSession { reply } => {
                let result = handle_close_session(&mut *transport, &state, &log_tx).await;
                let _ = reply.send(result);
            }
            TransportCommand::SendRequest { data, reply } => {
                let result = handle_send_request(
                    &mut *transport,
                    &mut session_state,
                    &state,
                    &log_tx,
                    data,
                )
                .await;
                let _ = reply.send(result);
            }
            TransportCommand::SendRequestWithReconnect { data, reply } => {
                let result = handle_send_request_with_reconnect(
                    &mut *transport,
                    &mut session_state,
                    &state,
                    &log_tx,
                    data,
                    &local_callsign,
                )
                .await;
                let _ = reply.send(result);
            }
        }
    }

    session::push_log(
        &state,
        &log_tx,
        DebugLogEntry::new(LogLevel::Info, "STATE", "Background task shutting down"),
    );
}

async fn handle_connect_modem(
    transport: &mut dyn Transport,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    host: &str,
    port: u16,
    callsign: &str,
) -> Result<(), AgwpeError> {
    let cfg = crate::transport::TransportConfig {
        kind: crate::transport::TransportKind::Ax25,
        agwpe: crate::transport::AgwpeParams {
            host: host.to_string(),
            port,
        },
        vara: crate::transport::VaraParams {
            cmd_host: String::new(),
            cmd_port: 0,
            data_host: String::new(),
            data_port: 0,
            mode: crate::transport::VaraMode::Fm,
            bandwidth: crate::transport::VaraBandwidth::VNarrow,
        },
        local_callsign: callsign.to_string(),
    };

    transport
        .connect_modem(&cfg)
        .await
        .map_err(transport_err_to_agwpe)?;

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "AGWPE", &format!("Connected as {}", callsign)),
    );

    Ok(())
}

fn transport_err_to_agwpe(e: crate::transport::TransportError) -> AgwpeError {
    match e {
        crate::transport::TransportError::NotConnected => AgwpeError::NotConnected,
        crate::transport::TransportError::Timeout => AgwpeError::Timeout,
        crate::transport::TransportError::Io(io) => AgwpeError::Io(io),
        crate::transport::TransportError::ModemError(m) => AgwpeError::ConnectionFailed(m),
        crate::transport::TransportError::SessionRejected(m) => AgwpeError::ConnectionFailed(m),
    }
}

async fn handle_disconnect_modem(
    transport: &mut dyn Transport,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    transport
        .disconnect_modem()
        .await
        .map_err(transport_err_to_agwpe)?;

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Info, "AGWPE", "Modem disconnected"),
    );
    Ok(())
}

async fn handle_query_ports(
    transport: &mut dyn Transport,
    _state: &SharedState,
    _log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    if !transport.port_query_supported() {
        return Ok(());
    }
    transport
        .query_ports()
        .await
        .map_err(transport_err_to_agwpe)
}

async fn handle_open_session(
    transport: &mut dyn Transport,
    session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    target: &str,
    port_num: u8,
    local_callsign: &str,
) -> Result<(), AgwpeError> {
    session_state.reset_abort();

    // Configure the transport with the session target so subsequent send/recv
    // and open_ax25_link calls address the right peer.
    let bpq_command = {
        let s = state.lock_or_poisoned();
        s.config.bpq_command.clone()
    };
    let skip_bpq_app = {
        let s = state.lock_or_poisoned();
        s.config.skip_bpq_app
    };
    let session_cfg = crate::transport::SessionConfig {
        local_callsign: local_callsign.to_string(),
        remote_callsign: target.to_string(),
        bpq_command,
        skip_bpq_app,
        agwpe_port: port_num,
    };
    transport
        .open_session(&session_cfg)
        .await
        .map_err(transport_err_to_agwpe)?;

    let state_entry = {
        let mut s = state.lock_or_poisoned();
        let entry = s.set_connection_state(ConnectionState::Connecting);
        s.set_agwpe_port(port_num);
        entry
    };
    let _ = log_tx.send(state_entry);
    {
        let mut s = state.lock_or_poisoned();
        s.config.update_target(target);
    }

    ax25_open_and_await_connected(transport, session_state, state, log_tx).await?;

    match perform_bpq_handshake(transport, session_state, state, log_tx, local_callsign).await {
        Ok(()) => {
            set_state(state, log_tx, ConnectionState::Connected);
            Ok(())
        }
        Err(e) => {
            let msg = format!("BPQ handshake failed: {}", e);
            push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
            );
            set_state(state, log_tx, ConnectionState::Error(msg.clone()));
            Err(AgwpeError::ConnectionFailed(msg))
        }
    }
}

async fn handle_close_session(
    transport: &mut dyn Transport,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    match transport.close_session().await {
        Ok(()) => {
            set_state(state, log_tx, ConnectionState::AgwpeConnected);
            Ok(())
        }
        Err(e) => Err(transport_err_to_agwpe(e)),
    }
}
