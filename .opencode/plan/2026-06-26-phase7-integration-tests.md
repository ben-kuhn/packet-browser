# Phase 7: Integration Tests

**Date:** 2026-06-26
**Status:** Draft
**Author:** Design collaboration with KU0HN

## Overview

Implement a comprehensive integration test suite for the packet-browser project, covering both Rust-level integration tests and Python-based end-to-end tests using Direwolf TNC instances connected via PipeWire virtual audio links (following the pattern established in the tncd project).

**Key addition:** Implement the BPQ handshake in the client to match production behavior, where the client sends a BPQ APPLICATION command after AX.25 connect, then completes the callsign/AGREE handshake with the server through BPQ's bidirectional bridge.

## Goals

- Implement BPQ handshake in client (send APPLICATION command, callsign, AGREE through BPQ)
- Rust integration tests for client proxy routes and AGWPE background task
- Python e2e tests exercising the full data path over virtual radio
- Use real LinBPQ to mimic production environment
- Reuse Direwolf + PipeWire infrastructure from tncd
- All tests auto-skip when dependencies (Direwolf, PipeWire, LinBPQ) are unavailable

## Non-Goals

- OTA testing with real radio hardware (covered separately)
- Performance/load testing
- Testing in CI (e2e tests require PipeWire session, run locally only)
- Python BPQ bridge (using real LinBPQ to match production)

---

