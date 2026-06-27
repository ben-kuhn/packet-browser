# Packet Browser v2: Client/Server Architecture Rewrite

**Date:** 2026-06-26
**Status:** Approved
**Author:** Design collaboration with KU0HN

## Overview

Replace the terminal-based browser with a client/server architecture that uses packet radio as a raw data channel. The server fetches and sanitizes web pages, compresses them with brotli, and streams them back. The client connects to AGWPE for the radio link and runs a local web proxy that renders pages in the user's browser.

## Goals

- Use packet radio as a raw data channel, not a terminal
- Preserve page formatting (HTML + inlined CSS)
- Minimize bandwidth: strip JS, images, heavy resources; brotli compress everything
- Client runs locally, user browses via their real browser (clickable links, forms work)
- Server stays behind BPQ (no change to BPQ integration)
- Terminal mode fully removed

## Non-Goals

- Supporting JavaScript-driven functionality (AJAX, dynamic content)
- Image rendering
- Multiple concurrent sessions over one radio link (serialized)

---

## 1. Architecture

```
User's Browser  ←HTTP→  Client (Rust)  ←AGWPE→  TNC  ←RF→  BPQ  ←TCP→  Server (Rust)
localhost:8080           axum+AGWPE     TCP:8000        node          TCP:63004
```

**Data flow:**
1. User clicks link in browser → `GET /browse?url=...` to client
2. Client sends `GET <url>\n` over AX.25 to server
3. Server validates URL, fetches with Chromium, scrubs HTML, brotli-compresses
4. Server sends `<status><size><compressed data>` back over AX.25
5. Client decompresses, rewrites URLs to route through local proxy, serves to browser

---

## 2. Wire Protocol

Minimal framing. Every byte counts at ~600 B/s effective throughput.

### Request (client → server)

```
GET <url>\n
```

or for form submissions:

```
POST <url>\n
<u32 big-endian: body length>
<url-encoded form body>
```

### Response (server → client)

```
<u8 status>
<u32 big-endian: compressed payload size>
<brotli-compressed HTML, exactly that many bytes>
```

**Status codes:**
- `0x00` = OK
- `0x01` = Error (payload is error message, also compressed)
- `0x02` = Blocked (payload is block reason, also compressed)

**Total overhead:** 5 bytes per response, 2-4 bytes per request.

### Connection Management

- Single persistent AX.25 connection between client and server
- Requests serialized (one at a time) — bandwidth too limited for concurrency
- Client reads response by: read 1 byte status, read 4 bytes size, read exactly `size` bytes
- If radio link drops, client reconnects with exponential backoff

---

## 3. HTML Scrubbing (Server-Side)

JavaScript runs inside Chrome's rendered DOM after page load:

1. **Inline CSS:** Fetch all `<link rel="stylesheet">` hrefs via `fetch()`, create `<style>` tags, remove `<link>` tags
2. **Strip CSS url() references:** Replace `url(...)` in inlined CSS with empty string (no images/fonts to fetch)
3. **Remove heavy elements:** `<script>`, `<iframe>`, `<video>`, `<audio>`, `<canvas>`, `<svg>`, `<object>`, `<embed>`, `<noscript>`, `<template>`
4. **Replace images:** `<img>` → text node `[image: alt text]` or `[image]`
5. **Strip event handlers:** Remove all `on*` attributes from all elements
6. **Size check:** If resulting HTML > 32 KB, strip all `<style>` and `class`/`id`/`style` attributes, inject minimal fallback CSS (~200 bytes)
7. **Return:** `document.documentElement.outerHTML`

### Fallback CSS (injected when HTML is too large)

```css
body{font-family:sans-serif;max-width:40em;margin:0 auto;padding:1em;line-height:1.5}
a{color:#06c}h1,h2,h3{margin:1em 0 .5em}table{border-collapse:collapse}
td,th{border:1px solid #ccc;padding:.3em}img{display:none}
```

---

## 4. Brotli Compression

- **Quality:** 11 (maximum). CPU cost irrelevant at 600 B/s.
- **Expected sizes:**

| Content type | Raw | Brotli Q11 | Transfer @600B/s |
|-------------|-----|------------|-----------------|
| Simple page | 15 KB | ~5 KB | 8 sec |
| Medium page | 40 KB | ~12 KB | 20 sec |
| Complex page | 100 KB | ~25 KB | 42 sec |
| Very large (fallback) | 30 KB | ~10 KB | 17 sec |

---

## 5. Workspace Structure

