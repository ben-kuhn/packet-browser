use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, oneshot};

use crate::state::{
    ConnectionState, DebugLogEntry, Direction, LockExt, LogLevel, SharedState,
};
use crate::transport::agwpe::{is_session_dead_payload, AgwpeError};
use crate::transport::{Transport, TransportError, TransportEvent};

// Defensive caps against a hostile or buggy peer.
const MAX_RESPONSE_SIZE: usize = 10 * 1024 * 1024;
const MAX_HANDSHAKE_TEXT: usize = 64 * 1024;

pub struct SessionState {
    pub response_timeout_secs: u64,
    pub abort_reconnect: Arc<AtomicBool>,
}

impl SessionState {
    pub fn new(response_timeout_secs: u64) -> Self {
        Self {
            response_timeout_secs: response_timeout_secs.max(1),
            abort_reconnect: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn reset_abort(&self) {
        self.abort_reconnect.store(false, Ordering::SeqCst);
    }

    pub fn abort(&self) {
        self.abort_reconnect.store(true, Ordering::SeqCst);
    }

    pub fn is_aborted(&self) -> bool {
        self.abort_reconnect.load(Ordering::SeqCst)
    }
}

// ---------------------------------------------------------------------------
// Logging helpers shared by every session-driver call.
// ---------------------------------------------------------------------------

pub(crate) fn push_log(
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    entry: DebugLogEntry,
) {
    {
        let mut s = state.lock_or_poisoned();
        s.add_log(entry.clone());
    }
    let _ = log_tx.send(entry);
}

pub(crate) fn set_state(
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    cs: ConnectionState,
) {
    let entry = {
        let mut s = state.lock_or_poisoned();
        s.set_connection_state(cs)
    };
    let _ = log_tx.send(entry);
}

fn transport_err_to_agwpe(err: TransportError) -> AgwpeError {
    match err {
        TransportError::NotConnected => AgwpeError::NotConnected,
        TransportError::Timeout => AgwpeError::Timeout,
        TransportError::Io(e) => AgwpeError::Io(e),
        TransportError::ModemError(m) => AgwpeError::ConnectionFailed(m),
        TransportError::SessionRejected(m) => AgwpeError::ConnectionFailed(m),
    }
}

/// Returns `true` iff `server_text` is byte-for-byte equal to the stored
/// disclaimer. `None` stored means we have never seen a consent, so we always
/// return `false` — the operator must consent explicitly.
pub(crate) fn matches_stored_disclaimer(server_text: &str, stored: Option<&str>) -> bool {
    match stored {
        Some(s) => s == server_text,
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Session-level driver code. Speaks Transport, not AgwpeFrame.
// ---------------------------------------------------------------------------

/// Send an AX.25 Connect frame (via `Transport::open_ax25_link`) and loop
/// waiting for the link-open confirmation.  The transport already knows the
/// target from the preceding `open_session` call.
pub(crate) async fn ax25_open_and_await_connected(
    transport: &mut dyn Transport,
    _session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    transport.open_ax25_link().await.map_err(transport_err_to_agwpe)?;

    loop {
        let deadline = Instant::now() + Duration::from_secs(30);
        let event = transport
            .recv(deadline)
            .await
            .map_err(transport_err_to_agwpe)?;
        match event {
            TransportEvent::LinkOpened => {
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Info, "PROTOCOL", "AX.25 connected")
                        .with_direction(Direction::Rx),
                );
                return Ok(());
            }
            TransportEvent::LinkRejected { reason } => {
                let msg = format!("AX.25 connection rejected: {}", reason);
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Info, "ERROR", &msg)
                        .with_direction(Direction::Rx),
                );
                return Err(AgwpeError::ConnectionFailed(msg));
            }
            TransportEvent::Disconnected { reason } => {
                let msg = format!("AX.25 connect: peer disconnected: {}", reason);
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Info, "ERROR", &msg)
                        .with_direction(Direction::Rx),
                );
                return Err(AgwpeError::ConnectionFailed(msg));
            }
            TransportEvent::Data(_) => {
                // Ignore any in-flight banner text before the confirmation.
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Debug,
                        "PROTOCOL",
                        "Ignoring data frame while awaiting AX.25 connect",
                    ),
                );
            }
        }
    }
}

/// Send the BPQ application command (e.g. "WEB\n") over the current session.
/// Called only when `skip_bpq_app` is false.
pub(crate) async fn bpq_send_app_command(
    transport: &mut dyn Transport,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<(), AgwpeError> {
    let bpq_command = {
        let s = state.lock_or_poisoned();
        s.config.bpq_command.clone()
    };

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "BPQ",
            &format!("Starting BPQ handshake with command: {}", bpq_command),
        ),
    );

    let cmd_data = format!("{}\n", bpq_command);
    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", &format!("Sending BPQ command: {:?}", cmd_data))
            .with_direction(Direction::Tx),
    );

    transport
        .send(cmd_data.as_bytes())
        .await
        .map_err(transport_err_to_agwpe)?;
    Ok(())
}

