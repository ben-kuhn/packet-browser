# Phase 5: Client Web Proxy + Configuration UI

**Date:** 2026-06-26
**Status:** Approved
**Author:** Design collaboration with KU0HN

## Overview

Implement the client-side web proxy with configuration UI, AGWPE connection management, and HTML URL rewriting. The client provides a web interface for users to configure AGWPE settings, establish AX.25 connections, and browse pages fetched over the radio link.

## Goals

- Web-based configuration interface (primary UX)
- INI config file matching TNCD conventions for persistence
- Async AGWPE manager with background task
- Connection state machine with debug logging
- HTML URL rewriting for local proxy routing
- SSE-based live debug log in web UI
- Dark theme UI with monospace debug panel

## Non-Goals

- Multiple concurrent AX.25 connections (one at a time)
- Auto-reconnect on connection drop (manual reconnect required)
- JavaScript execution on client side (JS stripped by server)

---

## 1. Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                        Tokio Runtime                           │
├──────────────────────────────────────────────────────────────┤
│                                                               │
│  ┌─────────────┐    command_tx     ┌──────────────────────┐ │
│  │  Axum HTTP   │ ──────────────►  │  AGWPE Background    │ │
│  │  Handlers    │                   │  Task                │ │
│  │              │ ◄──────────────  │                      │ │
│  └─────────────┘    event_rx       │  - TCP connection    │ │
│        │                            │  - Frame parsing     │ │
│        │                            │  - State machine     │ │
│        ▼                            │  - Debug logging     │ │
│  ┌─────────────┐                    └──────────────────────┘ │
│  │  AppState    │                                            │
│  │  (Arc<Mutex>)│                                            │
│  │  - config    │                                            │
│  │  - conn state│                                            │
│  │  - debug log │                                            │
│  └─────────────┘                                            │
│                                                               │
└──────────────────────────────────────────────────────────────┘
```

---

## 2. Configuration

### Config File Format (INI, matching TNCD)

Location: `~/.config/packet-browser/config.ini` (or specified with `-c`)

```ini
# packet-browser-client configuration

[server]
agwpe_host = 127.0.0.1
agwpe_port = 8000

[session]
my_callsign = N0CALL
target_callsign = NODE1
```

**Config fields:**
- `agwpe_host`: AGWPE TCP API host (default: 127.0.0.1)
- `agwpe_port`: AGWPE TCP API port (default: 8000)
- `my_callsign`: User's amateur radio callsign (persisted)
- `target_callsign`: Last connected BPQ node (cached on each connect)

### Config Resolution Order

1. CLI flags (`--agwpe-host`, `--agwpe-port`) override everything
2. Config file (`-c path/to/file.ini`, or default `~/.config/packet-browser/config.ini`)
3. Built-in defaults

### CLI Interface (matching TNCD pattern)

```
packet-browser-client [OPTIONS]

Options:
  -c, --config FILE          Configuration file (INI format)
  --agwpe-host HOST          AGWPE host (default: 127.0.0.1)
  --agwpe-port PORT          AGWPE port (default: 8000)
  --listen-addr ADDR         Web proxy listen address (default: 127.0.0.1:8080)
  -v, -vv, -vvv              Console verbosity levels
```

**Verbosity levels:**
- (none) → WARN only
- `-v` → INFO (connection state changes, errors)
- `-vv` → DEBUG (frame summaries: type, size, callsigns)
- `-vvv` → TRACE (full frame hex dumps)

---

## 3. Connection State Machine

```
Disconnected ──[AGWPE connect]──► AgwpeConnected
                                       │
                                  [AX.25 connect]
                                       │
                                       ▼
                                   Connecting ──[success]──► Connected
                                       │                        │
                                  [failure]               [disconnect]
                                       │                        │
                                       ▼                        ▼
                                     Error ◄──────────── Disconnected
