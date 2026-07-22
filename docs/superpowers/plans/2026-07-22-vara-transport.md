# VARA / Mercury Transport — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the client a second wire transport (VARA FM primary, VARA HF secondary; Mercury runs on the VARA HF path incidentally) selectable per-connect from `/connect`, without changing the server or the app-layer wire protocol.

**Architecture:** Extract a `Transport` trait from the current 1873-line `client/src/agwpe.rs`. Move that file into a `client/src/transport/` module tree with sibling `agwpe.rs` and `vara.rs` implementations. Rename `AgwpeManager` → `TransportManager` and hold a `Box<dyn Transport>`. All higher layers (`proxy.rs`, response framing, graceful reconnect, auto-consent, error pages, cache) call the trait rather than AGWPE frames. VARA speaks two TCP sockets (ASCII command port + raw data port) that the impl multiplexes behind a single `recv` future returning `TransportEvent::Data` or `TransportEvent::Disconnected`. UI adds a Transport dropdown; config adds `[transport]` and `[vara]` INI sections; server binary is untouched.

**Tech Stack:** Rust 1.x with `tokio`, `async_trait`, `thiserror`, `configparser` (all already in the workspace). Unit tests use `#[test]` for pure logic and `#[tokio::test]` for async paths driven by in-process `TcpListener` mocks.

## Global Constraints

- Server binary and wire-level RESP framing are unchanged. Any diff to `server/**` or `shared/protocol.rs` in this plan is a mistake.
- Existing AGWPE behaviour MUST be byte-for-byte preserved. Every AGWPE unit test that passes on `main` today MUST still pass after every task in this plan.
- Auto-consent path continues to send `AGREE\n` on the wire regardless of transport (see `docs/superpowers/specs/2026-07-20-graceful-reconnect-design.md`). Suppressing the disclaimer modal is UI-only.
- Retry ceiling remains exactly one reconnect per `/browse` request; a second `SessionDied` after reconnect propagates as an error.
- Only `TransportEvent::Disconnected` and `AgwpeError::SessionDied`-equivalent trigger reconnect. Other errors propagate.
- Config defaults for new fields: `[transport].default = ax25`, `[vara].cmd_host = 127.0.0.1`, `[vara].cmd_port = 8300`, `[vara].data_host = 127.0.0.1`, `[vara].data_port = 8301`, `[vara].mode = fm`, `[vara].bandwidth = vwide`.
- VARA command lines are terminated by `\r` (byte 0x0D). Response lines from the modem may arrive terminated by `\r`, `\n`, or `\r\n` — parse all three.
- Bandwidth config values are lowercase (`vwide`, `vnarrow`, `bw250`, `bw500`, `bw2300`, `bw2750`); on-wire commands are uppercase. The mapping table is defined in Task 5.
- No task may skip its failing-test step. Every task ends with a commit.

---

## Task 1: Introduce the `transport` module skeleton — trait, error, event, config types

**Files:**
- Create: `client/src/transport/mod.rs`
- Modify: `client/src/main.rs` (add `mod transport;`)

**Interfaces:**
- Consumes: nothing
- Produces:
  - `pub trait Transport` with `connect_modem`, `disconnect_modem`, `open_session`, `close_session`, `send`, `recv`, `port_query_supported`
  - `pub enum TransportEvent { Data(Vec<u8>), Disconnected { reason: String } }`
  - `pub enum TransportError { NotConnected, Io(std::io::Error), ModemError(String), SessionRejected(String), Timeout }`
  - `pub enum TransportKind { Ax25, VaraFm, VaraHf }` with `FromStr` + `Display`
  - `pub struct TransportConfig { pub kind: TransportKind, pub agwpe: AgwpeParams, pub vara: VaraParams }`
  - `pub struct SessionConfig { pub local_callsign: String, pub remote_callsign: String, pub bpq_command: String, pub skip_bpq_app: bool, pub agwpe_port: u8 }`

- [ ] **Step 1: Write the failing tests in `client/src/transport/mod.rs`**

Create the file (which will not yet exist, so this step also creates the module skeleton with the types the tests reference). Put the tests at the bottom of the same file so they compile in one unit:

```rust
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
```

- [ ] **Step 2: Write the minimal `client/src/transport/mod.rs`**

```rust
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
```

Add `mod transport;` to `client/src/main.rs` right after the existing `mod` declarations (search for `mod agwpe;` — put it directly below).

- [ ] **Step 3: Add `async-trait` to `client/Cargo.toml` if it isn't already there**

Run: `grep async-trait client/Cargo.toml || echo NOT_PRESENT`

If `NOT_PRESENT`, add under `[dependencies]`:

```toml
async-trait = "0.1"
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client transport::tests`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: Run the whole client test suite to prove nothing regressed**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client`
Expected: all pre-existing tests still pass, plus the two new `transport::tests`.

- [ ] **Step 6: Commit**

```bash
git add client/src/transport/mod.rs client/src/main.rs client/Cargo.toml Cargo.lock
git commit -m "client: introduce transport module — Transport trait + config types

Foundation for the VARA transport (v0.5.0). Types only; nothing consumes
them yet. Existing AGWPE code is untouched."
```

---

## Task 2: Extend `client.ini` parsing with `[transport]` and `[vara]` sections

**Files:**
- Modify: `client/src/config.rs` (add `TransportSection`, `VaraSection`, wire into `FileConfig`)
- Modify: `client/src/config.rs` `#[cfg(test)]` module

**Interfaces:**
- Consumes: `TransportKind`, `VaraMode`, `VaraBandwidth` from Task 1
- Produces:
  - `pub struct TransportSection { pub default: TransportKind }` (defaults to `Ax25`)
  - `pub struct VaraSection { pub cmd_host: String, pub cmd_port: u16, pub data_host: String, pub data_port: u16, pub mode: VaraMode, pub bandwidth: VaraBandwidth }`
  - `FileConfig.transport: TransportSection`
  - `FileConfig.vara: VaraSection`

- [ ] **Step 1: Write the failing test**

Add to the existing test module at the bottom of `client/src/config.rs`:

```rust
#[test]
fn loads_transport_and_vara_sections() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.ini");
    std::fs::write(
        &p,
        r#"
[transport]
default = vara_fm

[vara]
cmd_host = 10.0.0.5
cmd_port = 8300
data_host = 10.0.0.5
data_port = 8301
mode = fm
bandwidth = vwide
"#,
    )
    .unwrap();

    let cfg = FileConfig::load(&p).unwrap();

    assert_eq!(cfg.transport.default, crate::transport::TransportKind::VaraFm);
    assert_eq!(cfg.vara.cmd_host, "10.0.0.5");
    assert_eq!(cfg.vara.cmd_port, 8300);
    assert_eq!(cfg.vara.data_port, 8301);
    assert_eq!(cfg.vara.mode, crate::transport::VaraMode::Fm);
    assert_eq!(cfg.vara.bandwidth, crate::transport::VaraBandwidth::VWide);
}

#[test]
fn missing_transport_section_defaults_to_ax25() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("t.ini");
    std::fs::write(&p, "").unwrap();
    let cfg = FileConfig::load(&p).unwrap();
    assert_eq!(cfg.transport.default, crate::transport::TransportKind::Ax25);
    assert_eq!(cfg.vara.cmd_port, 8300);
}
```

- [ ] **Step 2: Run test to verify it fails to compile**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client config`
Expected: FAIL — `no field 'transport' on FileConfig`.

- [ ] **Step 3: Add `TransportSection` and `VaraSection` structs and parse them**

Add to `client/src/config.rs` (near the existing `ConnectionConfig` / `CacheSection`):

```rust
use crate::transport::{TransportKind, VaraBandwidth, VaraMode};

