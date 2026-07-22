# VARA / Mercury Modem Transport

## Problem

`packet-browser-client` speaks AX.25 through Direwolf via the AGWPE
protocol and nothing else. AX.25 packet is slow, and the client's
transport layer (`client/src/agwpe.rs`, 1873 lines) is tightly coupled
to AGWPE frame types, an `AgwpeManager` actor, and AGWPE-specific
command variants. Operators who have VARA (VARA FM or VARA HF) or
Mercury available on their end can get much higher throughput —
particularly VARA FM for local links — but the client has no way to
use them.

## Goals

- Add VARA FM (primary) and VARA HF (secondary) as first-class wire
  transports the operator can select on the `/connect` page.
- Preserve the existing graceful-reconnect infrastructure — session-
  drop detection, auto-consent, response-timeout fallback, error pages
  — across both transports.
- Extract a `Transport` trait so future transports can be added without
  a second 1873-line file.
- Ship in `v0.5.0`; wire-compatible with the current `v0.4.0` server.

## Non-goals

- Server-side VARA support. The server is a BPQ application; the
  transport between LinBPQ and the packet-browser-server is TCP either
  way. LinBPQ handles VARA on the server side as one of its native port
  types, so the server binary and its deployment topology are
  unchanged.
- Simultaneous multi-transport sessions. One transport at a time; the
  operator switches on `/connect`.
- Modem-flavour presets beyond VARA FM and VARA HF. Mercury runs
  incidentally on the VARA HF option because it is VARA-HF-API
  compatible; it does not get its own dropdown entry.

## Architecture

### Module reorg

Move `client/src/agwpe.rs` into a transport module tree:

```
client/src/transport/
├── mod.rs      — Transport trait, TransportError, TransportEvent, TransportManager
├── agwpe.rs    — existing AGWPE code, refactored to impl Transport
└── vara.rs     — new VARA implementation
```

Everything above the transport layer stays put: response framing,
cache, proxy handlers, state machine (`Disconnected → Connecting →
Connected → Reconnecting → Error`), graceful-reconnect logic
(`handle_reconnect`, auto-consent, disclaimer matching), the LinBPQ
status-line filter, the `is_session_dead_payload` scanner for
`*** DISCONNECTED FROM` and `Returned to Node …`, and the session-
error pages. Those all operate on byte streams and don't know which
wire delivered them.

`AgwpeManager` is renamed `TransportManager` and holds
`Box<dyn Transport>`. Its public API is unchanged from the caller's
perspective (`send_request`, `send_request_with_reconnect`,
`connect_to_modem`, `disconnect_modem`, `open_session`,
`close_session`).

### The `Transport` trait

Final naming may drift during implementation; the shape is what
matters.

```rust
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

pub enum TransportEvent {
    Data(Vec<u8>),
    Disconnected { reason: String },
}

pub enum TransportError {
    NotConnected,
    Io(std::io::Error),
    ModemError(String),          // AGWPE frame errors, VARA cmd-port errors
    SessionRejected(String),     // AGWPE ConnectionRejected, VARA BUSY DETECTED
    Timeout,
}
```

`BackgroundState` and the actor loop become generic over
`Box<dyn Transport>`. `handle_send_request`, `handle_reconnect`, and
the four BPQ helpers become transport-agnostic driver code that calls
`send` / `recv`.

### Graceful-reconnect reuse

The transport reports session death via `TransportEvent::Disconnected`
from `recv`. The higher layer decides whether to run
`handle_reconnect`. So `is_session_dead_payload` and the auto-consent
flow stay in exactly one place and cover both transports for free.

The AGWPE-only "reopen the AGWPE TCP socket during reconnect" step
(commit `157a564`, added to survive Direwolf's frozen-reader-thread
state) becomes a no-op in the VARA implementation — its equivalent is
just re-issuing `DISCONNECT` on the still-open command port.

## VARA protocol handling

### Two TCP sockets

Both VARA FM and VARA HF expose a command port and a data port. Both
are opened as separate TCPs to the modem. The command port carries
ASCII lines terminated by `\r`; the data port carries raw session
bytes bidirectionally once a session is up. `vara.rs`'s `Transport`
impl owns both sockets and multiplexes them behind a single `recv`
future.

### `connect_modem`

1. TCP-connect to `cmd_host:cmd_port` and `data_host:data_port`.
2. Send on command port, waiting for `OK` after each:
   - `MYCALL <local_callsign>`
   - `LISTEN OFF` — we are a client, not an inbound listener.
   - `COMPRESSION OFF` — brotli is already applied at the app layer.
   - Mode line: `VNARROW` or `VWIDE` for FM; `BW250` / `BW500` /
     `BW2300` / `BW2750` for HF.
3. `MISSING <field>` or other error responses surface as
   `TransportError::ModemError(String)`.

### `open_session`

1. Send `CONNECT <local> <remote>` on command port.
2. Expect `PENDING`, then either `CONNECTED <local> <remote>` (success)
   or `DISCONNECTED` (rejection or timeout) or `BUSY DETECTED` (link
   busy on RF).
3. On success, the session is live. The data port is now the raw byte
   stream to the peer's LinBPQ VARA port.
4. `BUSY DETECTED` returns `TransportError::SessionRejected("channel
   busy")`.

### `send` and `recv`

- `send(data)`: write bytes to the data port. VARA handles
  fragmentation and retransmit internally.
