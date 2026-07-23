use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};

use crate::state::{ConnectionState, DebugLogEntry, LockExt, LogLevel, SharedState};
use crate::transport::agwpe::AgwpeError;
use crate::transport::session::{
    self, ax25_open_and_await_connected, handle_send_request,
    handle_send_request_with_reconnect, perform_bpq_handshake, push_log, set_state, SessionState,
};
use crate::transport::Transport;

enum TransportCommand {
    ConnectModem {
        config: crate::transport::TransportConfig,
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
    /// Shared with the background actor's `SessionState`.  Set to `true` by
    /// `close_session` and `disconnect_modem` before enqueuing the command so
    /// an in-flight `SendRequestWithReconnect` can detect the preemption and
    /// bail out of its recv loop immediately.
    pub(crate) abort_flag: Arc<AtomicBool>,
}

impl TransportManager {
    /// Spawn the background actor, injecting the concrete transport.  The
    /// caller builds whichever `Box<dyn Transport>` it needs (e.g.
    /// `Box::new(AgwpeTransport::new())` for AGWPE, or a future
    /// `Box::new(VaraTransport::new())` for Task 6's VARA path) and passes it
    /// here so `TransportManager` itself stays transport-agnostic.
    pub fn spawn(
        transport: Box<dyn Transport>,
        state: SharedState,
        log_tx: broadcast::Sender<DebugLogEntry>,
        response_timeout_secs: u64,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::channel(32);
        let abort_flag = Arc::new(AtomicBool::new(false));
        let abort_flag_clone = abort_flag.clone();

        tokio::spawn(async move {
            background_task(command_rx, transport, state, log_tx, response_timeout_secs, abort_flag_clone).await;
        });

        Self { command_tx, abort_flag }
    }

    pub async fn connect_modem(
        &self,
        config: crate::transport::TransportConfig,
    ) -> Result<(), AgwpeError> {
        let (tx, rx) = oneshot::channel();
        self.command_tx
            .send(TransportCommand::ConnectModem {
                config,
                reply: tx,
            })
            .await
            .map_err(|_| AgwpeError::TaskStopped)?;
        rx.await.map_err(|_| AgwpeError::TaskStopped)?
    }