/// Wait for the server's callsign prompt and send our local callsign in
/// response.  The prompt is identified by the presence of the word "callsign"
/// (or "AGREE", which some deployments fold into the same banner line).
pub(crate) async fn bpq_await_callsign_prompt_and_send_callsign(
    transport: &mut dyn Transport,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    local_callsign: &str,
) -> Result<(), AgwpeError> {
    let mut received_text = String::new();

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Waiting for callsign prompt...")
            .with_direction(Direction::Rx),
    );

    loop {
        let deadline = Instant::now() + Duration::from_secs(30);
        let event = transport
            .recv(deadline)
            .await
            .map_err(transport_err_to_agwpe)?;
        match event {
            TransportEvent::Data(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                if received_text.len() + text.len() > MAX_HANDSHAKE_TEXT {
                    return Err(AgwpeError::ConnectionFailed(
                        "Handshake text exceeded maximum size".to_string(),
                    ));
                }
                received_text.push_str(&text);
                if received_text.contains("callsign") || received_text.contains("AGREE") {
                    break;
                }
            }
            TransportEvent::LinkRejected { reason } => {
                return Err(AgwpeError::ConnectionFailed(format!(
                    "Connection rejected during BPQ handshake: {}",
                    reason
                )));
            }
            TransportEvent::Disconnected { reason } => {
                return Err(AgwpeError::ConnectionFailed(format!(
                    "Peer disconnected during BPQ handshake: {}",
                    reason
                )));
            }
            TransportEvent::LinkOpened => {
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Debug, "BPQ", "Ignoring LinkOpened during handshake"),
                );
            }
        }
    }

    let call_data = format!("{}\n", local_callsign);
    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", &format!("Sending callsign: {:?}", call_data))
            .with_direction(Direction::Tx),
    );
    transport
        .send(call_data.as_bytes())
        .await
        .map_err(transport_err_to_agwpe)?;
    Ok(())
}

/// Wait for the server's logging-disclaimer / AGREE prompt and return the raw
/// disclaimer text (byte-for-byte, no trim or normalisation).  The caller
/// decides what to do with it — either surface it for operator consent or
/// compare it against a stored value for auto-consent.
pub(crate) async fn bpq_await_disclaimer(
    transport: &mut dyn Transport,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
) -> Result<String, AgwpeError> {
    let mut received_text = String::new();

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Waiting for AGREE prompt...")
            .with_direction(Direction::Rx),
    );

    loop {
        let deadline = Instant::now() + Duration::from_secs(30);
        let event = transport
            .recv(deadline)
            .await
            .map_err(transport_err_to_agwpe)?;
        match event {
            TransportEvent::Data(bytes) => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                if received_text.len() + text.len() > MAX_HANDSHAKE_TEXT {
                    return Err(AgwpeError::ConnectionFailed(
                        "Handshake text exceeded maximum size".to_string(),
                    ));
                }
                received_text.push_str(&text);

                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Trace, "BPQ", &format!("Received text: {:?}", text))
                        .with_direction(Direction::Rx),
                );

                if received_text.to_uppercase().contains("AGREE") {
                    break;
                }
            }
            TransportEvent::LinkRejected { reason } => {
                return Err(AgwpeError::ConnectionFailed(format!(
                    "Connection rejected during BPQ handshake: {}",
                    reason
                )));
            }
            TransportEvent::Disconnected { reason } => {
                return Err(AgwpeError::ConnectionFailed(format!(
                    "Peer disconnected during BPQ handshake: {}",
                    reason
                )));
            }
            TransportEvent::LinkOpened => {
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(LogLevel::Debug, "BPQ", "Ignoring LinkOpened during handshake"),
                );
            }
        }
    }

    Ok(received_text)
}