```

**States:**
- `Disconnected` - No AGWPE TCP connection
- `AgwpeConnected` - TCP to AGWPE open, callsign registered, ports known
- `Connecting` - AX.25 connect request sent, waiting for response
- `Connected` - AX.25 link up, ready to browse
- `Error(String)` - Something failed, requires manual reconnect

---

## 4. Web Routes

### Page Routes (serve HTML)

| Route | Method | Purpose |
|-------|--------|---------|
| `/` | GET | Redirect to `/connect` |
| `/connect` | GET | Connection UI (callsigns, port dropdown, status, debug log) |
| `/configuration` | GET | AGWPE settings form |
| `/browse` | GET | Fetch page (redirect to `/connect` if disconnected) |
| `/browse` | POST | Submit form (redirect to `/connect` if disconnected) |

### API Routes (return JSON, called by JS)

| Route | Method | Purpose |
|-------|--------|---------|
| `/api/agwpe-status` | GET | Try AGWPE connect, return ports + status as JSON |
| `/api/connect` | POST | Establish AX.25 connection, cache target_callsign |
| `/api/disconnect` | POST | Tear down AX.25 connection |
| `/api/config` | GET | Load current config as JSON |
| `/api/config` | POST | Save config to INI file |

### SSE Route

| Route | Method | Purpose |
|-------|--------|---------|
| `/events` | GET | SSE stream of debug log entries |

---

## 5. Shared State

```rust
struct AppState {
    config: FileConfig,
    connection_state: ConnectionState,
    debug_log: VecDeque<DebugLogEntry>,  // ring buffer
    available_ports: Vec<PortInfo>,
    agwpe_port_num: Option<u8>,          // selected from dropdown
}
```

### Debug Log Entry

```rust
struct DebugLogEntry {
    timestamp: DateTime<Utc>,
    level: LogLevel,           // Info, Debug, Trace
    direction: Option<Dir>,    // Tx, Rx (for frames)
    category: String,          // "STATE", "AGWPE", "PROTOCOL", "ERROR"
    message: String,
    details: Option<String>,   // hex dump at trace level
}
```

**Ring buffer:** Configurable size (default 1000 entries), oldest dropped when full.

### Port Info

```rust
struct PortInfo {
    port_num: u8,
    description: String,
}
```

---

## 6. AGWPE Module Refactor

The current synchronous `agwpe.rs` must become async with a background task.

### New Frame Types

- `QueryPorts = 0x47` ('G') - request port list
- `PortInfo = 0x67` ('g') - port info response

### Port Discovery Flow

1. Send 'G' frame (empty data)
2. Receive one or more 'g' frames, each containing: `port_number (u8)` + `description (null-terminated string)`
3. Last 'g' frame has `data_len == 0` → end of list

### Background Task Architecture

```rust
enum AgwpeCommand {
    ConnectToAgwpe { host: String, port: u16 },
    DisconnectAgwpe,
    QueryPorts,
    Ax25Connect { target: String, port_num: u8 },
    Ax25Disconnect,
    SendRequest { data: Vec<u8>, reply: oneshot::Sender<Vec<u8>> },
}