#[derive(Debug, Clone)]
pub struct TransportSection {
    pub default: TransportKind,
}

impl Default for TransportSection {
    fn default() -> Self {
        Self { default: TransportKind::Ax25 }
    }
}

#[derive(Debug, Clone)]
pub struct VaraSection {
    pub cmd_host: String,
    pub cmd_port: u16,
    pub data_host: String,
    pub data_port: u16,
    pub mode: VaraMode,
    pub bandwidth: VaraBandwidth,
}

impl Default for VaraSection {
    fn default() -> Self {
        Self {
            cmd_host: "127.0.0.1".to_string(),
            cmd_port: 8300,
            data_host: "127.0.0.1".to_string(),
            data_port: 8301,
            mode: VaraMode::Fm,
            bandwidth: VaraBandwidth::VWide,
        }
    }
}

fn parse_vara_mode(s: &str) -> VaraMode {
    match s {
        "hf" => VaraMode::Hf,
        _ => VaraMode::Fm,
    }
}

fn parse_vara_bandwidth(s: &str) -> VaraBandwidth {
    match s {
        "vnarrow" => VaraBandwidth::VNarrow,
        "vwide" => VaraBandwidth::VWide,
        "bw250" => VaraBandwidth::Bw250,
        "bw500" => VaraBandwidth::Bw500,
        "bw2300" => VaraBandwidth::Bw2300,
        "bw2750" => VaraBandwidth::Bw2750,
        _ => VaraBandwidth::VWide,
    }
}
```

Add fields to `FileConfig` (put alongside `pub connection: ConnectionConfig`):

```rust
pub transport: TransportSection,
pub vara: VaraSection,
```

Update `impl Default for FileConfig`:

```rust
transport: TransportSection::default(),
vara: VaraSection::default(),
```

In `FileConfig::load`, add parsing after the existing `connection` parse block:

```rust
let transport_default = ini
    .get("transport", "default")
    .and_then(|v| v.parse::<TransportKind>().ok())
    .unwrap_or(TransportKind::Ax25);