    pub async fn disconnect_modem(&self) -> Result<(), AgwpeError> {
        self.abort_flag.store(true, Ordering::SeqCst);
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
        self.abort_flag.store(true, Ordering::SeqCst);
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
    abort_flag: Arc<AtomicBool>,
) {
    // Clamp to at least 1s: a zero-second timeout would fire on every read,
    // instantly SessionDied-ing every request and looping through reconnects.
    let mut session_state = SessionState::new(response_timeout_secs, abort_flag);

    // Cache the local callsign so session::handle_reconnect and
    // handle_send_request_with_reconnect can pass it through the handshake
    // without re-reading it from SharedState on every request.
    let mut local_callsign = String::new();

    while let Some(cmd) = command_rx.recv().await {
        match cmd {
            TransportCommand::ConnectModem {
                config,
                reply,
            } => {
                session_state.reset_abort();
                let callsign = config.local_callsign.clone();
                let result = handle_connect_modem(
                    &mut *transport,
                    &state,
                    &log_tx,
                    config,
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
                let result = handle_close_session(&mut *transport, &mut session_state, &state, &log_tx).await;
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
    config: crate::transport::TransportConfig,
) -> Result<(), AgwpeError> {
    let callsign = config.local_callsign.clone();

    transport
        .connect_modem(&config)
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
    session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    // Signal the abort flag so that any reconnect loop that checks
    // `session_state.is_aborted()` can exit early.
    // NOTE: under the current serial actor model a CloseSession cannot
    // preempt an in-flight SendRequestWithReconnect — the abort flag is
    // scaffolding preserved for a future select!-based dispatch refactor
    // where commands could interleave.
    session_state.abort();

    match transport.close_session().await {
        Ok(()) => {
            set_state(state, log_tx, ConnectionState::AgwpeConnected);
            Ok(())
        }
        Err(e) => Err(transport_err_to_agwpe(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::create_shared_state;
    use crate::transport::{
        SessionConfig, Transport, TransportConfig, TransportError, TransportEvent,
    };
    use async_trait::async_trait;
    use std::time::Instant;

    /// Minimal no-op transport for unit tests.  All methods succeed immediately;
    /// `recv` returns `Disconnected` so `handle_send_request` doesn't loop.
    struct NullTransport;

    #[async_trait]
    impl Transport for NullTransport {
        async fn connect_modem(&mut self, _cfg: &TransportConfig) -> Result<(), TransportError> {
            Ok(())
        }
        async fn disconnect_modem(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn open_session(&mut self, _cfg: &SessionConfig) -> Result<(), TransportError> {
            Ok(())
        }
        async fn close_session(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn send(&mut self, _data: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv(&mut self, _deadline: Instant) -> Result<TransportEvent, TransportError> {
            Ok(TransportEvent::Disconnected {
                reason: "null transport".to_string(),
            })
        }
        fn port_query_supported(&self) -> bool {
            false
        }
    }

    /// Blocking transport for abort-preemption tests.  `recv` sleeps for 50 ms
    /// per call and then returns a single-byte Data payload so the recv loop in
    /// `handle_send_request` keeps spinning.  This gives the test a window to
    /// call `close_session` / `disconnect_modem` between two successive `recv`
    /// calls, exercising the `is_aborted()` check inside the Data arm.
    ///
    /// Note: the current serial actor cannot preempt a *blocking* `recv` call
    /// mid-sleep; it only sees the abort flag on the next trip through the Data
    /// arm.  A full select!-based dispatch refactor would lift that limitation,
    /// but is deferred.  50 ms is short enough that the 2 s test timeout is
    /// never at risk.
    struct BlockingTransport;

    #[async_trait]
    impl Transport for BlockingTransport {
        async fn connect_modem(&mut self, _cfg: &TransportConfig) -> Result<(), TransportError> {
            Ok(())
        }
        async fn disconnect_modem(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn open_session(&mut self, _cfg: &SessionConfig) -> Result<(), TransportError> {
            Ok(())
        }
        async fn close_session(&mut self) -> Result<(), TransportError> {
            Ok(())
        }
        async fn send(&mut self, _data: &[u8]) -> Result<(), TransportError> {
            Ok(())
        }
        async fn recv(&mut self, _deadline: Instant) -> Result<TransportEvent, TransportError> {
            // Sleep long enough for the test to call close_session/disconnect_modem
            // between consecutive recv calls, but short enough to stay well
            // within the 2-second assertion timeout.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            // Return a single zero byte — not a disconnect payload, not a valid
            // framed response — so handle_send_request loops back into recv and
            // checks is_aborted() on the next Data arrival.
            Ok(TransportEvent::Data(vec![0]))
        }
        fn port_query_supported(&self) -> bool {
            false
        }
    }

    fn make_manager() -> TransportManager {
        let state = create_shared_state(crate::config::FileConfig::default());
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        TransportManager::spawn(Box::new(NullTransport), state, log_tx, 5)
    }

    fn make_blocking_manager() -> TransportManager {
        let state = create_shared_state(crate::config::FileConfig::default());
        let (log_tx, _) = tokio::sync::broadcast::channel(16);
        TransportManager::spawn(Box::new(BlockingTransport), state, log_tx, 30)
    }

    /// Verify that `close_session()` preempts a queued `send_request_with_reconnect`
    /// by exercising the real abort path: the actor blocks inside `handle_send_request`
    /// on `recv`, the test then calls `close_session` (which sets `abort_flag = true`),
    /// and the next time `recv` returns `Data` the `is_aborted()` check fires and the
    /// send resolves as `Err(DisconnectedByOperator)`.
    #[tokio::test]
    async fn close_session_sets_abort_flag() {
        let manager = make_blocking_manager();
        assert!(!manager.abort_flag.load(Ordering::SeqCst));

        // Spawn send_request_with_reconnect in a separate task so we can race it
        // with close_session.  The actor will dequeue this, call transport.send()
        // (immediate), and then block inside recv() for 50 ms per call.
        let m = manager.clone();
        let send_handle = tokio::spawn(async move {
            m.send_request_with_reconnect(vec![1, 2, 3]).await
        });

        // Wait long enough that the actor is definitely inside recv() (one 50 ms
        // sleep), then call close_session to set abort_flag = true.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // close_session sets the abort flag on the shared Arc before enqueuing the
        // command — the actor will observe it on the next Data arm iteration.
        manager.close_session().await.unwrap();

        // The send task must resolve with DisconnectedByOperator within 2 s.
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send_handle,
        )
        .await
        .expect("send_request_with_reconnect did not resolve within 2 s — abort flag not observed")
        .expect("task panicked");

        assert!(
            matches!(result, Err(AgwpeError::DisconnectedByOperator)),
            "expected DisconnectedByOperator, got {:?}",
            result,
        );
        // Flag stays set — ConnectModem would reset it on the next fresh session.
        assert!(manager.abort_flag.load(Ordering::SeqCst));
    }

    /// Same preemption test exercising the `disconnect_modem` path.
    #[tokio::test]
    async fn disconnect_modem_sets_abort_flag() {
        let manager = make_blocking_manager();
        assert!(!manager.abort_flag.load(Ordering::SeqCst));

        let m = manager.clone();
        let send_handle = tokio::spawn(async move {
            m.send_request_with_reconnect(vec![4, 5, 6]).await
        });

        // Let the actor enter recv().
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // Set the abort flag by calling the public API.
        manager.disconnect_modem().await.unwrap();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            send_handle,
        )
        .await
        .expect("send_request_with_reconnect did not resolve within 2 s — abort flag not observed")
        .expect("task panicked");

        assert!(
            matches!(result, Err(AgwpeError::DisconnectedByOperator)),
            "expected DisconnectedByOperator, got {:?}",
            result,
        );
        assert!(manager.abort_flag.load(Ordering::SeqCst));
    }
}