- `recv(deadline)`: `tokio::select!` on:
  - data-port readable → return `TransportEvent::Data(bytes)`.
  - command-port line arrives → interpret:
    - `DISCONNECTED` → `TransportEvent::Disconnected { reason:
      "vara modem reports disconnect" }`.
    - Everything else (`BUFFER n`, `LINK REGISTERED`, informational
      status lines) is logged at debug and ignored.
  - deadline elapsed → `Err(TransportError::Timeout)`.

### `close_session`

Send `DISCONNECT` on the command port, drain the command port for up
to 3 s waiting for the `DISCONNECTED` confirmation, then return. This
is the direct analog of the AGWPE `d`-frame + drain we added in
`3970b64`. The 3 s drain deadline mirrors that fix.

### Session-death detection is stronger than on AGWPE

The modem tells us explicitly when the peer drops off via
`DISCONNECTED` on the command port. On VARA we do not need the 30 s
response-timeout fallback to catch a stuck session. The existing
app-layer detectors — `is_session_dead_payload` matching
`*** DISCONNECTED FROM …` and `Returned to Node …` — still fire off
server-emitted text and cover the edge case where LinBPQ tears down
the WEB application session while VARA-layer link stays up briefly, so
we keep them in place.

## UI and config

### `/connect` page

Add a **Transport** dropdown as the first field of the Connection card
with three options:

- `AX.25 (AGWPE)`
- `VARA FM`
- `VARA HF`

The form re-renders based on the selection.

- **AX.25**: existing fields — AGWPE host + port, callsign pair,
  AGWPE port list dropdown.
- **VARA FM / VARA HF**: cmd host + cmd port, data host + data port
  (defaulting to the same host), bandwidth mode dropdown (`VNARROW` /
  `VWIDE` for FM; `BW250` / `BW500` / `BW2300` / `BW2750` for HF), and
  the callsign pair.

The AX.25 Connect / Disconnect buttons, the connection status pill,
and the Debug Log pane are shared across transports.

Error pages: `render_session_error_page` already accepts a free-form
reason string and a "show Reconnect link" flag, so VARA-specific
reasons (`"VARA modem: MISSING MYCALL"`, `"Channel busy — try
again"`, etc.) plug into it unchanged.

### `client.ini` schema

Keep `[server]` for AGWPE. Add a peer `[vara]` section. Add a
`[transport]` section that picks the default dropdown value. Only the
section matching the chosen transport is consumed on connect — the
other section can be left blank.

```ini
[transport]
default = ax25          ; ax25 | vara_fm | vara_hf

[server]                ; AGWPE, unchanged
agwpe_host = 127.0.0.1
agwpe_port = 8000

[session]               ; unchanged
my_callsign = W1TEST
target_callsign = N0CALL-8

[connection]            ; unchanged, applies to both transports
response_timeout_secs = 30
auto_reconnect = true

[vara]
cmd_host = 127.0.0.1
cmd_port = 8300
data_host = 127.0.0.1
data_port = 8301
mode = fm               ; fm | hf
bandwidth = vwide       ; vnarrow | vwide (fm) or bw250 | bw500 | bw2300 | bw2750 (hf)
```

## Testing

### Unit tests (`transport/vara.rs`)

- Parse each command-port response VARA can emit and assert it maps to
  the expected `TransportEvent` or `TransportError` variant: `OK`,
  `PENDING`, `CONNECTED W1TEST N0CALL-8`, `DISCONNECTED`,
  `BUSY DETECTED`, `MISSING MYCALL`, `BUFFER 123`, `LINK REGISTERED`.
- Assert `connect_modem` emits `MYCALL`, `LISTEN OFF`, `COMPRESSION
  OFF`, and the mode line in that order.
- Assert `close_session` emits `DISCONNECT` and returns once the
  confirmation arrives; assert it returns within the 3 s drain
  deadline when the confirmation is dropped.

### Trait-shape regression

Existing AGWPE tests move with the file to `transport/agwpe.rs`.
Anywhere a test asserted state-machine behaviour rather than AGWPE
frame details, refactor it to run against the trait so both transports
share coverage.

### Integration test

`client/tests/vara_mock_modem.rs`: a ~200-line Rust mock that binds
two TCP ports and speaks the minimum command dialog — `OK` echoes,
`CONNECTED` on cue, `DISCONNECTED` on cue. Drive a `TransportManager`
end-to-end through `connect_modem → open_session → send / recv →
close_session → open_session` (post-reconnect). Assert graceful-
reconnect fires on a simulated peer disconnect.

### Manual demo

`demo.sh` today wires AX.25 through Direwolf. Add a companion
`demo-vara.sh` documenting "run Mercury on both ends" as the manual
end-to-end path, rather than mocking the modem in the demo — mocks
defeat the point of the real-audio-loop demo. VARA proprietary is
license-gated, so Mercury is the only in-repo-runnable option;
document the two Mercury processes and their config in the demo script
header.

## Migration and compatibility

- Configs without `[transport]` or `[vara]` sections default to
  `ax25` — nothing existing breaks.
- Wire protocol on the byte-stream layer is unchanged, so the server
  needs no changes.
- A `v0.4.0` server pairs correctly with a `v0.5.0` client on either
  transport.
- Existing AGWPE unit tests continue to pass unchanged after they
  follow the file into `transport/agwpe.rs`.

## Out of scope

- Server-side VARA transport.
- Modem-flavour presets beyond FM and HF.
- Automatic transport selection or per-URL transport routing.
- VARA license negotiation, key entry, or activation UI. Operator is
  assumed to have configured VARA out-of-band.