/// Full BPQ handshake driven off the Transport trait.  Blocks on operator
/// consent via SharedState.pending_consent when it needs a new AGREE.
pub(crate) async fn perform_bpq_handshake(
    transport: &mut dyn Transport,
    _session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    local_callsign: &str,
) -> Result<(), AgwpeError> {
    let skip_bpq_app = {
        let s = state.lock_or_poisoned();
        s.config.skip_bpq_app
    };

    if skip_bpq_app {
        push_log(
            state,
            log_tx,
            DebugLogEntry::new(
                LogLevel::Info,
                "BPQ",
                "Skipping BPQ application command (direct connection mode)",
            ),
        );
    } else {
        bpq_send_app_command(transport, state, log_tx).await?;
    }

    bpq_await_callsign_prompt_and_send_callsign(transport, state, log_tx, local_callsign).await?;

    let received_text = bpq_await_disclaimer(transport, state, log_tx).await?;

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
    let _ = log_tx.send(state_entry);
    let _ = log_tx.send(DebugLogEntry::new(
        LogLevel::Info,
        "BPQ",
        "Waiting for operator consent",
    ));

    let accepted = match tokio::time::timeout(Duration::from_secs(300), consent_rx).await {
        Ok(Ok(accepted)) => accepted,
        Ok(Err(_)) => false,
        Err(_) => {
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

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "BPQ", "Sending AGREE").with_direction(Direction::Tx),
    );
    transport
        .send(b"AGREE\n")
        .await
        .map_err(transport_err_to_agwpe)?;

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Info, "BPQ", "BPQ handshake completed successfully"),
    );

    Ok(())
}