```
docker-packet-browser/
├── Cargo.toml                  # workspace root
├── shared/
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs              # protocol types, brotli helpers, constants
├── server/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             # TCP:63004 listener, protocol handler
│       ├── lib.rs              # re-exports for testing
│       ├── browser.rs          # Chromium + HTML scrub JS (rewritten)
│       ├── config.rs           # env config (updated fields)
│       ├── filter.rs           # KEEP as-is
│       ├── logger.rs           # KEEP as-is
│       ├── blocklist.rs        # KEEP as-is
│       └── session.rs          # simplified: callsign + idle timer
├── client/
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs             # entry: starts AGWPE + axum proxy
│       ├── agwpe.rs            # AGWPE TCP API client
│       ├── proxy.rs            # axum web server
│       ├── rewrite.rs          # HTML URL rewriting (lol_html)
│       └── protocol.rs         # wire protocol client-side
├── flake.nix                   # updated for workspace
├── docker-compose.yml          # server only
├── Makefile                    # workspace-aware
└── ...
```

---

## 6. Module Details

### 6.1 Shared Crate (`shared/`)

**`lib.rs`** — shared types and helpers:

```rust
pub mod protocol;
pub mod compress;
```

**`protocol.rs`:**
- `Request` enum: `Get { url: String }`, `Post { url: String, body: Vec<u8> }`
- `Response` struct: `status: Status, payload: Vec<u8>` (compressed)
- `Status` enum: `Ok`, `Error`, `Blocked`
- `encode_request(req) -> Vec<u8>`
- `decode_request(data: &[u8]) -> Result<Request>`
- `encode_response(resp) -> Vec<u8>`
- `decode_response_header(data: &[u8]) -> Result<(Status, u32)>` (status + size)

**`compress.rs`:**
- `brotli_compress(data: &[u8], quality: u32) -> Vec<u8>`
- `brotli_decompress(data: &[u8]) -> Result<Vec<u8>>`

### 6.2 Server Crate (`server/`)

**`main.rs`** — rewritten:
- TCP listener on `LISTEN_PORT` (default 63004)
- Accept connection → read callsign → validate → create session
- Protocol loop: read request → validate URL → fetch page → compress → send response
- No more terminal command parsing, pagination, or display formatting
- Health check mode (`--healthcheck`) preserved

**`browser.rs`** — rewritten:
- `BrowserInstance` struct preserved (Chrome launch, DevTools connection, crash recovery)
- `fetch_page(url) -> Result<String>` returns sanitized HTML string (not `PageContent`)
- New JS scrubber replaces `JS_EXTRACT_PAGE`, `JS_COLLECT`, `JS_EXTRACT_INPUTS`, `JS_INTERACT`
- Remove `InputField`, `InputKind`, `PageContent` types
- Remove `interact_with_input()` method

**`config.rs`** — updated:
- Remove: `lines_per_page`, `debug_mode`
- Add: `brotli_quality: u32` (default 11)
- Keep: `listen_port`, `portal_url`, `idle_timeout_minutes`, `blocked_ranges`, `blocklist_*`, `log_*`, `syslog_*`

**`session.rs`** — simplified:
- Remove: `links`, `inputs`, `page_content`, `lines_per_page`, `full_page_mode`, `previous_url`
- Keep: `callsign`, `acknowledged`, `current_url`, `last_activity`
- Keep: `validate_callsign()`, `acknowledge()`, `touch()`, `is_timed_out()`

**Delete:** `display.rs`, `commands.rs`, and their tests

### 6.3 Client Crate (`client/`)

**`main.rs`:**
- Parse CLI args / env vars for config
- Start AGWPE connection (background task)
- Start axum web server
- Wire them together: web requests → AGWPE send → AGWPE recv → web response

**`agwpe.rs`** — AGWPE TCP API client:

AGWPE frame format (36-byte header, little-endian):
```
Offset  Size  Field
0       1     port (u8)
1       1     data_kind (u8) — frame type
2       1     pid (u8) — 0x00 for AX.25 I-frames
3       10    call_from (9-char callsign + null)
13      10    call_to (9-char callsign + null)
23      4     data_len (u32 LE)
27      4     user_data (u32 LE)
31      4     reserved (zeros)
35      1     (padding to 36)
```

Key frame types:
- `X` (0x58): Register callsign — send on connect
- `x` (0x78): Registration response
- `C` (0x43): Connect to remote
- `c` (0x63): Connected notification
- `d` (0x64): Data received from remote
- `D` (0x44): Send data to remote (also used for disconnect when data_len=0)
- `R` (0x52): Connection rejected