enum AgwpeEvent {
    StateChanged(ConnectionState),
    LogEntry(DebugLogEntry),
    PortsDiscovered(Vec<PortInfo>),
}
```

**Background task responsibilities:**
1. Reads from `command_rx` channel
2. Owns the `tokio::net::TcpStream` to AGWPE
3. Reads frames from TCP in a loop
4. Dispatches incoming frames (data responses, connect confirmations, etc.)
5. Sends state changes and debug entries via `event_tx`

---

## 7. UI Design

### Connect Page (`/connect`)

- Current connection state (color-coded badge)
- My callsign input (pre-populated from config)
- Target callsign input (pre-populated from config)
- Port dropdown (populated from AGWPE port discovery)
- Connect button (disabled if already connected)
- Disconnect button (disabled if not connected)
- Debug log panel (auto-scrolling, filterable by level)

**JS behavior:**
- On page load: call `GET /api/agwpe-status` to check AGWPE and populate ports
- If AGWPE unreachable: show error with link to `/configuration`
- On connect: call `POST /api/connect`, update state via SSE
- Debug log: subscribe to `/events` SSE stream, append entries in real-time

### Configuration Page (`/configuration`)

- AGWPE host input
- AGWPE port input
- Save button
- "Test Connection" button (optional, validates AGWPE is reachable)

**JS behavior:**
- On page load: call `GET /api/config` to pre-populate form
- On save: call `POST /api/config`, show success/error message

### Styling

Dark theme, monospace for debug log:

```css
body { background: #1a1a2e; color: #e0e0e0; font-family: sans-serif; }
input, select, button { background: #16213e; color: #e0e0e0; border: 1px solid #0f3460; }
.debug-log { font-family: monospace; font-size: 0.85em; background: #0a0a1a; }
.status-connected { color: #4caf50; }
.status-disconnected { color: #f44336; }
.status-connecting { color: #ff9800; }
```

---

## 8. Implementation Steps

### Step 1: Config module (`client/src/config.rs`)

- `FileConfig` struct with serde Serialize/Deserialize
- INI read/write using `configparser` crate
- `load(path)` / `save(path)` functions
- Default path: `dirs::config_dir()/packet-browser/config.ini`
- CLI override logic: defaults → config file → CLI flags

**Tests:**
- Config load/save round-trip
- CLI override precedence
- Missing config file uses defaults

### Step 2: State module (`client/src/state.rs`)

- `ConnectionState` enum (Disconnected, AgwpeConnected, Connecting, Connected, Error)
- `DebugLogEntry` struct with ring buffer
- `PortInfo` struct (port_num, description)
- `AppState` with config, connection state, debug log, available ports
- `Arc<Mutex<AppState>>` for thread-safe access
- Ring buffer helpers for debug log (push, get_all, filter_by_level)

**Tests:**
- State transitions
- Debug log ring buffer overflow
- Port info storage

### Step 3: AGWPE refactor (`client/src/agwpe.rs`)

- Add `QueryPorts` (0x47) / `PortInfo` (0x67) frame types
- Refactor to async with `tokio::net::TcpStream`
- `AgwpeManager` background task with command/event channels
- Port discovery via 'G'/'g' frames
- Protocol request/response via oneshot channels
- Debug logging on all operations (push to shared state)

**Tests:**
- Port info frame parsing
- Frame encoding/decoding (already done)
- Command/event channel flow

### Step 4: Rewrite module (`client/src/rewrite.rs`)

- Use `lol_html` streaming rewriter
- Rewrite `<a href>` → `/browse?url=<encoded>`
- Rewrite `<form action>` → `/browse?url=<encoded>`
- Resolve relative URLs against page base URL
- Strip `javascript:`, `data:`, `mailto:` URLs
- Inject CSS for image placeholders

**Tests:**
- Absolute URL rewriting
- Relative URL resolution
- Fragment handling
- Dangerous protocol stripping

### Step 5: UI module (`client/src/ui.rs`)

- HTML templates as embedded `const &str` (using `include_str!` or inline)
- Connect page with JS for API calls + SSE
- Configuration page with form
- Dark theme CSS
- Debug log panel with live SSE updates
- Level filter dropdown (Info, Debug, Trace, All)

### Step 6: Proxy module (`client/src/proxy.rs`)

- Axum router with all routes
- `/browse` handler: check state → encode protocol request → send via AGWPE → recv response → decompress → rewrite URLs → return HTML
- `/browse` redirect to `/connect` if disconnected
- API handlers for connect/disconnect/config/agwpe-status
- SSE endpoint for debug log
- Config file read/write handlers

**Tests:**
- Browse redirect when disconnected
- Protocol request encoding
- Response decompression

### Step 7: Main wiring (`client/src/main.rs`)

- `clap` CLI parsing with `-c`, `-v`/`-vv`/`-vvv`, overrides
- Initialize tracing subscriber with log level
- Load config from disk (defaults → file → CLI)
- Create shared state
- Spawn AGWPE background task
- Start axum server
- Graceful shutdown on Ctrl+C

### Step 8: Integration tests

- End-to-end: config load → AGWPE connect → browse request → response
- Mock AGWPE server for testing
- State transitions through full flow

---

## 9. Dependencies

```toml
# client/Cargo.toml additions
clap = { version = "4", features = ["derive"] }
configparser = "3"         # INI format (matching TNCD's configparser)
dirs = "5"                 # XDG config paths
chrono = { version = "0.4", features = ["serde"] }
tokio-stream = "0.1"       # SSE support
```

**Existing dependencies (already in Cargo.toml):**
- `tokio` (async runtime)
- `axum` (web server)
- `lol_html` (HTML rewriting)
- `url` (URL parsing)
- `serde`, `serde_json` (serialization)
- `thiserror` (error types)
- `tracing`, `tracing-subscriber` (logging)
- `packet-browser-shared` (protocol + compression)

---

## 10. File Structure

```
client/
├── Cargo.toml
└── src/
    ├── main.rs          # Entry point, CLI, wiring
    ├── config.rs        # INI config load/save
    ├── state.rs         # Shared state, debug log, connection state
    ├── agwpe.rs         # AGWPE protocol (async refactor)
    ├── proxy.rs         # Axum routes and handlers
    ├── rewrite.rs       # HTML URL rewriting
    └── ui.rs            # HTML templates (embedded)
```

---

## 11. User Flow

1. User starts `packet-browser-client` (no config needed)
2. Opens browser to `localhost:8080`
3. Sees `/connect` page - AGWPE status shows "Disconnected"
4. JS auto-calls `GET /api/agwpe-status` → client tries AGWPE on default 127.0.0.1:8000
5. **If AGWPE reachable:** ports populate in dropdown, user enters callsigns, clicks Connect
6. **If AGWPE unreachable:** error message with link to `/configuration`
7. `/configuration` page: user sets AGWPE host/port, clicks Save → writes INI file
8. Back to `/connect`: auto-retries AGWPE with new settings
9. Once connected, `/browse` works
10. On next connect, `target_callsign` is cached in config file

---

## 12. Testing Strategy

### Unit tests
- Config INI read/write round-trip
- CLI override logic
- State transitions
- Debug log ring buffer
- URL rewriting rules
- Port info frame parsing

### Integration tests
- Mock AGWPE server for end-to-end flow
- Config load → AGWPE connect → browse request → response
- State machine transitions through full flow

### Manual testing
- Connect to real AGWPE + BPQ + server
- Browse several sites, verify content loads
- Test forms (search, login)
- Verify debug log shows all operations
- Test config persistence across restarts

---

## 13. Migration Notes

**Breaking changes from current `client/src/main.rs`:**
- Complete rewrite of entry point
- Config moved from env vars to INI file
- Callsigns now in config file (not env vars)

**Preserved:**
- AGWPE frame encoding/decoding (refactored to async)
- Protocol types from `shared` crate

**New:**
- Web UI (axum + HTML templates)
- Config file persistence (INI format)
- SSE debug log streaming
- URL rewriting (lol_html)
- Connection state machine
- Background AGWPE task