/// Send a request over the current session and read back the response.  Uses
/// `session_state.response_timeout_secs` as a per-recv deadline; a timeout is
/// treated as SessionDied so the caller can decide whether to reconnect.
pub(crate) async fn handle_send_request(
    transport: &mut dyn Transport,
    session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    data: Vec<u8>,
) -> Result<Vec<u8>, AgwpeError> {
    let timeout_secs = session_state.response_timeout_secs;

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Debug,
            "PROTOCOL",
            &format!("Sending {} bytes", data.len()),
        )
        .with_direction(Direction::Tx),
    );

    transport.send(&data).await.map_err(transport_err_to_agwpe)?;

    let mut response_data: Vec<u8> = Vec::new();
    let mut expected_len: Option<u32> = None;
    let mut frame_start: usize = 0;

    push_log(
        state,
        log_tx,
        DebugLogEntry::new(LogLevel::Debug, "PROTOCOL", "Waiting for response...")
            .with_direction(Direction::Rx),
    );

    loop {
        let deadline = Instant::now() + Duration::from_secs(timeout_secs);
        let event = match transport.recv(deadline).await {
            Ok(e) => e,
            Err(TransportError::Timeout) => {
                push_log(
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
            Err(e) => return Err(transport_err_to_agwpe(e)),
        };

        match event {
            TransportEvent::Data(bytes) => {
                if response_data.len() + bytes.len() > MAX_RESPONSE_SIZE {
                    return Err(AgwpeError::InvalidFrame(format!(
                        "Response exceeded maximum size of {} bytes",
                        MAX_RESPONSE_SIZE
                    )));
                }
                let bytes_len = bytes.len();
                response_data.extend_from_slice(&bytes);

                if is_session_dead_payload(&response_data) {
                    push_log(
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

                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Trace,
                        "PROTOCOL",
                        &format!("Received {} bytes (total: {})", bytes_len, response_data.len()),
                    )
                    .with_direction(Direction::Rx),
                );

                if expected_len.is_none() {
                    match packet_browser_shared::protocol::Response::decode_header(&response_data) {
                        Ok(Some((_status, b64_len, _etag, _max_age, header_end))) => {
                            if header_end > 0 {
                                let preview_len = header_end.min(128);
                                push_log(
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
                            push_log(
                                state,
                                log_tx,
                                DebugLogEntry::new(
                                    LogLevel::Debug,
                                    "PROTOCOL",
                                    &format!("Response header: base64_payload_size={}", b64_len),
                                ),
                            );
                        }
                        Ok(None) => {
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
                        push_log(
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
            TransportEvent::Disconnected { reason } => {
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Info,
                        "PROTOCOL",
                        &format!("Peer disconnected mid-request — treating as SessionDied: {}", reason),
                    )
                    .with_direction(Direction::Rx),
                );
                return Err(AgwpeError::SessionDied { reason });
            }
            TransportEvent::LinkRejected { reason } => {
                return Err(AgwpeError::ConnectionFailed(format!(
                    "Connection rejected during request: {}",
                    reason
                )));
            }
            TransportEvent::LinkOpened => {
                push_log(
                    state,
                    log_tx,
                    DebugLogEntry::new(
                        LogLevel::Debug,
                        "PROTOCOL",
                        "Ignoring stray LinkOpened during request",
                    ),
                );
            }
        }
    }
}

/// Re-run the full AX.25 + BPQ + AGREE handshake for a session that died
/// unexpectedly.  Transitions to `Reconnecting` at entry and `Connected` on
/// successful auto-consent.  Returns `Err(NeedsReconsent)` when the server's
/// disclaimer text differs from the stored consent, leaving the state as
/// `AwaitingConsent` so the UI can open the consent modal on the operator's
/// next visit.
pub(crate) async fn handle_reconnect(
    transport: &mut dyn Transport,
    session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    reason: String,
    local_callsign: &str,
) -> Result<(), AgwpeError> {
    set_state(
        state,
        log_tx,
        ConnectionState::Reconnecting { reason: reason.clone() },
    );
    push_log(
        state,
        log_tx,
        DebugLogEntry::new(
            LogLevel::Info,
            "PROTOCOL",
            &format!("Session lost ({}); attempting reconnect", reason),
        ),
    );

    // reopen_modem_connection tears down the AX.25 link, drops+re-establishes
    // the TCP socket, and re-registers our callsign. For non-AGWPE transports
    // it's a no-op — they just leave the modem connection intact.
    let result: Result<(), AgwpeError> = async {
        transport
            .reopen_modem_connection()
            .await
            .map_err(transport_err_to_agwpe)?;

        // reopen_modem_connection flips state to AgwpeConnected on success;
        // put us back in Reconnecting so the UI doesn't flicker while we drive
        // the rest of the handshake.
        set_state(
            state,
            log_tx,
            ConnectionState::Reconnecting { reason: reason.clone() },
        );
        if session_state.is_aborted() {
            return Err(AgwpeError::DisconnectedByOperator);
        }

        ax25_open_and_await_connected(transport, session_state, state, log_tx).await?;
        if session_state.is_aborted() {
            return Err(AgwpeError::DisconnectedByOperator);
        }

        let skip_bpq_app = {
            let s = state.lock_or_poisoned();
            s.config.skip_bpq_app
        };
        if !skip_bpq_app {
            bpq_send_app_command(transport, state, log_tx).await?;
            if session_state.is_aborted() {
                return Err(AgwpeError::DisconnectedByOperator);
            }
        } else {
            push_log(
                state,
                log_tx,
                DebugLogEntry::new(
                    LogLevel::Info,
                    "BPQ",
                    "Skipping BPQ application command (direct connection mode)",
                ),
            );
        }
        bpq_await_callsign_prompt_and_send_callsign(transport, state, log_tx, local_callsign).await?;
        if session_state.is_aborted() {
            return Err(AgwpeError::DisconnectedByOperator);
        }
        let disclaimer = bpq_await_disclaimer(transport, state, log_tx).await?;
        if session_state.is_aborted() {
            return Err(AgwpeError::DisconnectedByOperator);
        }

        // Auto-consent check — exact-string equality only.
        let stored = {
            let s = state.lock_or_poisoned();
            s.last_agreed_disclaimer.clone()
        };
        if !matches_stored_disclaimer(&disclaimer, stored.as_deref()) {
            push_log(
                state,
                log_tx,
                DebugLogEntry::new(
                    LogLevel::Info,
                    "PROTOCOL",
                    "Server disclaimer differs from stored consent; re-consent required",
                ),
            );
            set_state(
                state,
                log_tx,
                ConnectionState::AwaitingConsent { disclaimer },
            );
            return Err(AgwpeError::NeedsReconsent);
        }

        if session_state.is_aborted() {
            return Err(AgwpeError::DisconnectedByOperator);
        }
        push_log(
            state,
            log_tx,
            DebugLogEntry::new(LogLevel::Info, "BPQ", "Auto-sending AGREE (matches stored consent)")
                .with_direction(Direction::Tx),
        );
        transport
            .send(b"AGREE\n")
            .await
            .map_err(transport_err_to_agwpe)?;

        set_state(state, log_tx, ConnectionState::Connected);
        push_log(
            state,
            log_tx,
            DebugLogEntry::new(LogLevel::Info, "PROTOCOL", "Reconnect successful"),
        );
        Ok(())
    }
    .await;

    match result {
        Ok(()) => Ok(()),
        Err(AgwpeError::NeedsReconsent) => Err(AgwpeError::NeedsReconsent),
        Err(e) => {
            let msg = format!("Reconnect failed: {}", e);
            push_log(
                state,
                log_tx,
                DebugLogEntry::new(LogLevel::Info, "ERROR", &msg),
            );
            set_state(state, log_tx, ConnectionState::Error(msg));
            Err(e)
        }
    }
}

pub(crate) async fn handle_send_request_with_reconnect(
    transport: &mut dyn Transport,
    session_state: &mut SessionState,
    state: &SharedState,
    log_tx: &broadcast::Sender<DebugLogEntry>,
    data: Vec<u8>,
    local_callsign: &str,
) -> Result<Vec<u8>, AgwpeError> {
    match handle_send_request(transport, session_state, state, log_tx, data.clone()).await {
        Ok(bytes) => Ok(bytes),
        Err(AgwpeError::SessionDied { reason }) => {
            handle_reconnect(transport, session_state, state, log_tx, reason, local_callsign).await?;
            handle_send_request(transport, session_state, state, log_tx, data).await
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