`AgwpeClient` struct:
- `connect(agwpe_host, agwpe_port)` — TCP connect to AGWPE
- `register(callsign)` — send 'X' frame
- `request_connect(to_callsign, port)` — send 'C' frame
- `send_data(data)` — send 'D' frame, fragmenting into chunks (default 256 bytes)
- `recv_data()` — receive and reassemble 'd' frames
- `disconnect()` — send disconnect frame
- Background task reads from TCP, dispatches frames to channels

**`proxy.rs`** — axum web server:

Routes:
- `GET /` → landing page (embedded HTML: URL bar, connection status, current page)
- `GET /browse?url=<encoded>` → forward GET to server, return rewritten HTML
- `POST /browse?url=<encoded>` → forward POST body to server, return rewritten HTML

Config:
- `LISTEN_ADDR` (default `127.0.0.1:8080`)
- `MY_CALLSIGN` (required)
- `SERVER_CALLSIGN` (required)
- `AGWPE_HOST` (default `127.0.0.1`)
- `AGWPE_PORT` (default `8000`)
- `AGWPE_PORT_NUM` (default `0`) — AGWPE port number (radio port)
- `CHUNK_SIZE` (default `256`) — max bytes per AGWPE data frame

**`rewrite.rs`** — HTML URL rewriting:

Use `lol_html` streaming rewriter:
- Rewrite `<a href="...">` → `<a href="/browse?url=<encoded>">`
- Rewrite `<form action="...">` → `<form action="/browse?url=<encoded>" method="POST">`
- Resolve relative URLs against page base URL
- Handle: absolute URLs, relative paths, protocol-relative, fragments
- Strip: `javascript:` URLs, `mailto:`, `tel:`
- Inject `<base>` tag for CSS background-image resolution (optional)

**`protocol.rs`** — client-side protocol:
- `send_get(agwpe, url)` — encode GET request, send via AGWPE
- `send_post(agwpe, url, body)` — encode POST request, send via AGWPE
- `recv_response(agwpe)` — read status+size header, read compressed payload, decompress
- Timeout handling (configurable, default 120s — pages take time over radio)

---

## 7. Dependencies

### Shared
| Crate | Purpose |
|-------|---------|
| `brotli` | Compression/decompression |
| `thiserror` | Error types |

### Server
| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `headless_chrome` | Chromium DevTools |
| `serde`, `serde_json` | Serialization |
| `chrono` | Timestamps |
| `regex` | Callsign validation |
| `thiserror` | Error types |
| `tracing`, `tracing-subscriber` | Logging |
| `reqwest` (blocking) | Blocklist fetching |
| `packet-browser-shared` | Protocol + compression |

### Client
| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime |
| `axum` | Web server |
| `lol_html` | Streaming HTML rewriting |
| `url` | URL parsing/resolution |
| `serde`, `serde_json` | Serialization |
| `thiserror` | Error types |
| `tracing`, `tracing-subscriber` | Logging |
| `packet-browser-shared` | Protocol + compression |

---

## 8. Implementation Phases

### Phase 1: Workspace restructure
- Convert root `Cargo.toml` to workspace
- Move `src/` → `server/src/`, `tests/` → `server/tests/`
- Create empty `client/` and `shared/` crates
- Update `server/Cargo.toml` with `packet-browser-shared` dependency
- Verify `cargo test` passes with existing code

### Phase 2: Shared crate
- Implement `protocol.rs`: Request/Response types, encode/decode
- Implement `compress.rs`: brotli compress/decompress
- Tests: round-trip serialization, compression ratios

### Phase 3: Server cleanup
- Delete `display.rs`, `commands.rs`, their tests
- Rewrite `browser.rs`: HTML scrubber JS, `fetch_page()` returns `String`
- Simplify `session.rs`: remove terminal-specific fields
- Update `config.rs`: remove `lines_per_page`/`debug_mode`, add `brotli_quality`
- Rewrite `main.rs`: protocol handler (read request → fetch → compress → send response)
- Update tests

### Phase 4: Client AGWPE module
- Implement AGWPE frame struct (36-byte header)
- Frame serialization/deserialization
- `AgwpeClient`: connect, register, request_connect, send_data, recv_data, disconnect
- Data fragmentation (configurable chunk size)
- Reconnection with exponential backoff
- Tests with mock TCP server

### Phase 5: Client web proxy
- axum server with routes: `/`, `/browse` (GET/POST)
- `rewrite.rs`: lol_html URL rewriting
- Async wiring: web request → AGWPE send → AGWPE recv → web response
- Landing page HTML (embedded in binary)
- Configurable bind address

### Phase 6: Build/packaging
- Update `flake.nix`: workspace build, server Docker image, client binary
- Update `docker-compose.yml`: remove `LINES_PER_PAGE`/`DEBUG_MODE`, add `BROTLI_QUALITY`
- Update `Makefile`: workspace-aware targets
- Update CI/CD: test all crates, build server image