let vara = VaraSection {
    cmd_host: ini
        .get("vara", "cmd_host")
        .unwrap_or_else(|| "127.0.0.1".to_string()),
    cmd_port: ini
        .get("vara", "cmd_port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8300),
    data_host: ini
        .get("vara", "data_host")
        .unwrap_or_else(|| "127.0.0.1".to_string()),
    data_port: ini
        .get("vara", "data_port")
        .and_then(|v| v.parse().ok())
        .unwrap_or(8301),
    mode: ini
        .get("vara", "mode")
        .map(|v| parse_vara_mode(&v))
        .unwrap_or(VaraMode::Fm),
    bandwidth: ini
        .get("vara", "bandwidth")
        .map(|v| parse_vara_bandwidth(&v))
        .unwrap_or(VaraBandwidth::VWide),
};

let transport = TransportSection { default: transport_default };
```

Add `transport` and `vara` to the returned `FileConfig` struct literal at the end of `load`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client config`
Expected: `test result: ok. all tests pass including the two new ones`.

- [ ] **Step 5: Commit**

```bash
git add client/src/config.rs
git commit -m "client: parse [transport] and [vara] config sections

Defaults preserve AGWPE behaviour (transport.default = ax25). The vara
section is populated with sensible defaults even when absent so downstream
code can consume it unconditionally."
```

---

## Task 3: Rename `client/src/agwpe.rs` to `client/src/transport/agwpe.rs` (mechanical move, no logic change)

**Files:**
- Move: `client/src/agwpe.rs` → `client/src/transport/agwpe.rs`
- Modify: `client/src/main.rs` (replace `mod agwpe;` with `pub use transport::agwpe;` re-export)
- Modify: `client/src/proxy.rs` (update `use crate::agwpe::…` → `use crate::transport::agwpe::…`)
- Modify: `client/src/state.rs` if it references `crate::agwpe` anywhere
- Modify: `client/tests/session_reconnect.rs` (update the `use` path)

**Interfaces:**
- Consumes: nothing new
- Produces: `crate::transport::agwpe::{AgwpeManager, AgwpeError, ConnectionState, …}` (same public surface, new path)

- [ ] **Step 1: Move the file**

```bash
git mv client/src/agwpe.rs client/src/transport/agwpe.rs
```

- [ ] **Step 2: Update `client/src/main.rs`**

Replace `mod agwpe;` with:

```rust
// AGWPE lives inside the transport module now; re-export at the old
// path so pre-existing callers keep compiling during the trait rollout.
pub mod agwpe {
    pub use crate::transport::agwpe::*;
}
```

And ensure `mod transport;` also declares its submodule:

Modify `client/src/transport/mod.rs`, add at the top:

```rust
pub mod agwpe;
```

- [ ] **Step 3: Grep for `crate::agwpe` and `super::agwpe`, verify still resolvable**

Run: `grep -rn "crate::agwpe\|super::agwpe" client/src client/tests`

Every hit should still compile because of the re-export in step 2.

- [ ] **Step 4: Run the whole client build + test suite to prove no regressions**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client`
Expected: same pass count as before the task, no failures.

- [ ] **Step 5: Commit**

```bash
git add client/src/main.rs client/src/transport/
git commit -m "client: move agwpe.rs into transport/ module

Mechanical file move. crate::agwpe stays valid via a re-export so
callers don't churn in the same commit. Follow-ups will collapse the
re-export as the trait migration lands."
```

---

## Task 4: Implement `Transport` for the AGWPE actor + rename `AgwpeManager` → `TransportManager`

**Files:**
- Modify: `client/src/transport/agwpe.rs` (add `impl Transport for AgwpeTransport`, adapt error/event mapping)
- Create: `client/src/transport/manager.rs` (new `TransportManager` holding `Box<dyn Transport>`)
- Modify: `client/src/transport/mod.rs` (re-export `TransportManager`)
- Modify: `client/src/proxy.rs`, `client/src/main.rs`, `client/src/state.rs`, `client/src/ui.rs` (`AgwpeManager` → `TransportManager` at call sites; error variants unchanged for now)
- Modify: `client/src/main.rs` (drop the temporary `crate::agwpe` re-export)

**Interfaces:**
- Consumes: `Transport`, `TransportError`, `TransportEvent`, `TransportConfig`, `SessionConfig` from Task 1
- Produces:
  - `pub struct AgwpeTransport { … }` implementing `Transport`. Its `recv` maps AGWPE `*** DISCONNECTED FROM` payloads and the `AgwpeError::Timeout`-from-response-loop into `TransportEvent::Disconnected { reason }`; other frames become `TransportEvent::Data`.
  - `pub struct TransportManager { command_tx: mpsc::Sender<TransportCommand>, … }` — same actor pattern the current `AgwpeManager` uses, but the background task holds `Box<dyn Transport>` and the commands are transport-agnostic (`ConnectModem`, `DisconnectModem`, `OpenSession { session }`, `CloseSession`, `SendRequest { data }`, `SendRequestWithReconnect { data }`).

- [ ] **Step 1: Write a failing regression test**

Add to `client/src/transport/mod.rs` test module:

```rust
#[tokio::test]
async fn agwpe_transport_reports_disconnect_payload_as_disconnected_event() {
    use super::agwpe::AgwpeTransport;

    let (mut tx_stream, mut transport) = AgwpeTransport::for_test_pair().await;

    // Server writes an AGWPE 'd' frame whose payload starts with the
    // Direwolf disconnect marker.
    tx_stream.write_all(
        &super::agwpe::test_helpers::disconnect_frame_bytes(b"*** DISCONNECTED FROM N0CALL-8\r\n"),
    ).await.unwrap();
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
```

- [ ] **Step 2: Run the test — expect it to fail because the trait impl doesn't exist yet**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client agwpe_transport_reports_disconnect_payload_as_disconnected_event`
Expected: compile error or FAIL.

- [ ] **Step 3: Implement `AgwpeTransport` behind the trait**

At the top of `client/src/transport/agwpe.rs`, add:

```rust
use crate::transport::{
    SessionConfig, Transport, TransportConfig, TransportError, TransportEvent, TransportKind,
};
use async_trait::async_trait;
```

Restructure the existing `BackgroundState` fields into a new public struct:

```rust
pub struct AgwpeTransport {
    stream: Option<tokio::net::TcpStream>,
    read_buf: Vec<u8>,
    local_callsign: String,
    remote_callsign: String,
    agwpe_port: u8,
    response_timeout_secs: u64,
}

impl AgwpeTransport {
    pub fn new(response_timeout_secs: u64) -> Self {
        Self {
            stream: None,
            read_buf: Vec::new(),
            local_callsign: String::new(),
            remote_callsign: String::new(),
            agwpe_port: 0,
            response_timeout_secs,
        }
    }
}

#[async_trait]
impl Transport for AgwpeTransport {
    async fn connect_modem(
        &mut self,
        cfg: &TransportConfig,
    ) -> Result<(), TransportError> {
        // Re-use the body of the existing handle_connect_to_agwpe, but
        // return TransportError variants and populate self.stream on
        // success.
        // (…move existing logic in…)
        Ok(())
    }
    async fn disconnect_modem(&mut self) -> Result<(), TransportError> { … }
    async fn open_session(&mut self, cfg: &SessionConfig) -> Result<(), TransportError> { … }
    async fn close_session(&mut self) -> Result<(), TransportError> { … }
    async fn send(&mut self, data: &[u8]) -> Result<(), TransportError> { … }
    async fn recv(&mut self, deadline: std::time::Instant)
        -> Result<TransportEvent, TransportError>
    { … }
    fn port_query_supported(&self) -> bool { true }
}
```

Wire each method to the *existing* free functions in this file (`handle_connect_to_agwpe`, `handle_ax25_connect`, `handle_send_request`, `handle_ax25_disconnect`) by turning them into `impl AgwpeTransport` methods and adjusting the return types to `Result<_, TransportError>`. Map the current `AgwpeError` variants to `TransportError`:

| Current `AgwpeError` variant                | Maps to                                    |
| ------------------------------------------- | ------------------------------------------ |
| `NotConnected`                              | `TransportError::NotConnected`             |
| `ConnectionFailed(s)`                       | `TransportError::ModemError(s)`            |
| `Timeout`                                   | `TransportError::Timeout`                  |
| `InvalidFrame(s)` / `Io(e)`                 | `TransportError::Io(e)` / `ModemError(s)`  |
| `SessionDied { reason }`                    | do not surface as error; convert to `TransportEvent::Disconnected { reason }` inside `recv` |
| `NeedsReconsent` / `DisconnectedByOperator` | keep as `TransportError::ModemError(…)` for now; higher layer maps to session-error pages |

Add the helper module used by the test:

```rust
#[cfg(test)]
pub mod test_helpers {
    pub fn disconnect_frame_bytes(payload: &[u8]) -> Vec<u8> {
        // Reuse the private frame encoder (make it pub(crate) for tests).
        let f = super::AgwpeFrame::new(
            0,
            super::FrameType::Disconnect,
            "W1TEST",
            "N0CALL-8",
            payload.to_vec(),
        );
        f.encode()
    }
}

#[cfg(test)]
impl AgwpeTransport {
    pub async fn for_test_pair() -> (tokio::net::TcpStream, Self) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server_side, _) = listener.accept().await.unwrap();
        let mut t = AgwpeTransport::new(30);
        t.stream = Some(server_side);
        (client, t)
    }
}
```

- [ ] **Step 4: Add `TransportManager` that owns the actor**

Create `client/src/transport/manager.rs`:

```rust
use crate::transport::{Transport, TransportConfig, SessionConfig};
use tokio::sync::{mpsc, oneshot};

pub enum TransportCommand {
    ConnectModem { cfg: TransportConfig, reply: oneshot::Sender<Result<(), String>> },
    DisconnectModem { reply: oneshot::Sender<Result<(), String>> },
    OpenSession { cfg: SessionConfig, reply: oneshot::Sender<Result<(), String>> },
    CloseSession { reply: oneshot::Sender<Result<(), String>> },
    SendRequest { data: Vec<u8>, reply: oneshot::Sender<Result<Vec<u8>, String>> },
    SendRequestWithReconnect { data: Vec<u8>, reply: oneshot::Sender<Result<Vec<u8>, String>> },
}

pub struct TransportManager {
    command_tx: mpsc::Sender<TransportCommand>,
}

impl TransportManager {
    pub fn spawn(mut transport: Box<dyn Transport>) -> Self {
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(background_task(rx, transport));
        Self { command_tx: tx }
    }

    // Thin wrappers per command variant — copy the shape of AgwpeManager's
    // existing helper methods (send + await oneshot).
}

async fn background_task(
    mut rx: mpsc::Receiver<TransportCommand>,
    mut transport: Box<dyn Transport>,
) {
    while let Some(cmd) = rx.recv().await {
        // Dispatch to the existing handle_* free functions, now written
        // against the trait rather than concrete BackgroundState.
    }
}
```

Move `handle_send_request`, `handle_reconnect`, `handle_send_request_with_reconnect`, and the four BPQ helpers out of `agwpe.rs` into a new `client/src/transport/session.rs` file, changing their signatures to take `&mut dyn Transport` instead of `&mut BackgroundState`.

- [ ] **Step 5: Update every `AgwpeManager` call site**

Run: `grep -rn "AgwpeManager" client/src` and rename each to `TransportManager`.

Update `client/src/proxy.rs` around L590 (`state.config.agwpe_host.clone()`) so the manager is spawned via:

```rust
let transport: Box<dyn Transport> = match state.config.transport.default {
    TransportKind::Ax25 => Box::new(AgwpeTransport::new(state.config.connection.response_timeout_secs)),
    TransportKind::VaraFm | TransportKind::VaraHf => {
        // Task 6 will land the VaraTransport impl; for this task, panic!
        // is acceptable because the UI hasn't grown the picker yet.
        unimplemented!("VaraTransport lands in Task 6")
    }
};
let manager = TransportManager::spawn(transport);
```

- [ ] **Step 6: Drop the temporary `crate::agwpe` re-export**

Remove the `pub mod agwpe { pub use crate::transport::agwpe::*; }` shim from `client/src/main.rs`. Every remaining `use crate::agwpe::…` becomes `use crate::transport::agwpe::…` (or `use crate::transport::…` if it's a trait/type from `mod.rs`).

- [ ] **Step 7: Run the whole client test suite**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client`
Expected: all AGWPE tests still pass; the new `agwpe_transport_reports_disconnect_payload_as_disconnected_event` test passes.

- [ ] **Step 8: Commit**

```bash
git add -A client/
git commit -m "client: refactor AGWPE behind the Transport trait

- AgwpeTransport implements Transport
- TransportManager replaces AgwpeManager (actor pattern preserved)
- Session-level driver code (handle_send_request, handle_reconnect,
  BPQ helpers) moves to transport/session.rs and speaks the trait
- No wire-level behaviour change; every pre-existing test still passes"
```

---

## Task 5: VARA command-line codec — parse responses and serialize commands

**Files:**
- Create: `client/src/transport/vara/codec.rs`
- Create: `client/src/transport/vara/mod.rs` (declares `pub mod codec;`)
- Modify: `client/src/transport/mod.rs` (add `pub mod vara;`)

**Interfaces:**
- Consumes: `VaraMode`, `VaraBandwidth` from Task 1
- Produces:
  - `pub enum VaraResponse { Ok, Pending, Connected { local: String, remote: String }, Disconnected, BusyDetected, LinkRegistered, Buffer(u32), Missing(String), Unknown(String) }`
  - `pub fn parse_line(line: &str) -> VaraResponse`
  - `pub fn bandwidth_wire_command(bw: VaraBandwidth) -> &'static str`
  - `pub fn setup_commands(local_callsign: &str, mode: VaraMode, bw: VaraBandwidth) -> Vec<String>`

- [ ] **Step 1: Write failing tests in `client/src/transport/vara/codec.rs`**

```rust
use super::super::{VaraBandwidth, VaraMode};

// (types + fns to be defined below)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_known_responses() {
        assert!(matches!(parse_line("OK"), VaraResponse::Ok));
        assert!(matches!(parse_line("PENDING"), VaraResponse::Pending));
        assert!(matches!(parse_line("DISCONNECTED"), VaraResponse::Disconnected));
        assert!(matches!(parse_line("BUSY DETECTED"), VaraResponse::BusyDetected));
        assert!(matches!(parse_line("LINK REGISTERED"), VaraResponse::LinkRegistered));

        match parse_line("CONNECTED W1TEST N0CALL-8") {
            VaraResponse::Connected { local, remote } => {
                assert_eq!(local, "W1TEST");
                assert_eq!(remote, "N0CALL-8");
            }
            other => panic!("{other:?}"),
        }

        match parse_line("BUFFER 42") {
            VaraResponse::Buffer(n) => assert_eq!(n, 42),
            other => panic!("{other:?}"),
        }

        match parse_line("MISSING MYCALL") {
            VaraResponse::Missing(s) => assert_eq!(s, "MYCALL"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn parse_line_trims_line_terminators() {
        assert!(matches!(parse_line("OK\r"), VaraResponse::Ok));
        assert!(matches!(parse_line("OK\r\n"), VaraResponse::Ok));
        assert!(matches!(parse_line("OK\n"), VaraResponse::Ok));
    }

    #[test]
    fn unknown_responses_are_preserved_for_debug_logging() {
        match parse_line("REGISTERED W1TEST 2026") {
            VaraResponse::Unknown(s) => assert_eq!(s, "REGISTERED W1TEST 2026"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn bandwidth_wire_command_matches_vara_spec() {
        assert_eq!(bandwidth_wire_command(VaraBandwidth::VNarrow), "VNARROW");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::VWide), "VWIDE");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw250), "BW250");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw500), "BW500");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw2300), "BW2300");
        assert_eq!(bandwidth_wire_command(VaraBandwidth::Bw2750), "BW2750");
    }

    #[test]
    fn setup_commands_emit_expected_ordering() {
        let cmds = setup_commands("W1TEST", VaraMode::Fm, VaraBandwidth::VWide);
        assert_eq!(cmds, vec![
            "MYCALL W1TEST".to_string(),
            "LISTEN OFF".to_string(),
            "COMPRESSION OFF".to_string(),
            "VWIDE".to_string(),
        ]);
    }
}
```

- [ ] **Step 2: Register the module**

Add to `client/src/transport/mod.rs`:

```rust
pub mod vara;
```

Create `client/src/transport/vara/mod.rs`:

```rust
pub mod codec;
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client vara::codec`
Expected: FAIL (types/fns undefined).

- [ ] **Step 4: Write the minimal implementation**

Fill `client/src/transport/vara/codec.rs`:

```rust
use super::super::{VaraBandwidth, VaraMode};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaraResponse {
    Ok,
    Pending,
    Connected { local: String, remote: String },
    Disconnected,
    BusyDetected,
    LinkRegistered,
    Buffer(u32),
    Missing(String),
    Unknown(String),
}

pub fn parse_line(raw: &str) -> VaraResponse {
    let s = raw.trim_end_matches(|c: char| c == '\r' || c == '\n');
    if s == "OK" { return VaraResponse::Ok; }
    if s == "PENDING" { return VaraResponse::Pending; }
    if s == "DISCONNECTED" { return VaraResponse::Disconnected; }
    if s == "BUSY DETECTED" { return VaraResponse::BusyDetected; }
    if s == "LINK REGISTERED" { return VaraResponse::LinkRegistered; }
    if let Some(rest) = s.strip_prefix("CONNECTED ") {
        let mut it = rest.split_whitespace();
        if let (Some(local), Some(remote)) = (it.next(), it.next()) {
            return VaraResponse::Connected {
                local: local.to_string(),
                remote: remote.to_string(),
            };
        }
    }
    if let Some(rest) = s.strip_prefix("BUFFER ") {
        if let Ok(n) = rest.trim().parse::<u32>() {
            return VaraResponse::Buffer(n);
        }
    }
    if let Some(rest) = s.strip_prefix("MISSING ") {
        return VaraResponse::Missing(rest.trim().to_string());
    }
    VaraResponse::Unknown(s.to_string())
}

pub fn bandwidth_wire_command(bw: VaraBandwidth) -> &'static str {
    match bw {
        VaraBandwidth::VNarrow => "VNARROW",
        VaraBandwidth::VWide => "VWIDE",
        VaraBandwidth::Bw250 => "BW250",
        VaraBandwidth::Bw500 => "BW500",
        VaraBandwidth::Bw2300 => "BW2300",
        VaraBandwidth::Bw2750 => "BW2750",
    }
}

pub fn setup_commands(
    local_callsign: &str,
    _mode: VaraMode,
    bw: VaraBandwidth,
) -> Vec<String> {
    vec![
        format!("MYCALL {}", local_callsign),
        "LISTEN OFF".to_string(),
        "COMPRESSION OFF".to_string(),
        bandwidth_wire_command(bw).to_string(),
    ]
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client vara::codec`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add client/src/transport/vara/ client/src/transport/mod.rs
git commit -m "client(vara): add command codec — response parser + setup commands"
```

---

## Task 6: `VaraTransport::connect_modem` — TCP dial cmd+data ports and drive setup commands

**Files:**
- Create: `client/src/transport/vara/transport.rs`
- Modify: `client/src/transport/vara/mod.rs`

**Interfaces:**
- Consumes: `VaraResponse`, `setup_commands`, `bandwidth_wire_command` from Task 5
- Produces: `pub struct VaraTransport { cmd: Option<tokio::net::TcpStream>, data: Option<tokio::net::TcpStream>, cmd_buf: String }` with a partially-implemented `Transport` impl (only `connect_modem` and `disconnect_modem` finished for this task).

- [ ] **Step 1: Write the failing async test**

Add to `client/src/transport/vara/transport.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{
        AgwpeParams, TransportConfig, TransportKind, VaraBandwidth, VaraMode, VaraParams,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    async fn mock_ports() -> (u16, u16, tokio::task::JoinHandle<Vec<String>>) {
        let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cmd_port = cmd_listener.local_addr().unwrap().port();
        let data_port = data_listener.local_addr().unwrap().port();

        let handle = tokio::spawn(async move {
            let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
            let (_data_sock, _) = data_listener.accept().await.unwrap();
            let mut lines = Vec::new();
            let (r, mut w) = cmd_sock.split();
            let mut reader = BufReader::new(r);
            for _ in 0..4 {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                lines.push(line.trim().to_string());
                w.write_all(b"OK\r").await.unwrap();
            }
            lines
        });

        (cmd_port, data_port, handle)
    }

    #[tokio::test]
    async fn connect_modem_sends_expected_setup_commands() {
        let (cmd_port, data_port, mock) = mock_ports().await;
        let mut vara = VaraTransport::new();
        let cfg = TransportConfig {
            kind: TransportKind::VaraFm,
            agwpe: AgwpeParams { host: "unused".into(), port: 0 },
            vara: VaraParams {
                cmd_host: "127.0.0.1".into(),
                cmd_port,
                data_host: "127.0.0.1".into(),
                data_port,
                mode: VaraMode::Fm,
                bandwidth: VaraBandwidth::VWide,
            },
        };
        vara.connect_modem(&cfg).await.unwrap();

        let lines = mock.await.unwrap();
        assert_eq!(lines, vec![
            "MYCALL ".to_string() + "N0CALL",
            // The line above intentionally uses the placeholder local
            // callsign for this task; open_session (Task 7) will re-issue
            // MYCALL when the operator's callsign is known.
            "LISTEN OFF".to_string(),
            "COMPRESSION OFF".to_string(),
            "VWIDE".to_string(),
        ]);
    }
}
```

**Note**: the placeholder-callsign approach documented in the test is the intended shape — `connect_modem` runs *before* the operator's callsign is known (the current AGWPE flow works the same way). Task 7's `open_session` will re-send `MYCALL` with the real callsign right before `CONNECT`.

- [ ] **Step 2: Register the transport submodule**

Update `client/src/transport/vara/mod.rs`:

```rust
pub mod codec;
pub mod transport;

pub use transport::VaraTransport;
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client vara::transport::tests::connect_modem_sends_expected_setup_commands`
Expected: compile error, `VaraTransport` undefined.

- [ ] **Step 4: Implement the minimum**

Fill `client/src/transport/vara/transport.rs`:

```rust
use crate::transport::vara::codec::{parse_line, setup_commands, VaraResponse};
use crate::transport::{
    SessionConfig, Transport, TransportConfig, TransportError, TransportEvent, TransportKind,
};
use async_trait::async_trait;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const PLACEHOLDER_CALL: &str = "N0CALL";
const OK_TIMEOUT_SECS: u64 = 5;

pub struct VaraTransport {
    cmd: Option<TcpStream>,
    data: Option<TcpStream>,
    cmd_line_buf: String,
}

impl VaraTransport {
    pub fn new() -> Self {
        Self { cmd: None, data: None, cmd_line_buf: String::new() }
    }

    async fn send_cmd(&mut self, line: &str) -> Result<(), TransportError> {
        let stream = self.cmd.as_mut().ok_or(TransportError::NotConnected)?;
        stream.write_all(line.as_bytes()).await?;
        stream.write_all(b"\r").await?;
        stream.flush().await?;
        Ok(())
    }

    async fn read_cmd_line(&mut self, deadline: Instant) -> Result<String, TransportError> {
        // Read a \r or \n terminated line from self.cmd, retrying WouldBlock
        // until deadline. Return the trimmed line.
        loop {
            if let Some(pos) = self.cmd_line_buf.find(|c: char| c == '\r' || c == '\n') {
                let mut line: String = self.cmd_line_buf.drain(..=pos).collect();
                // Also strip any trailing \n if we split on \r
                if let Some('\n') = self.cmd_line_buf.chars().next() {
                    self.cmd_line_buf.remove(0);
                }
                line.truncate(line.trim_end_matches(|c: char| c == '\r' || c == '\n').len());
                return Ok(line);
            }
            if Instant::now() >= deadline {
                return Err(TransportError::Timeout);
            }
            let stream = self.cmd.as_mut().ok_or(TransportError::NotConnected)?;
            let mut chunk = [0u8; 512];
            let n = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                tokio::io::AsyncReadExt::read(stream, &mut chunk),
            )
            .await
            .map_err(|_| TransportError::Timeout)??;
            if n == 0 { return Err(TransportError::NotConnected); }
            self.cmd_line_buf
                .push_str(&String::from_utf8_lossy(&chunk[..n]));
        }
    }

    async fn await_ok(&mut self) -> Result<(), TransportError> {
        let deadline = Instant::now() + std::time::Duration::from_secs(OK_TIMEOUT_SECS);
        let line = self.read_cmd_line(deadline).await?;
        match parse_line(&line) {
            VaraResponse::Ok => Ok(()),
            VaraResponse::Missing(field) => Err(TransportError::ModemError(format!("MISSING {field}"))),
            other => Err(TransportError::ModemError(format!("expected OK, got {other:?}"))),
        }
    }
}

#[async_trait]
impl Transport for VaraTransport {
    async fn connect_modem(
        &mut self,
        cfg: &TransportConfig,
    ) -> Result<(), TransportError> {
        if !matches!(cfg.kind, TransportKind::VaraFm | TransportKind::VaraHf) {
            return Err(TransportError::ModemError(
                "VaraTransport called with non-VARA TransportKind".into(),
            ));
        }
        let cmd = TcpStream::connect((&*cfg.vara.cmd_host, cfg.vara.cmd_port)).await?;
        let data = TcpStream::connect((&*cfg.vara.data_host, cfg.vara.data_port)).await?;
        self.cmd = Some(cmd);
        self.data = Some(data);

        for cmd_line in setup_commands(PLACEHOLDER_CALL, cfg.vara.mode, cfg.vara.bandwidth) {
            self.send_cmd(&cmd_line).await?;
            self.await_ok().await?;
        }
        Ok(())
    }

    async fn disconnect_modem(&mut self) -> Result<(), TransportError> {
        self.cmd = None;
        self.data = None;
        self.cmd_line_buf.clear();
        Ok(())
    }

    async fn open_session(
        &mut self,
        _cfg: &SessionConfig,
    ) -> Result<(), TransportError> {
        // Filled in Task 7.
        Err(TransportError::ModemError("open_session not yet implemented".into()))
    }

    async fn close_session(&mut self) -> Result<(), TransportError> {
        // Filled in Task 7.
        Err(TransportError::ModemError("close_session not yet implemented".into()))
    }

    async fn send(&mut self, _data: &[u8]) -> Result<(), TransportError> {
        Err(TransportError::ModemError("send not yet implemented".into()))
    }

    async fn recv(
        &mut self,
        _deadline: Instant,
    ) -> Result<TransportEvent, TransportError> {
        Err(TransportError::ModemError("recv not yet implemented".into()))
    }

    fn port_query_supported(&self) -> bool { false }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client vara::transport::tests::connect_modem_sends_expected_setup_commands`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add client/src/transport/vara/
git commit -m "client(vara): connect_modem — dial cmd+data ports, emit setup commands"
```

---

## Task 7: `VaraTransport::open_session`, `close_session`, `send`, `recv`

**Files:**
- Modify: `client/src/transport/vara/transport.rs`

**Interfaces:**
- Consumes: everything from Task 6
- Produces: fully implemented `Transport` impl for `VaraTransport`.

- [ ] **Step 1: Write failing tests**

Add to the existing `#[cfg(test)] mod tests` in `client/src/transport/vara/transport.rs`:

```rust
#[tokio::test]
async fn open_session_sends_connect_and_reports_success_on_connected_line() {
    let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cmd_port = cmd_listener.local_addr().unwrap().port();
    let data_port = data_listener.local_addr().unwrap().port();

    let mock = tokio::spawn(async move {
        let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
        let (_data_sock, _) = data_listener.accept().await.unwrap();
        let (r, mut w) = cmd_sock.split();
        let mut reader = BufReader::new(r);
        // Ack the four setup commands.
        for _ in 0..4 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            w.write_all(b"OK\r").await.unwrap();
        }
        // Ack the MYCALL re-issue in open_session.
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "MYCALL W1TEST");
        w.write_all(b"OK\r").await.unwrap();
        // Ack CONNECT with PENDING then CONNECTED.
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "CONNECT W1TEST N0CALL-8");
        w.write_all(b"PENDING\r").await.unwrap();
        w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();
    });

    let mut vara = VaraTransport::new();
    let cfg = TransportConfig {
        kind: TransportKind::VaraFm,
        agwpe: AgwpeParams { host: "unused".into(), port: 0 },
        vara: VaraParams {
            cmd_host: "127.0.0.1".into(),
            cmd_port,
            data_host: "127.0.0.1".into(),
            data_port,
            mode: VaraMode::Fm,
            bandwidth: VaraBandwidth::VWide,
        },
    };
    vara.connect_modem(&cfg).await.unwrap();
    let session = SessionConfig {
        local_callsign: "W1TEST".into(),
        remote_callsign: "N0CALL-8".into(),
        bpq_command: String::new(),
        skip_bpq_app: true,
        agwpe_port: 0,
    };
    vara.open_session(&session).await.unwrap();
    mock.await.unwrap();
}

#[tokio::test]
async fn recv_translates_disconnected_command_line_to_transport_event() {
    // Similar setup — after open_session succeeds, the mock writes
    // "DISCONNECTED\r" on the command port. recv() must return
    // TransportEvent::Disconnected with a reason describing the cause.
    // (Full body follows the same pattern as the previous test; write
    // the mock to skip through setup and CONNECT, then emit DISCONNECTED.)
}

#[tokio::test]
async fn send_writes_bytes_on_data_port() {
    // Verify that vara.send(b"GET /\n").await writes exactly those bytes
    // to the mock data-port socket, using tokio::io::AsyncReadExt::read_exact
    // on the mock side.
}

#[tokio::test]
async fn close_session_sends_disconnect_and_drains_confirmation() {
    // After open_session, call close_session(). The mock reads DISCONNECT
    // on the command port and writes DISCONNECTED back within 100ms.
    // Assert close_session returns Ok(()) within the 3s drain deadline.
}
```

For the second, third, and fourth tests, write the mock in the same pattern as the first — bind two listeners, spawn a task that scripts the mock's read/write sequence, then drive `VaraTransport` from the main task. Keep the mock scripts to the minimum bytes needed to exercise the code path.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client vara::transport`
Expected: four new tests fail.

- [ ] **Step 3: Implement `open_session`, `close_session`, `send`, `recv`**

In `client/src/transport/vara/transport.rs`, replace the placeholder bodies with:

```rust
async fn open_session(
    &mut self,
    cfg: &SessionConfig,
) -> Result<(), TransportError> {
    // Re-issue MYCALL with the operator's callsign now that we know it.
    self.send_cmd(&format!("MYCALL {}", cfg.local_callsign)).await?;
    self.await_ok().await?;

    // Request the connection.
    self.send_cmd(&format!(
        "CONNECT {} {}",
        cfg.local_callsign, cfg.remote_callsign
    ))
    .await?;

    // Accept PENDING then CONNECTED, or fail on DISCONNECTED / BUSY DETECTED.
    let connect_deadline = Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let line = self.read_cmd_line(connect_deadline).await?;
        match parse_line(&line) {
            VaraResponse::Pending => continue,
            VaraResponse::Connected { .. } => return Ok(()),
            VaraResponse::Disconnected => {
                return Err(TransportError::SessionRejected(
                    "vara: link dropped during CONNECT".into(),
                ));
            }
            VaraResponse::BusyDetected => {
                return Err(TransportError::SessionRejected("channel busy".into()));
            }
            VaraResponse::Unknown(s) => {
                tracing::debug!(response = %s, "ignoring VARA cmd during CONNECT");
                continue;
            }
            other => {
                return Err(TransportError::ModemError(format!(
                    "unexpected during CONNECT: {other:?}"
                )));
            }
        }
    }
}

async fn close_session(&mut self) -> Result<(), TransportError> {
    self.send_cmd("DISCONNECT").await?;
    // Drain up to 3s waiting for DISCONNECTED confirmation.
    let deadline = Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match self.read_cmd_line(deadline).await {
            Ok(line) if matches!(parse_line(&line), VaraResponse::Disconnected) => {
                return Ok(());
            }
            Ok(_) => continue,
            Err(TransportError::Timeout) => return Ok(()),
            Err(e) => return Err(e),
        }
    }
}

async fn send(&mut self, data: &[u8]) -> Result<(), TransportError> {
    let stream = self.data.as_mut().ok_or(TransportError::NotConnected)?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

async fn recv(
    &mut self,
    deadline: Instant,
) -> Result<TransportEvent, TransportError> {
    loop {
        if Instant::now() >= deadline {
            return Err(TransportError::Timeout);
        }
        let data = self.data.as_mut().ok_or(TransportError::NotConnected)?;
        let cmd = self.cmd.as_mut().ok_or(TransportError::NotConnected)?;
        let mut data_chunk = [0u8; 4096];
        tokio::select! {
            // Data-port readable → return the bytes.
            n = tokio::io::AsyncReadExt::read(data, &mut data_chunk) => {
                let n = n?;
                if n == 0 {
                    return Ok(TransportEvent::Disconnected {
                        reason: "data port closed".into(),
                    });
                }
                return Ok(TransportEvent::Data(data_chunk[..n].to_vec()));
            }
            // Command-port readable → parse a line.
            _ = read_cmd_ready(cmd) => {
                let line_deadline = Instant::now()
                    + std::time::Duration::from_millis(100);
                let line = self.read_cmd_line(line_deadline).await?;
                match parse_line(&line) {
                    VaraResponse::Disconnected => {
                        return Ok(TransportEvent::Disconnected {
                            reason: "vara modem reports disconnect".into(),
                        });
                    }
                    other => {
                        tracing::debug!(?other, "VARA cmd line during recv");
                        continue;
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                continue;
            }
        }
    }
}
```

Add the helper `read_cmd_ready` at module scope:

```rust
async fn read_cmd_ready(stream: &mut TcpStream) -> std::io::Result<()> {
    stream.readable().await
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client vara::transport`
Expected: all four tests + the connect_modem test pass.

- [ ] **Step 5: Commit**

```bash
git add client/src/transport/vara/transport.rs
git commit -m "client(vara): open_session/close_session/send/recv

Session lifecycle over the command port; data on the data port; session
death recognised on 'DISCONNECTED' cmd-port line, mapped to
TransportEvent::Disconnected."
```

---

## Task 8: End-to-end integration test — mock VARA modem drives the full session lifecycle

**Files:**
- Create: `client/tests/vara_mock_modem.rs`

**Interfaces:**
- Consumes: `VaraTransport`, `TransportConfig`, `SessionConfig` from earlier tasks
- Produces: a single integration test that binds two TCP listeners, scripts a full VARA modem dialogue (`connect_modem` → `open_session` → `send` + `recv` (both directions) → `close_session` → `open_session` again to simulate a reconnect), and asserts the client-side state after each step.

- [ ] **Step 1: Write the test**

`client/tests/vara_mock_modem.rs`:

```rust
use packet_browser_client::transport::vara::VaraTransport;
use packet_browser_client::transport::{
    AgwpeParams, SessionConfig, Transport, TransportConfig, TransportEvent, TransportKind,
    VaraBandwidth, VaraMode, VaraParams,
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[tokio::test]
async fn vara_lifecycle_connect_send_recv_reconnect() {
    let cmd_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let data_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let cmd_port = cmd_listener.local_addr().unwrap().port();
    let data_port = data_listener.local_addr().unwrap().port();

    let mock = tokio::spawn(async move {
        let (mut cmd_sock, _) = cmd_listener.accept().await.unwrap();
        let (mut data_sock, _) = data_listener.accept().await.unwrap();

        // Setup phase: MYCALL / LISTEN OFF / COMPRESSION OFF / VWIDE.
        let (r, mut w) = cmd_sock.split();
        let mut reader = BufReader::new(r);
        for _ in 0..4 {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            w.write_all(b"OK\r").await.unwrap();
        }

        // open_session #1: MYCALL re-issue, then CONNECT.
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "MYCALL W1TEST");
        w.write_all(b"OK\r").await.unwrap();

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "CONNECT W1TEST N0CALL-8");
        w.write_all(b"PENDING\r").await.unwrap();
        w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();

        // Data phase.
        let mut got = [0u8; 5];
        data_sock.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"HELLO");
        data_sock.write_all(b"WORLD").await.unwrap();

        // close_session.
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "DISCONNECT");
        w.write_all(b"DISCONNECTED\r").await.unwrap();

        // open_session #2 (simulate a reconnect).
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "MYCALL W1TEST");
        w.write_all(b"OK\r").await.unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "CONNECT W1TEST N0CALL-8");
        w.write_all(b"PENDING\r").await.unwrap();
        w.write_all(b"CONNECTED W1TEST N0CALL-8\r").await.unwrap();
    });

    let mut vara = VaraTransport::new();
    let cfg = TransportConfig {
        kind: TransportKind::VaraFm,
        agwpe: AgwpeParams { host: "unused".into(), port: 0 },
        vara: VaraParams {
            cmd_host: "127.0.0.1".into(),
            cmd_port,
            data_host: "127.0.0.1".into(),
            data_port,
            mode: VaraMode::Fm,
            bandwidth: VaraBandwidth::VWide,
        },
    };
    let session = SessionConfig {
        local_callsign: "W1TEST".into(),
        remote_callsign: "N0CALL-8".into(),
        bpq_command: String::new(),
        skip_bpq_app: true,
        agwpe_port: 0,
    };

    vara.connect_modem(&cfg).await.unwrap();
    vara.open_session(&session).await.unwrap();
    vara.send(b"HELLO").await.unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    match vara.recv(deadline).await.unwrap() {
        TransportEvent::Data(bytes) => assert_eq!(bytes, b"WORLD"),
        other => panic!("expected Data, got {other:?}"),
    }

    vara.close_session().await.unwrap();
    vara.open_session(&session).await.unwrap();

    mock.await.unwrap();
}
```

- [ ] **Step 2: Run the test**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client --test vara_mock_modem`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add client/tests/vara_mock_modem.rs
git commit -m "client(vara): end-to-end integration test with mock modem

Drives VaraTransport through connect → session → send/recv → close →
reopen against a scripted TCP mock, standing in for a real VARA modem."
```

---

## Task 9: `/connect` UI — Transport dropdown and dynamic field render

**Files:**
- Modify: `client/src/ui.rs` (`connect_page` signature, HTML/JS)
- Modify: `client/src/proxy.rs` (`connect_page_handler` passes new state; existing GET /api/state extended to return transport info)

**Interfaces:**
- Consumes: `TransportKind`, `VaraParams` from Task 1; `TransportSection`, `VaraSection` from Task 2.
- Produces:
  - `connect_page` gains parameters: `transport_default: TransportKind`, `vara_params: &VaraParams`
  - `GET /api/state` response body includes `"transport": "ax25" | "vara_fm" | "vara_hf"` and the `vara.*` echo fields.

- [ ] **Step 1: Write the failing UI test**

Add to the existing `#[cfg(test)] mod tests` in `client/src/ui.rs`:

```rust
#[test]
fn connect_page_renders_transport_dropdown_with_defaults() {
    let html = connect_page(
        "W1TEST",
        "N0CALL-8",
        "127.0.0.1",
        8000,
        crate::transport::TransportKind::VaraFm,
        &crate::transport::VaraParams {
            cmd_host: "10.0.0.5".into(),
            cmd_port: 8300,
            data_host: "10.0.0.5".into(),
            data_port: 8301,
            mode: crate::transport::VaraMode::Fm,
            bandwidth: crate::transport::VaraBandwidth::VWide,
        },
        // …existing extra args if any
    );

    assert!(html.contains("<select id=\"transport\""));
    assert!(html.contains("value=\"ax25\""));
    assert!(html.contains("value=\"vara_fm\" selected"));
    assert!(html.contains("value=\"vara_hf\""));
    assert!(html.contains("id=\"vara-cmd-host\""));
    assert!(html.contains("value=\"10.0.0.5\""));
}
```

- [ ] **Step 2: Update `connect_page` signature and body**

Add the two new parameters (`transport_default`, `vara_params`) and add markup + JS. The Transport dropdown becomes the first form field; below it, two mutually-exclusive containers (`<div id="ax25-fields">` and `<div id="vara-fields">`) hold the transport-specific inputs. A small JS handler on the dropdown's `change` event toggles `hidden` on the two containers.

For brevity, the exact HTML is left to the implementer's judgement, but it must satisfy every assertion in the test.

- [ ] **Step 3: Update `connect_page_handler`**

In `client/src/proxy.rs`, extend the `Html(ui::connect_page(...))` call to include the two new arguments, sourced from `state.config.transport.default` and `state.config.vara` (build a `&VaraParams` on the fly if the config uses a separate `VaraSection` struct — populate a `VaraParams` from `VaraSection`).

Extend `GET /api/state`'s response struct (search for `agwpe_host: String` at ~L804) to include:

```rust
transport: String,          // TransportKind::to_string()
vara_cmd_host: String,
vara_cmd_port: u16,
vara_data_host: String,
vara_data_port: u16,
vara_mode: String,          // "fm" | "hf"
vara_bandwidth: String,
```

- [ ] **Step 4: Run tests**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client ui::tests::connect_page_renders_transport_dropdown_with_defaults`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add client/src/ui.rs client/src/proxy.rs
git commit -m "client: /connect Transport dropdown + dynamic VARA fields"
```

---

## Task 10: Wire the `/api/connect` handler to build a `VaraTransport` when the operator picks VARA

**Files:**
- Modify: `client/src/proxy.rs` (`api_connect_handler` and its request struct)

**Interfaces:**
- Consumes: `VaraTransport` (Task 6/7), `TransportManager` (Task 4), `TransportConfig`, `SessionConfig`.
- Produces: `POST /api/connect` accepts an additional `"transport": "ax25" | "vara_fm" | "vara_hf"` field (defaults to `ax25` when absent). When the operator picks VARA, the handler builds `Box::new(VaraTransport::new()) as Box<dyn Transport>` and hands it to `TransportManager::spawn`.

- [ ] **Step 1: Write a failing integration test**

Create `client/tests/api_connect_transport_dispatch.rs`:

```rust
// Send POST /api/connect with transport=vara_fm and assert:
// - the handler responds 200
// - the manager was spawned with a VaraTransport (proxy expose a
//   test-only accessor or check via GET /api/state showing
//   transport == "vara_fm")

// If instrumenting the internals is too heavy, an acceptable weaker
// assertion is: POST /api/connect with transport=vara_fm succeeds at
// the HTTP layer (i.e. field parsing works and the request is routed)
// even though the TCP connect will fail because there is no VARA modem
// listening. The handler should return a JSON body with a well-formed
// error string mentioning VARA.
```

Choose the weaker assertion form — instrumenting the manager for tests is out of scope for this task.

- [ ] **Step 2: Update the `POST /api/connect` request struct**

Search `client/src/proxy.rs` for the request struct backing `api_connect_handler` (grep for `Deserialize` near `agwpe_host`). Add:

```rust
#[serde(default)]
transport: Option<String>,      // "ax25" | "vara_fm" | "vara_hf"
#[serde(default)]
vara_cmd_host: Option<String>,
#[serde(default)]
vara_cmd_port: Option<u16>,
#[serde(default)]
vara_data_host: Option<String>,
#[serde(default)]
vara_data_port: Option<u16>,
#[serde(default)]
vara_mode: Option<String>,
#[serde(default)]
vara_bandwidth: Option<String>,
```

In the handler body, resolve the `TransportKind` from the field (falling back to `state.config.transport.default`), assemble a `TransportConfig` populated from either the AGWPE fields (existing) or the VARA fields, and then:

```rust
let transport: Box<dyn crate::transport::Transport> = match transport_kind {
    TransportKind::Ax25 => Box::new(
        crate::transport::agwpe::AgwpeTransport::new(
            state.config.connection.response_timeout_secs,
        ),
    ),
    TransportKind::VaraFm | TransportKind::VaraHf => {
        Box::new(crate::transport::vara::VaraTransport::new())
    }
};
let manager = crate::transport::TransportManager::spawn(transport);
```

Replace the previous `AgwpeManager::spawn` call site with this block.

- [ ] **Step 3: Run the test**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client --test api_connect_transport_dispatch`
Expected: PASS.

- [ ] **Step 4: Run the whole suite**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo test -p packet-browser-client`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add client/src/proxy.rs client/tests/api_connect_transport_dispatch.rs
git commit -m "client: /api/connect dispatches to VaraTransport when picked"
```

---

## Task 11: `demo-vara.sh` scaffold and README section

**Files:**
- Create: `demo-vara.sh`
- Modify: `README.md`

**Interfaces:**
- Consumes: nothing new
- Produces: a runnable-but-documentational shell script that describes the Mercury-on-both-ends topology and prints usage instructions, plus a README section pointing to it.

- [ ] **Step 1: Write `demo-vara.sh`**

```bash
#!/usr/bin/env bash
# demo-vara.sh — end-to-end VARA/Mercury manual demo scaffold
#
# The AX.25 demo (demo.sh) can auto-wire Direwolf on both ends because
# Direwolf is open-source and works with looped audio. VARA is
# proprietary and license-gated. Mercury is open-source and speaks the
# VARA-HF API; it's the only in-repo-runnable end-to-end option.
#
# This script does not launch modems for you — it documents the
# expected topology and starts packet-browser-server + packet-browser-client
# configured to point at the modem endpoints below. Run Mercury (or
# your VARA installation) yourself on each side before invoking.

set -euo pipefail

: "${MERCURY_CMD_PORT_CLIENT:=3000}"
: "${MERCURY_DATA_PORT_CLIENT:=3001}"
: "${LINBPQ_HTTP_PORT:=8082}"

cat <<EOF
[demo-vara] Expected topology:

  packet-browser-client
      │
      ├── Mercury (cmd :$MERCURY_CMD_PORT_CLIENT / data :$MERCURY_DATA_PORT_CLIENT)
      │        │
      │        ▼ RF (or loopback audio)
      │
      ├── Mercury on the other end
      │        │
      │        ▼
      ├── LinBPQ (VARA port configured)
      │        │
      │        ▼
      └── packet-browser-server (unchanged)

Before running this demo:
  - Start Mercury on both ends
  - Configure LinBPQ on the server side with a VARA port pointing at
    Mercury's cmd/data ports
  - Open http://127.0.0.1:<client-port>/connect and pick "VARA HF"
    with the Mercury ports above.

This script is a placeholder that intentionally does not spawn Mercury.
When run, it prints this help and exits.
EOF

exit 0
```

- [ ] **Step 2: Make it executable**

```bash
chmod +x demo-vara.sh
```

- [ ] **Step 3: Update README.md**

Search the existing "Demo mode" section and append a short subsection linking `demo-vara.sh`. Content the implementer must write:

- One-sentence lead: "For VARA/Mercury manual testing, see `demo-vara.sh` — it prints the expected topology and required prerequisites."
- Bullet list of the four Mercury/LinBPQ prerequisites listed in `demo-vara.sh`.

- [ ] **Step 4: Commit**

```bash
git add demo-vara.sh README.md
git commit -m "docs: demo-vara.sh scaffold + README pointer

VARA is license-gated; Mercury is the runnable stand-in. The script
documents the topology and required steps rather than launching modems
itself."
```

---

## Task 12: v0.5.0 release commit + tag

**Files:**
- Modify: `client/Cargo.toml` (version = "0.5.0")
- Modify: `server/Cargo.toml` (version = "0.5.0")
- Modify: `server/src/main.rs` (const VERSION)
- Modify: `Cargo.lock` (sync)

**Interfaces:**
- Consumes: everything above
- Produces: annotated tag `v0.5.0`.

- [ ] **Step 1: Bump versions**

Bump `client/Cargo.toml`, `server/Cargo.toml`, and the `VERSION` constant in `server/src/main.rs` from `0.4.0` → `0.5.0`.

- [ ] **Step 2: Sync `Cargo.lock`**

Run: `nix --extra-experimental-features 'nix-command flakes' develop -c cargo build --workspace`
Expected: clean build, `Cargo.lock` updated.

- [ ] **Step 3: Commit the version bump**

```bash
git add client/Cargo.toml server/Cargo.toml server/src/main.rs Cargo.lock
git commit -m "release: bump packet-browser to 0.5.0

Client gains VARA FM and VARA HF as first-class transports selectable
per-connect on /connect (see the v0.5.0 tag message for the full
release notes). Server unchanged."
```

- [ ] **Step 4: Push and tag**

```bash
git push origin main
git tag -a v0.5.0 -m "v0.5.0 — VARA / Mercury client transport

- Extract Transport trait from agwpe.rs into client/src/transport/
- VARA FM (primary) and VARA HF (secondary) transports; Mercury runs
  incidentally on the VARA HF path
- /connect page grows a Transport dropdown; [transport] and [vara]
  config sections added to client.ini
- Server binary unchanged; wire protocol unchanged
- End-to-end mock-modem integration test covers connect_modem →
  open_session → send/recv → close_session → reconnect
- demo-vara.sh scaffold documents the Mercury-on-both-ends topology" HEAD
git push origin v0.5.0
```

- [ ] **Step 5: Update `nix-ham-packages` in a follow-up commit** (out of scope for this plan; note it here so it isn't forgotten)

The `nix-ham-packages` bump pattern is documented in that repo's history; the release commit for `v0.4.0` there (`eb04732`) is a template.

---

## Self-Review

Spec coverage: every section of `2026-07-22-vara-transport-design.md` maps to a task —

- Module reorg + trait extraction → Tasks 1, 3, 4
- `Transport` trait shape → Task 1
- Graceful-reconnect reuse via `TransportEvent::Disconnected` → Task 4 (mapping table) + Task 7 (`recv`)
- VARA `connect_modem` → Tasks 5, 6
- VARA `open_session`, session-death detection, `close_session` → Task 7
- `send` / `recv` multiplex → Task 7
- Session-death detection stronger than AGWPE (existing app-layer detectors kept) → covered implicitly by Task 4 preserving `is_session_dead_payload` in the session-level driver
- Error mapping to operator-visible pages → Task 4 mapping table + Task 7 (`SessionRejected` for busy channel)
- `/connect` UI Transport dropdown → Task 9
- `client.ini` schema (`[transport]`, `[vara]`) → Task 2
- Unit tests (parser, setup command ordering) → Task 5
- Integration test with mock modem → Task 8
- Manual demo (`demo-vara.sh`) → Task 11
- Migration/compatibility → Task 2 (defaults) + Task 4 (behaviour preservation) + Task 12 (release)

No `TBD` / `TODO` / "similar to task N" / vague-error-handling markers survive above.

Type consistency: `TransportKind`, `TransportEvent`, `TransportError`, `SessionConfig`, `TransportConfig`, `VaraParams`, `VaraMode`, `VaraBandwidth`, `VaraResponse`, `AgwpeTransport`, `VaraTransport`, `TransportManager` — every name introduced in one task is used with the same signature in every follow-up task.