## 1. Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Test Layers                                   │
├─────────────────────────────────────────────────────────────────────┤
│                                                                       │
│  Layer 1: Rust unit tests (inline #[cfg(test)])                      │
│    - shared: protocol encode/decode, brotli compress/decompress      │
│    - client: AGWPE frames, state management, config, URL rewriting   │
│    - server: config, session, filter, logger                         │
│                                                                       │
│  Layer 2: Rust integration tests (tests/ directories)                │
│    - client: proxy routes (axum test client), AGWPE mock server      │
│    - server: protocol handler with mock TCP                          │
│                                                                       │
│  Layer 3: Python e2e tests (e2e/ directory)                          │
│    - Direwolf pair + PipeWire audio cross-link                       │
│    - LinBPQ (real BPQ node, bridges Direwolf AGWPE → server TCP)    │
│    - Test HTTP server (serves test pages)                            │
│    - Full path: client → AGWPE → Direwolf → audio → Direwolf →      │
│      LinBPQ → server → Chromium → response → back                  │
│                                                                       │
│  BPQ Handshake Flow:                                                 │
│    1. Client AX.25 connects to LinBPQ                                │
│    2. Client sends "WEB\n" (APPLICATION command)                     │
│    3. LinBPQ executes APPLICATION, connects to server TCP            │
│    4. Client sends callsign (LinBPQ forwards to server)              │
│    5. Client sends "AGREE\n" (LinBPQ forwards to server)             │
│    6. Server sends welcome + portal page (LinBPQ forwards to client) │
│    7. Client displays portal page in web UI                          │
│    8. Enter request/response loop                                    │
│                                                                       │
└─────────────────────────────────────────────────────────────────────┘
```

---

## 4. Prerequisites

### 4.1 Client BPQ handshake implementation

The client currently doesn't handle the BPQ handshake after AX.25 connect. We need to implement:

1. After AX.25 connect succeeds, send the BPQ APPLICATION command (e.g., "WEB\n")
   - This tells BPQ to execute its APPLICATION command which connects to the packet-browser server
   - BPQ then acts as a bidirectional bridge between AX.25 and TCP
2. Send the client's callsign (BPQ forwards to server)
3. Receive the AGREE prompt from server (via BPQ)
4. Send "AGREE\n" (BPQ forwards to server)
5. Receive the welcome message + portal page (Response frame from server via BPQ)
6. Store the portal page for display
7. Then enter the request/response loop

**Changes needed:**

Add to `client/src/config.rs`:
```rust
pub struct FileConfig {
    pub agwpe_host: String,
    pub agwpe_port: u16,
    pub my_callsign: String,
    pub target_callsign: String,
    pub bpq_command: String,  // NEW: e.g., "WEB" - the BPQ APPLICATION command
}
```

Add CLI flag to `client/src/config.rs`:
```rust
pub struct CliArgs {
    // ... existing fields ...
    #[arg(long, default_value = "WEB")]
    pub bpq_command: String,
}
```

Modify `client/src/agwpe.rs` `handle_ax25_connect`:
- After receiving Connected frame, send `bpq_command\n`
- Read AGREE prompt (text data), send callsign
- Read AGREE prompt (text data), send "AGREE\n"
- Read welcome message (text data)
- Read portal page (Response frame)
- Store portal page in state

**Note:** The data from BPQ will be mixed text (prompts) and binary (Response frames). We need to parse line-by-line for text, then switch to binary frame parsing after AGREE is sent.

### 4.2 Make Chromium path configurable

The server hardcodes `/bin/chromium`. For e2e testing outside Docker, we need to support an environment variable override.

**Change to `server/src/browser.rs`:**
```rust
fn chromium_path() -> String {
    std::env::var("CHROMIUM_PATH").unwrap_or_else(|_| "/bin/chromium".to_string())
}
```

Then use `chromium_path()` instead of the hardcoded string in `BrowserInstance::new()`.

### 2.2 Test dependencies

**System packages (for e2e tests):**
- `direwolf` - TNC emulator
- `pipewire` + tools (`pw-link`, `pw-dump`, `pw-cli`, `pw-metadata`)
- `python3` + `pytest` + `pytest-asyncio`

**Python packages (e2e/requirements-test.txt):**
```
pytest>=7.0
pytest-asyncio>=0.21
```

**Nix dev shell update (flake.nix):**
Add `direwolf` and `pipewire` to devShells.default.buildInputs.

---

## 3. Rust Integration Tests

### 3.1 Client proxy tests (`client/tests/proxy_test.rs`)

Use `axum::serve` with a random port + `reqwest` to test routes:

```
Test: root_redirects_to_connect
  GET / → 303 redirect to /connect

Test: connect_page_returns_html
  GET /connect → 200, contains "Packet Browser", contains callsign input

Test: configuration_page_returns_html
  GET /configuration → 200, contains "AGWPE Settings"

Test: api_config_get_returns_json
  GET /api/config → 200, JSON with agwpe_host, agwpe_port, etc.

Test: api_config_post_saves_config
  POST /api/config → 200, JSON { ok: true }
  Verify config file was written (use tempdir)

Test: api_agwpe_status_get_returns_state
  GET /api/agwpe-status → 200, JSON with state, ports

Test: browse_redirects_when_disconnected
  GET /browse?url=https://example.com → 303 redirect to /connect
  (because connection_state is Disconnected by default)

Test: events_endpoint_returns_sse
  GET /events → 200, content-type: text/event-stream
  Receives initial log entries as SSE events
```

**Test setup:**
- Create `SharedState` with default config
- Create `broadcast::channel` for debug logs
- Create `AgwpeManager` (background task starts but won't connect to AGWPE)
- Build router with `create_router()`
- Start axum on random port via `tokio::net::TcpListener::bind("127.0.0.1:0")`
- Use `reqwest::Client` for HTTP requests

### 3.2 Client AGWPE mock tests (`client/tests/agwpe_mock_test.rs`)

Start a mock AGWPE TCP server that speaks the protocol:

```
Test: agwpe_connect_and_register
  Mock server accepts TCP, receives 'X' frame, sends 'x' response
  Verify AgwpeManager.connect_to_agwpe() succeeds
  Verify state transitions to AgwpeConnected

Test: agwpe_port_discovery
  Mock server responds to 'G' frame with two 'g' frames + terminator
  Verify state.available_ports contains both ports

Test: agwpe_registration_failure
  Mock server sends unexpected response to 'X' frame
  Verify connect_to_agwpe() returns error
  Verify state transitions to Error

Test: agwpe_connection_refused
  No server running on the port
  Verify connect_to_agwpe() returns error
```

**Mock AGWPE server:**
```rust
async fn start_mock_agwpe(port: u16) -> MockAgwpeServer { ... }

struct MockAgwpeServer {
    // Handles registration, port query, AX.25 connect
    // Configurable responses (success, failure, etc.)
}
```

---

## 4. Python E2E Tests

### 4.1 Directory structure

```
e2e/
├── conftest.py              # Shared fixtures (Direwolf pair, PipeWire, etc.)
├── pytest.ini               # pytest config
├── requirements-test.txt    # Python test dependencies
├── bpq_bridge.py            # BPQ simulator (AGWPE → TCP bridge)
├── test_audio.py            # Audio path validation (no packet-browser)
├── test_e2e.py              # Full path integration tests
└── helpers.py               # Shared utilities (free_port, wait_for_port, etc.)
```

### 4.2 Shared infrastructure (conftest.py)

Reuse directly from tncd's test_e2e.py:
- `free_port()` - random free TCP port
- `wait_for_port()` - block until port accepts connections
- `kill_proc()` - SIGTERM then SIGKILL
- `get_pw_ports()` - find PipeWire ports by PID
- `pw_disconnect_links()` - disconnect PipeWire links
- `pw_set_capture_volume()` - set capture volume
- `pw_configure_for_test()` - configure PipeWire settings
- `pw_restore_settings()` - restore original settings
- `pw_crosslink()` - cross-link two Direwolf instances
- `write_direwolf_config()` - generate Direwolf config file

**New fixtures:**

```python
@pytest.fixture()
def direwolf_pair(tmp_path):
    """Two Direwolf instances with AGWPE on both sides, audio cross-linked.
    
    Direwolf-A: AGWPE port (client connects here)
    Direwolf-B: AGWPE port (LinBPQ connects here)
    
    Yields dict with agwpe_port_a, agwpe_port_b, proc_a, proc_b
    """

@pytest.fixture()
def test_http_server(tmp_path):
    """Simple HTTP server serving test pages.
    
    Creates test HTML pages with links, forms, etc.
    Yields dict with url (base URL), port
    
    Pages served:
      / - Simple page with links to other test pages
      /page2 - Another page with different links
      /form - Page with a form (GET and POST)
      /large - Page with enough content to test compression
    """

@pytest.fixture()
def pb_server(tmp_path, test_http_server):
    """Start packet-browser-server on a random port.
    
    Uses CHROMIUM_PATH env var to find Chromium.
    Sets PORTAL_URL to test_http_server's URL.
    
    Yields dict with port, proc
    """

@pytest.fixture()
def linbpq_instance(tmp_path, direwolf_pair, pb_server):
    """Start LinBPQ with test configuration.
    
    Creates bpq32.cfg with:
    - Telnet port (disabled for tests)
    - Radio port connected to Direwolf-B via AGWPE
    - APPLICATION command connecting to packet-browser server
    
    Yields dict with proc, config_path
    """

@pytest.fixture()
def pb_client(direwolf_pair, tmp_path):
    """Start packet-browser-client connecting to Direwolf-A.
    
    Creates config file with:
      agwpe_host = 127.0.0.1
      agwpe_port = <direwolf_pair.agwpe_port_a>
      my_callsign = W1TEST
      target_callsign = N0CALL-7
      bpq_command = WEB
    
    Starts client with --listen-addr 127.0.0.1:<random_port>
    
    Yields dict with web_port, proc, config_path
    """
```

### 4.3 BPQ test configuration (bpq_test.cfg)

For e2e tests, we need a minimal LinBPQ configuration that:
1. Has a telnet port connected to Direwolf via AGWPE
2. Has an APPLICATION command that connects to the packet-browser server
3. Uses minimal settings for fast test execution

**Test bpq32.cfg:**
```
SIMPLE
NODECALL=N0CALL-7
NODEALIAS=TEST
LOCATOR=EN43bx
IDINTERVAL=0
BTINTERVAL=0

; Telnet port connected to Direwolf
PORT
 PORTNUM=1
 ID=Telnet Server
 DRIVER=TELNET
 CONFIG
 LOGGING=0
 LOCALNET=127.0.0.1/32
 HTTPPORT=0
 TCPPORT=0
 FBBPORT=0
 CMDPORT=0
 MAXSESSIONS=2
 CloseOnDisconnect=1
ENDPORT

; Radio port connected to Direwolf via AGWPE
PORT
 PORTNUM=2
 ID=Radio Port
 DRIVER=UZ7HO
 CHANNEL=A
 PORTCALL=N0CALL-7
 PERSIST=255
 SLOTTIME=100
 TXDELAY=100
 TXTAIL=50
 MAXFRAME=1
 FRACK=5000
 RESPTIME=100
 RETRIES=10
 PACLEN=128
 CONFIG
  ADDR 127.0.0.1 <direwolf_agwpe_port>
ENDPORT

; Application command to connect to packet-browser server
APPLICATION 1,WEB,C 1 HOST 0 S

LINMAIL
```

**Notes:**
- `CloseOnDisconnect=1` ensures the connection closes when the server disconnects
- `HTTPPORT=0`, `TCPPORT=0`, `FBBPORT=0`, `CMDPORT=0` disable all BPQ management interfaces
- The APPLICATION command connects to the packet-browser server on TCP 63004
- We'll need to dynamically replace `<direwolf_agwpe_port>` with the actual Direwolf AGWPE port

### 4.4 Test scenarios (test_e2e.py)

```python
pytestmark = [
    pytest.mark.skipif(not shutil.which("direwolf"), reason="direwolf not installed"),
    pytest.mark.skipif(not shutil.which("pw-link"), reason="pipewire not available"),
    pytest.mark.skipif(not shutil.which("linbpq"), reason="linbpq not installed"),
]

needs_chromium = pytest.mark.skipif(
    not (os.environ.get("CHROMIUM_PATH") or os.path.exists("/bin/chromium")),
    reason="chromium not available"
)


class TestAudioPath:
    """Validate PipeWire audio path between Direwolf instances."""
    
    def test_direwolf_pair_starts(self, direwolf_pair):
        """Both Direwolf instances are running after audio cross-link."""
        assert direwolf_pair["proc_a"].poll() is None
        assert direwolf_pair["proc_b"].poll() is None
    
    def test_agwpe_ports_accept_connections(self, direwolf_pair):
        """Both Direwolf AGWPE ports accept TCP connections."""
        # Connect to both ports, verify AGWPE protocol responds
        ...


class TestLinBPQ:
    """Validate LinBPQ starts and connects to Direwolf."""
    
    def test_linbpq_starts(self, linbpq_instance):
        """LinBPQ starts successfully with test configuration."""
        assert linbpq_instance["proc"].poll() is None
    
    def test_linbpq_connects_to_direwolf(self, linbpq_instance, direwolf_pair):
        """LinBPQ connects to Direwolf-B AGWPE port."""
        # Check LinBPQ logs for successful AGWPE connection
        ...


class TestFullE2E:
    """Full end-to-end tests through the complete data path."""
    
    @needs_chromium
    def test_connect_to_agwpe(self, direwolf_pair, pb_server, linbpq_instance, pb_client):
        """Client connects to AGWPE via Direwolf-A."""
        # POST /api/agwpe-status to trigger AGWPE connect
        # Verify state changes to AgwpeConnected
        # Verify ports are discovered
        ...
    
    @needs_chromium
    def test_ax25_connect(self, direwolf_pair, pb_server, linbpq_instance, pb_client):
        """Client establishes AX.25 connection through audio to LinBPQ."""
        # First connect to AGWPE
        # Then POST /api/connect with target=N0CALL-7
        # Verify state changes to Connected
        # Verify LinBPQ received the connection
        ...
    
    @needs_chromium
    def test_bpq_handshake(self, direwolf_pair, pb_server, linbpq_instance, pb_client):
        """Client completes BPQ handshake (WEB command, callsign, AGREE)."""
        # Connect AGWPE + AX.25
        # Verify client sends "WEB\n" command
        # Verify client sends callsign
        # Verify client sends "AGREE\n"
        # Verify state shows portal page loaded
        ...
    
    @needs_chromium
    def test_browse_portal_page(self, direwolf_pair, pb_server, linbpq_instance, 
                                 pb_client, test_http_server):
        """Client displays the portal page received from server."""
        # Connect AGWPE + AX.25 + complete BPQ handshake
        # Verify portal page is displayed in web UI
        # Verify links are rewritten to /browse?url=...
        ...
    
    @needs_chromium
    def test_browse_follow_link(self, direwolf_pair, pb_server, linbpq_instance,
                                 pb_client, test_http_server):
        """Client follows a rewritten link through the radio path."""
        # Browse portal page
        # Extract a rewritten link from the response
        # Follow the link (GET /browse?url=...)
        # Verify the second page loads correctly
        ...
    
    @needs_chromium
    def test_browse_form_submission(self, direwolf_pair, pb_server, linbpq_instance,
                                     pb_client, test_http_server):
        """Client submits a form through the radio path."""
        # Browse to /form page
        # Submit form via POST /browse?url=...
        # Verify response contains form submission result
        ...
    
    @needs_chromium
    def test_debug_log_sse(self, direwolf_pair, pb_server, linbpq_instance, pb_client):
        """SSE debug log stream receives entries during operations."""
        # Connect to /events SSE endpoint
        # Perform AGWPE connect + AX.25 connect + BPQ handshake
        # Verify SSE stream contains log entries for state changes
        ...
    
    @needs_chromium
    def test_config_persistence(self, pb_client, tmp_path):
        """Configuration changes persist to INI file."""
        # POST /api/config with new AGWPE host/port
        # Verify config file was written
        # GET /api/config returns updated values
        ...
```

### 4.5 Test HTTP server content

The test HTTP server serves these pages:

**`/` (portal page):**
```html
<!DOCTYPE html>
<html>
<head><title>Test Portal</title></head>
<body>
  <h1>Packet Browser Test Portal</h1>
  <p>Welcome to the test portal.</p>
  <ul>
    <li><a href="/page2">Page 2</a></li>
    <li><a href="/form">Search Form</a></li>
    <li><a href="/large">Large Page</a></li>
  </ul>
</body>
</html>
```

**`/page2`:**
```html
<!DOCTYPE html>
<html>
<head><title>Page 2</title></head>
<body>
  <h1>Page 2</h1>
  <p>This is the second test page.</p>
  <a href="/">Back to portal</a>
</body>
</html>
```

**`/form`:**
```html
<!DOCTYPE html>
<html>
<head><title>Search</title></head>
<body>
  <h1>Search</h1>
  <form action="/form" method="POST">
    <input type="text" name="q" placeholder="Search...">
    <button type="submit">Search</button>
  </form>
  <div id="result"></div>
</body>
</html>
```

**`/large`:**
Generate a page with ~20KB of content (repeated paragraphs) to test compression.

### 4.6 Timing considerations

AX.25 connected mode over virtual audio takes time:
- Direwolf TX delay: 100ms
- Audio propagation: near-instant (virtual)
- Direwolf RX processing: ~100ms
- AGWPE notification: ~10ms

Expected timings:
- AGWPE connect: < 1s
- AX.25 connect (SABM/UA handshake): 2-5s
- Page request (small page): 10-30s (depends on Chromium + compression + radio speed)
- Page request (large page): 30-120s

Use generous timeouts (120s for page loads, 30s for connections).

---

## 5. File Structure

```
docker-packet-browser/
├── server/
│   └── tests/
│       ├── config_test.rs          (existing)
│       ├── session_test.rs         (existing)
│       ├── filter_test.rs          (existing)
│       ├── logger_test.rs          (existing)
│       └── protocol_test.rs        (NEW: server protocol handler test)
├── client/
│   └── tests/
│       ├── proxy_test.rs           (NEW: axum route tests)
│       └── agwpe_mock_test.rs      (NEW: AGWPE background task tests)
├── e2e/
│   ├── conftest.py                 (NEW: shared fixtures)
│   ├── pytest.ini                  (NEW: pytest config)
│   ├── requirements-test.txt       (NEW: Python test deps)
│   ├── helpers.py                  (NEW: utility functions)
│   ├── bpq_test.cfg                (NEW: LinBPQ test configuration template)
│   ├── test_audio.py               (NEW: audio path validation)
│   └── test_e2e.py                 (NEW: full path tests)
├── server/src/browser.rs           (MODIFY: configurable Chromium path)
├── client/src/config.rs            (MODIFY: add bpq_command field)
├── client/src/agwpe.rs             (MODIFY: implement BPQ handshake)
├── flake.nix                       (MODIFY: add direwolf, pipewire, linbpq to dev shell)
└── Makefile                        (MODIFY: add e2e target)
```

---

## 6. Implementation Steps

### Step 1: Prerequisites

1. Make Chromium path configurable via `CHROMIUM_PATH` env var in `server/src/browser.rs`
2. Add `bpq_command` field to `FileConfig` in `client/src/config.rs`
3. Add `--bpq-command` CLI flag to `CliArgs` in `client/src/config.rs`
4. Add `direwolf`, `pipewire`, and `linbpq` to Nix dev shell in `flake.nix`
5. Create `e2e/` directory with `pytest.ini`, `requirements-test.txt`

### Step 2: Rust client proxy tests (`client/tests/proxy_test.rs`)

1. Create test helper that starts axum on random port with mock state
2. Implement route tests (redirect, HTML pages, API endpoints)
3. Test browse redirect when disconnected
4. Test SSE endpoint

### Step 3: Rust client AGWPE mock tests (`client/tests/agwpe_mock_test.rs`)

1. Implement mock AGWPE TCP server
2. Test connect + register flow
3. Test port discovery
4. Test error cases (registration failure, connection refused)

### Step 4: Implement BPQ handshake in client (`client/src/agwpe.rs`)

1. Modify `handle_ax25_connect` to send BPQ command after AX.25 connect
2. Implement line-by-line reading for text prompts (callsign, AGREE)
3. Implement Response frame parsing for portal page
4. Store portal page in state for web UI display
5. Test with mock BPQ server that simulates the handshake

### Step 5: E2E test infrastructure

1. Copy PipeWire + Direwolf helpers from tncd into `e2e/helpers.py`
2. Create `bpq_test.cfg` template for LinBPQ test configuration
3. Implement `direwolf_pair` fixture in `conftest.py`
4. Implement `test_http_server` fixture
5. Implement `linbpq_instance` fixture (starts LinBPQ with test config)
6. Implement `pb_server` and `pb_client` fixtures

### Step 6: E2E test scenarios

1. `test_audio.py`: validate Direwolf pair starts and AGWPE ports work
2. `test_e2e.py`: LinBPQ starts and connects to Direwolf
3. `test_e2e.py`: AGWPE connect test
4. `test_e2e.py`: AX.25 connect test (through audio to LinBPQ)
5. `test_e2e.py`: BPQ handshake test (WEB command, callsign, AGREE)
6. `test_e2e.py`: browse portal page test
7. `test_e2e.py`: follow link test
8. `test_e2e.py`: form submission test
9. `test_e2e.py`: SSE debug log test
10. `test_e2e.py`: config persistence test

### Step 7: Makefile + CI

1. Add `make e2e` target that runs `cd e2e && pytest`
2. E2e tests are local-only (not in CI) since they need PipeWire session

---

## 7. Testing Strategy

### Unit tests (existing, 47 tests)
- Run with `cargo test`
- No external dependencies

### Rust integration tests (new, ~12 tests)
- Run with `cargo test`
- Proxy tests: need tokio runtime, no external deps
- AGWPE mock tests: need tokio runtime, no external deps

### Python e2e tests (new, ~10 tests)
- Run with `cd e2e && pytest`
- Require: Direwolf, PipeWire, Chromium, packet-browser binaries
- Auto-skip if dependencies missing
- NOT run in CI (need PipeWire session)

### Manual testing
- Connect to real AGWPE + BPQ + server
- Browse several sites, verify content loads
- Test forms (search, login)
- Verify debug log shows all operations
- Test config persistence across restarts

---

## 8. Dependencies

### New Rust dependencies (none)
All needed crates are already in Cargo.toml.

### New Python dependencies
```
pytest>=7.0
pytest-asyncio>=0.21
```

### System dependencies (for e2e)
- direwolf (TNC emulator)
- pipewire + tools (pw-link, pw-dump, pw-cli, pw-metadata)
- linbpq (BPQ node software)
- chromium (for server)

---

## 9. Migration Notes

**New files:**
- `client/tests/proxy_test.rs`
- `client/tests/agwpe_mock_test.rs`
- `e2e/` directory with all Python test files
- `e2e/bpq_test.cfg` - LinBPQ test configuration template

**Modified files:**
- `server/src/browser.rs` - configurable Chromium path
- `client/src/config.rs` - add bpq_command field to FileConfig and CliArgs
- `client/src/agwpe.rs` - implement BPQ handshake after AX.25 connect
- `flake.nix` - add direwolf, pipewire, linbpq to dev shell
- `Makefile` - add e2e target

**No breaking changes to existing code.**