### Phase 7: Integration tests
- Server: protocol handler, HTML scrub output, compression
- Client: AGWPE frame parsing, URL rewriting, proxy endpoint
- End-to-end: mock AGWPE → server → client → verify HTML output

---

## 9. Configuration

### Server (environment variables)

| Variable | Default | Description |
|----------|---------|-------------|
| `LISTEN_PORT` | `63004` | TCP port for BPQ connections |
| `PORTAL_URL` | `https://www.zeroretries.radio` | Default home page |
| `IDLE_TIMEOUT_MINUTES` | `10` | Session idle timeout |
| `BROTLI_QUALITY` | `11` | Brotli compression level (0-11) |
| `BLOCKED_RANGES` | `127.0.0.0/8,...` | SSRF prevention |
| `BLOCKLIST_ENABLED` | `true` | Enable blocklist |
| `BLOCKLIST_URLS` | *(empty)* | Blocklist URLs |
| `BLOCKLIST_REFRESH_HOURS` | `24` | Blocklist refresh interval |
| `LOG_ROTATE_ENABLED` | `true` | Enable log rotation |
| `LOG_RETAIN_DAYS` | `30` | Log retention |

### Client (environment variables)

| Variable | Default | Description |
|----------|---------|-------------|
| `LISTEN_ADDR` | `127.0.0.1:8080` | Web proxy bind address |
| `MY_CALLSIGN` | *(required)* | Your amateur radio callsign |
| `SERVER_CALLSIGN` | *(required)* | BPQ node's callsign |
| `AGWPE_HOST` | `127.0.0.1` | AGWPE TCP API host |
| `AGWPE_PORT` | `8000` | AGWPE TCP API port |
| `AGWPE_PORT_NUM` | `0` | AGWPE radio port number |
| `CHUNK_SIZE` | `256` | Max bytes per AGWPE data frame |
| `REQUEST_TIMEOUT_SECS` | `120` | Timeout for page fetch |

---

## 10. Deployment

### Server (Docker, behind BPQ)

```yaml
services:
  packet-browser-server:
    image: ghcr.io/ben-kuhn/docker-packet-browser:latest
    ports:
      - "127.0.0.1:63004:63004"
    volumes:
      - ./logs:/var/log/packet-browser
      - ./hosts:/etc/hosts
    environment:
      - LISTEN_PORT=63004
      - PORTAL_URL=https://www.zeroretries.radio
      - BROTLI_QUALITY=11
      # ... other server config
    read_only: true
    # ... security hardening (same as before)
```

### Client (native binary on user's machine)

```bash
# Install
cargo install --path client

# Run
MY_CALLSIGN=N0CALL SERVER_CALLSIGN=NODE1 ./packet-browser-client
```

Or via Docker (if AGWPE is on same host):

```yaml
services:
  packet-browser-client:
    image: ghcr.io/ben-kuhn/packet-browser-client:latest
    ports:
      - "127.0.0.1:8080:8080"
    environment:
      - MY_CALLSIGN=N0CALL
      - SERVER_CALLSIGN=NODE1
      - AGWPE_HOST=host.docker.internal  # or IP of AGWPE machine
```

---

## 11. Testing Strategy

### Unit tests
- **Shared:** protocol encode/decode round-trip, compression round-trip
- **Server:** URL validation (existing), HTML scrub JS output (mock), response encoding
- **Client:** AGWPE frame parsing, URL rewriting rules, proxy routing

### Integration tests
- **Server:** start TCP listener, send protocol request, verify response
- **Client:** mock AGWPE server, send web request, verify rewritten HTML

### Manual testing
- Connect client to real AGWPE + BPQ + server
- Browse several sites, verify content loads
- Test forms (search, login)
- Test large pages (verify fallback CSS triggers)

---

## 12. Migration Notes

**Breaking changes:**
- Terminal mode removed entirely
- `display.rs`, `commands.rs` deleted
- `session.rs` simplified (no terminal state)
- `config.rs` fields changed
- Wire protocol changed (no longer line-oriented text)

**Preserved:**
- `filter.rs` (URL validation, SSRF prevention)
- `logger.rs` (structured JSON logging)
- `blocklist.rs` (background blocklist manager)
- `config.rs` helper functions (env var parsing)
- BPQ integration (TCP:63004)
- Docker deployment model for server
- Security hardening (read-only rootfs, cap_drop, etc.)

**New:**
- `shared/` crate (protocol + compression)
- `client/` crate (AGWPE + web proxy)
- Brotli compression
- HTML scrubbing (replaces text extraction)
- URL rewriting (lol_html)
