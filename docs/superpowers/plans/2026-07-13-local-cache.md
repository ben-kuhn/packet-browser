# Local response cache — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a two-layer HTTP-style cache (operator browser + on-disk SQLite in `packet-browser-client`) with wire-level conditional revalidation and origin `Cache-Control` honoring, so revisited URLs cost near-zero AX.25 airtime.

**Architecture:** The wire protocol grows one field per direction (`IF-NONE-MATCH <etag>` on requests, `<etag> <max_age>` on responses) plus a new `Status::NotModified`. The server derives etags by SHA-256 over the sanitized HTML and reads origin cache-control via a companion `reqwest` HEAD. The client persists brotli-compressed responses in a SQLite table with LRU eviction and consults it before every AX.25 fetch. The operator's browser gets `ETag` + `Cache-Control` headers so it can skip even the localhost round-trip.

**Tech Stack:** Rust 2021 (workspace: `shared`, `server`, `client`), `rusqlite` (bundled), `sha2`, `reqwest` (already present server-side), `axum` (already present client-side), `brotli` (already present shared). Python `pytest` for the one e2e touchpoint.

## Global Constraints

- Every byte on the wire must be `0x20..=0x7e` or `\n` — no NUL, CR, or `0xFF`. Existing tests enforce this; the new etag/max_age fields must respect it.
- Etag on the wire is exactly 16 base64url chars, or the single ASCII `-` when not applicable. Base64url alphabet: `A–Z a–z 0–9 - _`.
- `<max_age>` is a signed decimal integer written in ASCII. Positive = seconds, `0` = must-revalidate, negative = do-not-cache.
- Etag definition (server-side): `base64url_no_pad(sha256(sanitized_html_utf8_bytes)[..12])` → 16 chars. Computed pre-brotli.
- `packet-browser-client` and `packet-browser-server` ship together. Bump both `[package].version` to `0.2.0` in Task 4's commit (server) and Task 8's commit (client). No dual-format wire parsing.
- Missing / corrupted cache database logs a warning and the client continues with cache disabled for the session. Never refuse to start.
- Config additions all have defaults; existing `config.ini` files continue to work.
- Follow the repo's commit-message convention: `<component>: <lowercase imperative>` (e.g. `shared: extend Response with etag and max_age`).
- Run tests from `nix develop -c cargo test --all-features -- --test-threads=1` (mirrors README). Individual test invocations shown below assume this shell.

---

### Task 1: Extend the wire protocol

**Files:**
- Modify: `shared/src/protocol.rs`
- Modify: `shared/Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `enum Status { Ok = 0, Err = 1, Blocked = 2, NotModified = 3 }`
  - `enum Request { Get { url: String, if_none_match: Option<String> }, Post { url: String, body: Vec<u8> } }`
  - `struct Response { pub status: Status, pub etag: String, pub max_age: i32, pub payload: Vec<u8> }`
  - `impl Response { pub fn decode_header(data: &[u8]) -> Result<Option<(Status, u32, String, i32, usize)>, ProtocolError>; }`
    — the tuple is `(status, base64_payload_len, etag, max_age, header_end_offset)`.

- [ ] **Step 1: Add sha2 to shared/Cargo.toml**

Modify `shared/Cargo.toml`:

```toml
[dependencies]
base64 = "0.22"
brotli = "8"
sha2 = "0.10"
thiserror = "1"
```

- [ ] **Step 2: Write failing tests for the new Request form**

Add to the existing `#[cfg(test)] mod tests` in `shared/src/protocol.rs`:

```rust
#[test]
fn test_get_request_with_if_none_match_roundtrip() {
    let req = Request::Get {
        url: "https://example.com".to_string(),
        if_none_match: Some("aBcDeFgHiJkLmNoP".to_string()),
    };
    let encoded = req.encode();
    assert!(encoded.starts_with(b"GET https://example.com IF-NONE-MATCH aBcDeFgHiJkLmNoP\n"));
    let decoded = Request::decode(&encoded).unwrap();
    assert_eq!(req, decoded);
}

#[test]
fn test_get_request_without_if_none_match_roundtrip() {
    let req = Request::Get {
        url: "https://example.com".to_string(),
        if_none_match: None,
    };
    let encoded = req.encode();
    assert_eq!(encoded, b"GET https://example.com\n");
    let decoded = Request::decode(&encoded).unwrap();
    assert_eq!(req, decoded);
}

#[test]
fn test_response_with_etag_and_max_age_roundtrip() {
    let resp = Response {
        status: Status::Ok,
        etag: "aBcDeFgHiJkLmNoP".to_string(),
        max_age: 3600,
        payload: b"body".to_vec(),
    };
    let encoded = resp.encode();
    assert!(encoded.starts_with(b"RESP0 "));
    let (status, b64_len, etag, max_age, header_end) =
        Response::decode_header(&encoded).unwrap().unwrap();
    assert_eq!(status, Status::Ok);
    assert_eq!(etag, "aBcDeFgHiJkLmNoP");
    assert_eq!(max_age, 3600);
    let payload = Response::decode_payload(&encoded[header_end..header_end + b64_len as usize]).unwrap();
    assert_eq!(payload, b"body");
}

#[test]
fn test_not_modified_status_roundtrip() {
    let resp = Response {
        status: Status::NotModified,
        etag: "aBcDeFgHiJkLmNoP".to_string(),
        max_age: 3600,
        payload: Vec::new(),
    };
    let encoded = resp.encode();
    assert!(encoded.starts_with(b"RESP3 0 aBcDeFgHiJkLmNoP 3600\n"));
    let (status, b64_len, etag, max_age, _) =
        Response::decode_header(&encoded).unwrap().unwrap();
    assert_eq!(status, Status::NotModified);
    assert_eq!(b64_len, 0);
    assert_eq!(etag, "aBcDeFgHiJkLmNoP");
    assert_eq!(max_age, 3600);
}

#[test]
fn test_response_negative_max_age_roundtrip() {
    let resp = Response {
        status: Status::Ok,
        etag: "-".to_string(),
        max_age: -1,
        payload: b"x".to_vec(),
    };
    let encoded = resp.encode();
    let (_, _, etag, max_age, _) = Response::decode_header(&encoded).unwrap().unwrap();
    assert_eq!(etag, "-");
    assert_eq!(max_age, -1);
}

#[test]
fn test_wire_bytes_still_telnet_safe_with_new_fields() {
    let payload: Vec<u8> = (0u8..=255).collect();
    let encoded = Response {
        status: Status::Ok,
        etag: "aBcDeFgHiJkLmNoP".to_string(),
        max_age: 42,
        payload,
    }
    .encode();
    for &b in &encoded {
        assert!(b == b'\n' || (0x20..=0x7e).contains(&b),
                "wire byte 0x{:02x} not telnet-safe", b);
    }
}
```

Also update the existing `test_response_encode_decode` and `test_response_resyncs_past_garbage` tests to construct `Response` with `etag: "-".to_string(), max_age: -1,` and to unpack the five-tuple from `decode_header` (drop the extra fields with `_`). Update `test_post_request_roundtrip` and `test_get_request_roundtrip` to use `if_none_match: None`.

- [ ] **Step 3: Run tests to confirm they fail to compile**

Run: `cargo test -p packet-browser-shared`
Expected: compile errors on the new fields.

- [ ] **Step 4: Implement Status::NotModified**

Modify `Status` and its `TryFrom<u8>` in `shared/src/protocol.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok = 0x00,
    Err = 0x01,
    Blocked = 0x02,
    NotModified = 0x03,
}

impl TryFrom<u8> for Status {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, ProtocolError> {
        match value {
            0x00 => Ok(Status::Ok),
            0x01 => Ok(Status::Err),
            0x02 => Ok(Status::Blocked),
            0x03 => Ok(Status::NotModified),
            _ => Err(ProtocolError::InvalidResponse),
        }
    }
}
```

- [ ] **Step 5: Extend Request::Get with if_none_match**

Replace the `Request` enum and its `encode`/`decode`:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Get {
        url: String,
        if_none_match: Option<String>,
    },
    Post { url: String, body: Vec<u8> },
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Request::Get { url, if_none_match: None } => {
                let mut data = Vec::with_capacity(5 + url.len());
                data.extend_from_slice(b"GET ");
                data.extend_from_slice(url.as_bytes());
                data.push(b'\n');
                data
            }
            Request::Get { url, if_none_match: Some(etag) } => {
                let mut data = Vec::with_capacity(4 + url.len() + 16 + etag.len() + 1);
                data.extend_from_slice(b"GET ");
                data.extend_from_slice(url.as_bytes());
                data.extend_from_slice(b" IF-NONE-MATCH ");
                data.extend_from_slice(etag.as_bytes());
                data.push(b'\n');
                data
            }
            Request::Post { url, body } => {
                let body_len = body.len() as u32;
                let mut data = Vec::with_capacity(5 + url.len() + 4 + body.len());
                data.extend_from_slice(b"POST ");
                data.extend_from_slice(url.as_bytes());
                data.push(b'\n');
                data.extend_from_slice(&body_len.to_be_bytes());
                data.extend_from_slice(body);
                data
            }
        }
    }

    pub fn decode(data: &[u8]) -> Result<Self, ProtocolError> {
        if data.is_empty() {
            return Err(ProtocolError::InvalidRequest);
        }
        if data.starts_with(b"GET ") {
            let line_end = data[4..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or(ProtocolError::InvalidRequest)?;
            let line = std::str::from_utf8(&data[4..4 + line_end])
                .map_err(|_| ProtocolError::InvalidRequest)?;
            // Split on the sentinel token; the URL itself must not contain " IF-NONE-MATCH ".
            if let Some((url, etag)) = line.split_once(" IF-NONE-MATCH ") {
                Ok(Request::Get {
                    url: url.to_string(),
                    if_none_match: Some(etag.to_string()),
                })
            } else {
                Ok(Request::Get {
                    url: line.to_string(),
                    if_none_match: None,
                })
            }
        } else if data.starts_with(b"POST ") {
            let url_end = data[5..]
                .iter()
                .position(|&b| b == b'\n')
                .ok_or(ProtocolError::InvalidRequest)?;
            let url = std::str::from_utf8(&data[5..5 + url_end])
                .map_err(|_| ProtocolError::InvalidRequest)?
                .to_string();
            let body_start = 5 + url_end + 1;
            if data.len() < body_start + 4 {
                return Err(ProtocolError::InvalidRequest);
            }
            let body_len = u32::from_be_bytes([
                data[body_start],
                data[body_start + 1],
                data[body_start + 2],
                data[body_start + 3],
            ]) as usize;
            if data.len() < body_start + 4 + body_len {
                return Err(ProtocolError::InvalidRequest);
            }
            let body = data[body_start + 4..body_start + 4 + body_len].to_vec();
            Ok(Request::Post { url, body })
        } else {
            Err(ProtocolError::InvalidRequest)
        }
    }
}
```

- [ ] **Step 6: Extend Response with etag and max_age fields**

Replace the `Response` struct and its `encode` / `decode_header`:

```rust
#[derive(Debug, Clone)]
pub struct Response {
    pub status: Status,
    pub etag: String,
    pub max_age: i32,
    pub payload: Vec<u8>,
}

impl Response {
    pub const MAGIC: &'static [u8] = b"RESP";

    pub fn encode(&self) -> Vec<u8> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        let encoded_payload = STANDARD.encode(&self.payload);
        let status_digit = match self.status {
            Status::Ok => '0',
            Status::Err => '1',
            Status::Blocked => '2',
            Status::NotModified => '3',
        };
        let mut data = Vec::with_capacity(
            Self::MAGIC.len() + 32 + self.etag.len() + encoded_payload.len() + 2,
        );
        data.extend_from_slice(Self::MAGIC);
        data.push(status_digit as u8);
        data.push(b' ');
        data.extend_from_slice(encoded_payload.len().to_string().as_bytes());
        data.push(b' ');
        data.extend_from_slice(self.etag.as_bytes());
        data.push(b' ');
        data.extend_from_slice(self.max_age.to_string().as_bytes());
        data.push(b'\n');
        data.extend_from_slice(encoded_payload.as_bytes());
        data.push(b'\n');
        data
    }

    pub fn decode_header(
        data: &[u8],
    ) -> Result<Option<(Status, u32, String, i32, usize)>, ProtocolError> {
        let magic_pos = match data
            .windows(Self::MAGIC.len())
            .position(|w| w == Self::MAGIC)
        {
            Some(p) => p,
            None => return Ok(None),
        };
        let after_magic = magic_pos + Self::MAGIC.len();
        if data.len() < after_magic + 2 {
            return Ok(None);
        }
        let status = match data[after_magic] {
            b'0' => Status::Ok,
            b'1' => Status::Err,
            b'2' => Status::Blocked,
            b'3' => Status::NotModified,
            _ => return Err(ProtocolError::InvalidResponse),
        };
        if data[after_magic + 1] != b' ' {
            return Err(ProtocolError::InvalidResponse);
        }
        let fields_start = after_magic + 2;
        let nl_offset = match data[fields_start..].iter().position(|&b| b == b'\n' || b == b'\r') {
            Some(o) => o,
            None => return Ok(None),
        };
        let header_line = std::str::from_utf8(&data[fields_start..fields_start + nl_offset])
            .map_err(|_| ProtocolError::InvalidResponse)?;
        let mut parts = header_line.split(' ');
        let len_str = parts.next().ok_or(ProtocolError::InvalidResponse)?;
        let etag = parts.next().ok_or(ProtocolError::InvalidResponse)?.to_string();
        let max_age_str = parts.next().ok_or(ProtocolError::InvalidResponse)?;
        if parts.next().is_some() {
            return Err(ProtocolError::InvalidResponse);
        }
        let payload_len: u32 = len_str.parse().map_err(|_| ProtocolError::InvalidResponse)?;
        let max_age: i32 = max_age_str.parse().map_err(|_| ProtocolError::InvalidResponse)?;
        let header_end = fields_start + nl_offset + 1;
        Ok(Some((status, payload_len, etag, max_age, header_end)))
    }

    pub fn decode_payload(base64_bytes: &[u8]) -> Result<Vec<u8>, ProtocolError> {
        use base64::{engine::general_purpose::STANDARD, Engine as _};
        STANDARD
            .decode(base64_bytes)
            .map_err(|_| ProtocolError::InvalidResponse)
    }
}
```

- [ ] **Step 7: Add etag helper**

Append to `shared/src/protocol.rs`:

```rust
/// Compute the wire etag for a sanitized HTML body.
///
/// Definition: base64url-nopad(sha256(html_utf8_bytes)[..12]) → exactly 16
/// ASCII chars. Base64url is the RFC 4648 "-_" alphabet.
pub fn sanitized_html_etag(html: &str) -> String {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use sha2::{Digest, Sha256};

    let hash = Sha256::digest(html.as_bytes());
    URL_SAFE_NO_PAD.encode(&hash[..12])
}

#[cfg(test)]
mod etag_tests {
    use super::*;

    #[test]
    fn etag_is_sixteen_chars() {
        assert_eq!(sanitized_html_etag("hello").len(), 16);
        assert_eq!(sanitized_html_etag("").len(), 16);
    }

    #[test]
    fn etag_is_deterministic() {
        assert_eq!(sanitized_html_etag("abc"), sanitized_html_etag("abc"));
    }

    #[test]
    fn etag_differs_on_content_change() {
        assert_ne!(sanitized_html_etag("abc"), sanitized_html_etag("abd"));
    }

    #[test]
    fn etag_uses_base64url_alphabet() {
        let e = sanitized_html_etag("hello world");
        for c in e.chars() {
            assert!(
                c.is_ascii_alphanumeric() || c == '-' || c == '_',
                "unexpected char {:?} in etag {}", c, e,
            );
        }
    }
}
```

- [ ] **Step 8: Run all shared tests**

Run: `cargo test -p packet-browser-shared`
Expected: all pass.

- [ ] **Step 9: Commit**

```bash
git add shared/Cargo.toml shared/src/protocol.rs Cargo.lock
git commit -m "shared: extend protocol with etag, max_age, and NotModified status"
```

---

### Task 2: Server config gains cache-control probe knobs

**Files:**
- Modify: `server/src/config.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `Config.origin_cc_head_timeout_ms: u64`
  - `Config.default_max_age_seconds: i32`
  - `Config.max_max_age_seconds: i32`

- [ ] **Step 1: Write failing tests**

Append to `server/src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_config_defaults() {
        // from_env reads process env; test isolation is best-effort — we assume
        // these keys are unset in the test process.
        std::env::remove_var("ORIGIN_CC_HEAD_TIMEOUT_MS");
        std::env::remove_var("DEFAULT_MAX_AGE_SECONDS");
        std::env::remove_var("MAX_MAX_AGE_SECONDS");
        let c = Config::from_env();
        assert_eq!(c.origin_cc_head_timeout_ms, 3000);
        assert_eq!(c.default_max_age_seconds, 3600);
        assert_eq!(c.max_max_age_seconds, 2_592_000);
    }
}
```

- [ ] **Step 2: Run test — expect compile failure**

Run: `cargo test -p packet-browser-server config::tests::cache_config_defaults`
Expected: FAIL — struct has no such fields.

- [ ] **Step 3: Add the fields, defaults, and env parsers**

In `server/src/config.rs`, append to the `Config` struct:

```rust
pub struct Config {
    // ... existing fields
    pub origin_cc_head_timeout_ms: u64,
    pub default_max_age_seconds: i32,
    pub max_max_age_seconds: i32,
}
```

Add to `Config::from_env`:

```rust
origin_cc_head_timeout_ms: parse_env_u64("ORIGIN_CC_HEAD_TIMEOUT_MS", 3000),
default_max_age_seconds: parse_env_i32("DEFAULT_MAX_AGE_SECONDS", 3600),
max_max_age_seconds: parse_env_i32("MAX_MAX_AGE_SECONDS", 2_592_000),
```

Add the missing helper next to the existing `parse_env_u64`:

```rust
fn parse_env_i32(key: &str, default: i32) -> i32 {
    env::var(key)
        .ok()
        .and_then(|v| v.parse::<i32>().ok())
        .unwrap_or(default)
}
```

- [ ] **Step 4: Run test — expect pass**

Run: `cargo test -p packet-browser-server config::tests::cache_config_defaults`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add server/src/config.rs
git commit -m "server: add origin-cache-control probe knobs to Config"
```

---

### Task 3: Server origin cache-control probe module

**Files:**
- Create: `server/src/origin_cc.rs`
- Modify: `server/src/lib.rs`

**Interfaces:**
- Consumes: `crate::config::Config`.
- Produces:
  - `pub struct CcDefaults { pub default_max_age: i32, pub max_max_age: i32 }`
  - `pub fn parse_cache_control(cc: Option<&str>, expires: Option<&str>, date: Option<&str>, defaults: &CcDefaults) -> i32`
  - `pub struct OriginDirectives { pub max_age: i32 }`
  - `pub fn probe_origin_cc(url: &str, config: &Config) -> OriginDirectives`

- [ ] **Step 1: Create module skeleton with failing tests**

Create `server/src/origin_cc.rs`:

```rust
use crate::browser::current_proxy_port;
use crate::config::Config;
use std::time::Duration;

pub struct CcDefaults {
    pub default_max_age: i32,
    pub max_max_age: i32,
}

pub struct OriginDirectives {
    pub max_age: i32,
}

/// Parse origin cache directives into a single wire `max_age` value.
///
/// Precedence order:
/// 1. `Cache-Control` directives (`no-store`/`private` → -1; `no-cache` → 0;
///    `s-maxage=N` > `max-age=N` → clamped positive).
/// 2. `Expires` minus `Date` if both parse as HTTP-dates.
/// 3. `defaults.default_max_age`.
///
/// The `max-age`/`s-maxage`/expires-derived values are clamped to
/// `defaults.max_max_age`.
pub fn parse_cache_control(
    cc: Option<&str>,
    expires: Option<&str>,
    date: Option<&str>,
    defaults: &CcDefaults,
) -> i32 {
    unimplemented!()
}

pub fn probe_origin_cc(url: &str, config: &Config) -> OriginDirectives {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defs() -> CcDefaults {
        CcDefaults { default_max_age: 3600, max_max_age: 2_592_000 }
    }

    #[test]
    fn cc_no_store_gives_negative() {
        assert_eq!(parse_cache_control(Some("no-store"), None, None, &defs()), -1);
    }

    #[test]
    fn cc_private_gives_negative() {
        assert_eq!(parse_cache_control(Some("private, max-age=600"), None, None, &defs()), -1);
    }

    #[test]
    fn cc_no_cache_gives_zero() {
        assert_eq!(parse_cache_control(Some("no-cache"), None, None, &defs()), 0);
    }

    #[test]
    fn cc_max_age_forwarded() {
        assert_eq!(parse_cache_control(Some("max-age=600"), None, None, &defs()), 600);
    }

    #[test]
    fn cc_s_maxage_takes_precedence_over_max_age() {
        assert_eq!(
            parse_cache_control(Some("s-maxage=60, max-age=600"), None, None, &defs()),
            60
        );
    }

    #[test]
    fn cc_max_age_clamped_to_cap() {
        assert_eq!(
            parse_cache_control(Some("max-age=99999999"), None, None, &defs()),
            2_592_000
        );
    }

    #[test]
    fn cc_expires_falls_back_when_no_cc() {
        // Expires = Date + 60s → 60.
        let out = parse_cache_control(
            None,
            Some("Sun, 06 Nov 2026 08:50:00 GMT"),
            Some("Sun, 06 Nov 2026 08:49:00 GMT"),
            &defs(),
        );
        assert_eq!(out, 60);
    }

    #[test]
    fn cc_missing_everything_gives_default() {
        assert_eq!(parse_cache_control(None, None, None, &defs()), 3600);
    }

    #[test]
    fn cc_malformed_gives_default() {
        assert_eq!(parse_cache_control(Some("bogus!!"), None, None, &defs()), 3600);
    }

    #[test]
    fn cc_expires_in_the_past_gives_default() {
        let out = parse_cache_control(
            None,
            Some("Sun, 06 Nov 2026 08:49:00 GMT"),
            Some("Sun, 06 Nov 2026 08:50:00 GMT"),
            &defs(),
        );
        assert_eq!(out, 3600);
    }
}
```

- [ ] **Step 2: Register module and export helper for proxy port**

Modify `server/src/lib.rs`:

```rust
pub mod blocklist;
pub mod browser;
pub mod config;
pub mod filter;
pub mod logger;
pub mod origin_cc;
pub mod proxy;
pub mod session;
```

The proxy port already lives in `server/src/browser.rs` as `static PROXY_PORT: OnceLock<u16>` (used by Firefox launch). Reuse it: expose a read-side accessor next to `set_proxy_port` in `browser.rs`:

```rust
// In server/src/browser.rs, next to set_proxy_port:
pub fn current_proxy_port() -> Option<u16> {
    PROXY_PORT.get().copied()
}
```

Then in `server/src/origin_cc.rs`, replace the earlier `use crate::proxy::current_proxy_port;` line with:

```rust
use crate::browser::current_proxy_port;
```

- [ ] **Step 3: Run tests — expect compile failure on unimplemented!()**

Run: `cargo test -p packet-browser-server origin_cc::tests`
Expected: FAIL — `unimplemented!()` panics.

- [ ] **Step 4: Implement parse_cache_control**

Replace the `unimplemented!()` in `parse_cache_control` with:

```rust
pub fn parse_cache_control(
    cc: Option<&str>,
    expires: Option<&str>,
    date: Option<&str>,
    defaults: &CcDefaults,
) -> i32 {
    if let Some(cc) = cc {
        let lower = cc.to_ascii_lowercase();
        let tokens: Vec<&str> = lower.split(',').map(|t| t.trim()).collect();
        if tokens.iter().any(|t| *t == "no-store" || *t == "private") {
            return -1;
        }
        if tokens.iter().any(|t| *t == "no-cache") {
            return 0;
        }
        // Find s-maxage first (shared-cache overrides max-age for us).
        let s_maxage = tokens
            .iter()
            .find_map(|t| t.strip_prefix("s-maxage="))
            .and_then(|v| v.parse::<i64>().ok());
        let max_age = tokens
            .iter()
            .find_map(|t| t.strip_prefix("max-age="))
            .and_then(|v| v.parse::<i64>().ok());
        let picked = s_maxage.or(max_age);
        if let Some(secs) = picked {
            let clamped = secs.clamp(0, defaults.max_max_age as i64);
            return clamped as i32;
        }
        // Cache-Control present but no useful directive → default.
        return defaults.default_max_age;
    }
    if let (Some(exp), Some(dt)) = (expires, date) {
        if let (Ok(exp_ts), Ok(dt_ts)) = (
            httpdate::parse_http_date(exp),
            httpdate::parse_http_date(dt),
        ) {
            if let Ok(delta) = exp_ts.duration_since(dt_ts) {
                let secs = delta.as_secs().min(defaults.max_max_age as u64) as i32;
                return secs;
            }
        }
    }
    defaults.default_max_age
}
```

Add `httpdate = "1"` to `server/Cargo.toml` dependencies.

- [ ] **Step 5: Run parse tests — expect pass**

Run: `cargo test -p packet-browser-server origin_cc::tests`
Expected: all `parse_cache_control` tests PASS.

- [ ] **Step 6: Implement probe_origin_cc**

Replace `probe_origin_cc`:

```rust
pub fn probe_origin_cc(url: &str, config: &Config) -> OriginDirectives {
    let defaults = CcDefaults {
        default_max_age: config.default_max_age_seconds,
        max_max_age: config.max_max_age_seconds,
    };
    let default_out = OriginDirectives { max_age: defaults.default_max_age };

    let proxy_port = match current_proxy_port() {
        Some(p) => p,
        None => return default_out,
    };
    let proxy = match reqwest::Proxy::all(format!("http://127.0.0.1:{}", proxy_port)) {
        Ok(p) => p,
        Err(_) => return default_out,
    };
    let client = match reqwest::blocking::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_millis(config.origin_cc_head_timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(_) => return default_out,
    };

    let headers = match client.head(url).send() {
        Ok(r) if r.status().is_success() || r.status().is_redirection() => r.headers().clone(),
        Ok(r) if r.status().as_u16() == 405 => {
            // Retry as GET, discard body; several origins reject HEAD.
            match client.get(url).send() {
                Ok(r) => r.headers().clone(),
                Err(_) => return default_out,
            }
        }
        Ok(_) => return default_out,
        Err(_) => return default_out,
    };

    let cc = headers.get("cache-control").and_then(|v| v.to_str().ok());
    let expires = headers.get("expires").and_then(|v| v.to_str().ok());
    let date = headers.get("date").and_then(|v| v.to_str().ok());
    OriginDirectives {
        max_age: parse_cache_control(cc, expires, date, &defaults),
    }
}
```

- [ ] **Step 7: Run all origin_cc tests**

Run: `cargo test -p packet-browser-server origin_cc`
Expected: all PASS. `probe_origin_cc` is not unit-tested directly (no in-repo HTTP mock); it will be covered by manual verification and by Task 4's integration.

- [ ] **Step 8: Commit**

```bash
git add server/Cargo.toml server/src/browser.rs server/src/lib.rs server/src/origin_cc.rs Cargo.lock
git commit -m "server: add origin Cache-Control probe with clamped defaults"
```

---

### Task 4: Server request handler honors etag and cache-control

**Files:**
- Modify: `server/src/main.rs`
- Modify: `server/Cargo.toml` (version bump)

**Interfaces:**
- Consumes: `Request::Get { url, if_none_match }`, `Response { status, etag, max_age, payload }`, `sanitized_html_etag`, `probe_origin_cc`.
- Produces:
  - `handle_request` sends `Status::NotModified` when the incoming `if_none_match` matches the fresh etag; otherwise `Status::Ok` with a fresh etag and origin-derived `max_age`.
  - `send_status_response` emits `etag = "-"`, `max_age = -1` for `Err` / `Blocked`.

- [ ] **Step 1: Bump server package version**

Modify `server/Cargo.toml`:

```toml
[package]
name = "packet-browser-server"
version = "0.2.0"
edition = "2021"
```

- [ ] **Step 2: Update the POST rejection to use uncacheable Response fields**

In `server/src/main.rs`, the current POST handling calls `send_error_response(&mut stream, "POST requests are not supported")`. Leave that call in place; the change is inside `send_status_response`, updated in Step 5 below.

- [ ] **Step 3: Update handle_request signature usage — pass through if_none_match**

Modify the request dispatch loop inside `handle_connection` (the block starting `match &request {`). Change how the URL is extracted so the if-none-match token also propagates:

```rust
let (url, if_none_match) = match &request {
    Request::Get { url, if_none_match } => (url.clone(), if_none_match.clone()),
    Request::Post { url, .. } => {
        eprintln!("[CMD] {} POST {} rejected (POST unsupported)", callsign, url);
        if let Err(e) = send_error_response(&mut stream, "POST requests are not supported") {
            eprintln!("[ERROR] Failed to send POST rejection to {}: {}", callsign, e);
            break;
        }
        continue;
    }
};

if let Err(e) = handle_request(&mut session, &mut browser, &callsign, &config, &logger, &mut stream, &url, if_none_match.as_deref()) {
    eprintln!("[ERROR] Request error for {}: {}", callsign, e);
}
```

- [ ] **Step 4: Rewrite handle_request to compute etag, probe CC, and choose status**

Replace the existing `handle_request` in `server/src/main.rs`:

```rust
fn handle_request(
    session: &mut Session,
    browser: &mut Option<BrowserInstance>,
    callsign: &str,
    config: &Config,
    logger: &Logger,
    stream: &mut TcpStream,
    url: &str,
    if_none_match: Option<&str>,
) -> std::io::Result<()> {
    use packet_browser_shared::protocol::sanitized_html_etag;
    use packet_browser_server::origin_cc::probe_origin_cc;

    if let Err(e) = validate_url(url, &config.blocked_ranges) {
        eprintln!("[FILTER] Rejected URL {} for {}: {}", url, callsign, e);
        let (status, log_status) = match e {
            UrlError::BlockedProtocol(_) | UrlError::BlockedHost(_) => {
                (Status::Blocked, LogStatus::Blocked)
            }
            UrlError::UnresolvableHost(_) | UrlError::InvalidUrl => {
                (Status::Err, LogStatus::Error)
            }
        };
        let message = e.to_string();
        let log_entry = LogEntry::new(
            session.callsign.clone(),
            url.to_string(),
            log_status,
            Some(message.clone()),
        );
        let _ = logger.log(&log_entry);
        send_status_response(stream, status, &message)?;
        return Ok(());
    }

    eprintln!("[FETCH] Loading {} for {}", url, callsign);

    let html = loop {
        let b = match browser.as_ref() {
            Some(b) => b,
            None => {
                eprintln!("[BROWSER] No browser instance, creating for {}", callsign);
                *browser = BrowserInstance::new(callsign).ok();
                if browser.is_none() {
                    send_error_response(stream, "Browser unavailable")?;
                    return Ok(());
                }
                continue;
            }
        };
        match b.fetch_page(url) {
            Ok(html) => break html,
            Err(BrowserError::BrowserCrashed) => {
                eprintln!("[BROWSER] Firefox session lost, restarting for {}", callsign);
                *browser = BrowserInstance::new(callsign).ok();
                if browser.is_none() {
                    send_error_response(stream, "Browser unavailable")?;
                    return Ok(());
                }
                continue;
            }
            Err(e) => {
                eprintln!("[FETCH] Error loading {} for {}: {}", url, callsign, e);
                let log_entry = LogEntry::new(
                    session.callsign.clone(),
                    url.to_string(),
                    LogStatus::Error,
                    Some(e.to_string()),
                );
                let _ = logger.log(&log_entry);
                send_error_response(stream, "Failed to load page")?;
                return Ok(());
            }
        }
    };

    let etag = sanitized_html_etag(&html);
    let directives = probe_origin_cc(url, config);

    let log_entry = LogEntry::new(
        session.callsign.clone(),
        url.to_string(),
        LogStatus::Ok,
        None,
    );
    let _ = logger.log(&log_entry);

    session.current_url = Some(url.to_string());

    if if_none_match.map(|e| e == etag).unwrap_or(false) {
        eprintln!(
            "[CACHE] {} etag {} matched, sending NotModified for {}",
            callsign, etag, url
        );
        let response = Response {
            status: Status::NotModified,
            etag: etag.clone(),
            max_age: directives.max_age,
            payload: Vec::new(),
        };
        stream.write_all(&response.encode())?;
        stream.flush()?;
        return Ok(());
    }

    let compressed = match brotli_compress(html.as_bytes(), config.brotli_quality) {
        Ok(data) => data,
        Err(e) => {
            eprintln!("[COMPRESS] Error compressing for {}: {}", callsign, e);
            send_error_response(stream, "Compression error")?;
            return Ok(());
        }
    };

    eprintln!(
        "[SEND] {} bytes -> {} bytes compressed (etag={}, max_age={}) for {}",
        html.len(), compressed.len(), etag, directives.max_age, callsign,
    );

    let response = Response {
        status: Status::Ok,
        etag,
        max_age: directives.max_age,
        payload: compressed,
    };
    stream.write_all(&response.encode())?;
    stream.flush()?;

    Ok(())
}
```

- [ ] **Step 5: Fill in etag "-" and max_age -1 for status responses**

Replace `send_status_response` in `server/src/main.rs`:

```rust
fn send_status_response(
    stream: &mut TcpStream,
    status: Status,
    message: &str,
) -> std::io::Result<()> {
    let compressed = brotli_compress(message.as_bytes(), 11)
        .unwrap_or_else(|_| message.as_bytes().to_vec());
    let response = Response {
        status,
        etag: "-".to_string(),
        max_age: -1,
        payload: compressed,
    };
    stream.write_all(&response.encode())?;
    stream.flush()?;
    Ok(())
}
```

- [ ] **Step 6: Build and run existing server tests**

Run: `cargo build -p packet-browser-server && cargo test -p packet-browser-server`
Expected: PASS. (No new unit tests here; behavior is covered by shared protocol tests and by manual verification via the demo. See Task 10 for e2e.)

- [ ] **Step 7: Commit**

```bash
git add server/Cargo.toml server/src/main.rs Cargo.lock
git commit -m "server: derive etag, probe origin CC, honor IF-NONE-MATCH"
```

---

### Task 5: Client persistent cache module

**Files:**
- Create: `client/src/cache.rs`
- Modify: `client/src/main.rs` (register module)
- Modify: `client/Cargo.toml`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct Cache` with `pub fn open(dir: &Path, cap_bytes: u64, max_ttl: Duration) -> Result<Self, CacheError>`
  - `pub struct Hit { pub etag: String, pub brotli_body: Vec<u8>, pub fetched_at: SystemTime, pub max_age: Duration }`
  - `impl Hit { pub fn is_fresh(&self, now: SystemTime) -> bool }`
  - `pub struct CacheEntry { pub url: String, pub etag: String, pub fetched_at: SystemTime, pub last_used: SystemTime, pub size: u64, pub max_age: Duration }`
  - `impl Cache { pub fn lookup(&self, url: &str) -> Option<Hit>; pub fn insert(&self, url: &str, etag: &str, brotli_body: &[u8], server_max_age_secs: i32); pub fn touch_fresh(&self, url: &str); pub fn touch_last_used(&self, url: &str); pub fn delete(&self, url: &str); pub fn clear(&self); pub fn list(&self) -> Vec<CacheEntry> }`
  - `pub enum CacheError { Io(std::io::Error), Sql(rusqlite::Error) }` — used only by `open`.

- [ ] **Step 1: Add dependencies**

Modify `client/Cargo.toml`:

```toml
[dependencies]
# ...existing entries...
rusqlite = { version = "0.32", features = ["bundled"] }
```

`shared` already exposes `sanitized_html_etag`; no new sha2 dep needed on the client.

- [ ] **Step 2: Write failing tests**

Create `client/src/cache.rs`:

```rust
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum CacheError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sql: {0}")]
    Sql(#[from] rusqlite::Error),
}

pub struct Cache {
    conn: Mutex<Connection>,
    cap_bytes: u64,
    max_ttl: Duration,
}

pub struct Hit {
    pub etag: String,
    pub brotli_body: Vec<u8>,
    pub fetched_at: SystemTime,
    pub max_age: Duration,
}

impl Hit {
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        now.duration_since(self.fetched_at)
            .map(|age| age < self.max_age)
            .unwrap_or(false)
    }
}

pub struct CacheEntry {
    pub url: String,
    pub etag: String,
    pub fetched_at: SystemTime,
    pub last_used: SystemTime,
    pub size: u64,
    pub max_age: Duration,
}

impl Cache {
    pub fn open(dir: &Path, cap_bytes: u64, max_ttl: Duration) -> Result<Self, CacheError> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join("cache.sqlite");
        let conn = Connection::open(&path)?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS entries (
                url         TEXT PRIMARY KEY,
                etag        TEXT NOT NULL,
                brotli_body BLOB NOT NULL,
                fetched_at  INTEGER NOT NULL,
                last_used   INTEGER NOT NULL,
                size        INTEGER NOT NULL,
                max_age     INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_last_used ON entries(last_used);
            "#,
        )?;
        Ok(Self { conn: Mutex::new(conn), cap_bytes, max_ttl })
    }

    pub fn lookup(&self, url: &str) -> Option<Hit> {
        let conn = self.conn.lock().ok()?;
        conn.query_row(
            "SELECT etag, brotli_body, fetched_at, max_age FROM entries WHERE url = ?1",
            params![url],
            |row| {
                let etag: String = row.get(0)?;
                let body: Vec<u8> = row.get(1)?;
                let fetched_at_secs: i64 = row.get(2)?;
                let max_age_secs: i64 = row.get(3)?;
                Ok((etag, body, fetched_at_secs, max_age_secs))
            },
        )
        .ok()
        .map(|(etag, body, f, m)| Hit {
            etag,
            brotli_body: body,
            fetched_at: UNIX_EPOCH + Duration::from_secs(f.max(0) as u64),
            max_age: Duration::from_secs(m.max(0) as u64),
        })
    }

    pub fn insert(
        &self,
        url: &str,
        etag: &str,
        brotli_body: &[u8],
        server_max_age_secs: i32,
    ) {
        if server_max_age_secs < 0 {
            return;
        }
        let capped = (server_max_age_secs as u64).min(self.max_ttl.as_secs());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let size = brotli_body.len() as i64;
        let cap = self.cap_bytes as i64;

        let mut conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let tx = match conn.transaction() {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cache insert: begin failed: {}", e);
                return;
            }
        };
        let up = tx.execute(
            r#"INSERT INTO entries (url, etag, brotli_body, fetched_at, last_used, size, max_age)
               VALUES (?1, ?2, ?3, ?4, ?4, ?5, ?6)
               ON CONFLICT(url) DO UPDATE SET
                   etag = excluded.etag,
                   brotli_body = excluded.brotli_body,
                   fetched_at = excluded.fetched_at,
                   last_used = excluded.last_used,
                   size = excluded.size,
                   max_age = excluded.max_age"#,
            params![url, etag, brotli_body, now, size, capped as i64],
        );
        if let Err(e) = up {
            tracing::warn!("cache insert: upsert failed: {}", e);
            return;
        }
        // Evict LRU until under cap.
        let total: i64 = tx
            .query_row("SELECT COALESCE(SUM(size), 0) FROM entries", [], |r| r.get(0))
            .unwrap_or(0);
        if total > cap {
            let mut over = total - cap;
            let victims: Vec<(String, i64)> = tx
                .prepare("SELECT url, size FROM entries ORDER BY last_used ASC")
                .and_then(|mut s| {
                    s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                        .collect()
                })
                .unwrap_or_default();
            for (v_url, v_size) in victims {
                if over <= 0 {
                    break;
                }
                if v_url == url {
                    continue; // never evict what we just inserted
                }
                if let Err(e) = tx.execute("DELETE FROM entries WHERE url = ?1", params![v_url]) {
                    tracing::warn!("cache evict: {}", e);
                    continue;
                }
                over -= v_size;
            }
        }
        if let Err(e) = tx.commit() {
            tracing::warn!("cache insert: commit failed: {}", e);
        }
    }

    pub fn touch_fresh(&self, url: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute(
            "UPDATE entries SET fetched_at = ?1, last_used = ?1 WHERE url = ?2",
            params![now, url],
        );
    }

    pub fn touch_last_used(&self, url: &str) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0) as i64;
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute(
            "UPDATE entries SET last_used = ?1 WHERE url = ?2",
            params![now, url],
        );
    }

    pub fn delete(&self, url: &str) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute("DELETE FROM entries WHERE url = ?1", params![url]);
    }

    pub fn clear(&self) {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let _ = conn.execute("DELETE FROM entries", []);
    }

    pub fn cap_bytes(&self) -> u64 {
        self.cap_bytes
    }

    pub fn list(&self) -> Vec<CacheEntry> {
        let conn = match self.conn.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let mut stmt = match conn.prepare(
            "SELECT url, etag, fetched_at, last_used, size, max_age FROM entries ORDER BY last_used DESC LIMIT 200",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([], |row| {
            Ok(CacheEntry {
                url: row.get(0)?,
                etag: row.get(1)?,
                fetched_at: UNIX_EPOCH + Duration::from_secs(row.get::<_, i64>(2)?.max(0) as u64),
                last_used: UNIX_EPOCH + Duration::from_secs(row.get::<_, i64>(3)?.max(0) as u64),
                size: row.get::<_, i64>(4)?.max(0) as u64,
                max_age: Duration::from_secs(row.get::<_, i64>(5)?.max(0) as u64),
            })
        })
        .and_then(|iter| iter.collect::<Result<Vec<_>, _>>())
        .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn open_cache(cap: u64, ttl_secs: u64) -> (tempfile::TempDir, Cache) {
        let d = tempdir().unwrap();
        let c = Cache::open(d.path(), cap, Duration::from_secs(ttl_secs)).unwrap();
        (d, c)
    }

    #[test]
    fn insert_then_lookup_roundtrip() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "etag1", &[1, 2, 3], 300);
        let hit = c.lookup("https://a").unwrap();
        assert_eq!(hit.etag, "etag1");
        assert_eq!(hit.brotli_body, vec![1, 2, 3]);
        assert_eq!(hit.max_age, Duration::from_secs(300));
    }

    #[test]
    fn negative_max_age_skips_write() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "etag1", &[1, 2, 3], -1);
        assert!(c.lookup("https://a").is_none());
    }

    #[test]
    fn zero_max_age_is_stored_but_never_fresh() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "etag1", &[1, 2, 3], 0);
        let hit = c.lookup("https://a").unwrap();
        assert!(!hit.is_fresh(SystemTime::now()));
    }

    #[test]
    fn max_ttl_caps_server_max_age() {
        let (_d, c) = open_cache(1_000_000, 60);
        c.insert("https://a", "etag1", &[1, 2, 3], 999_999);
        let hit = c.lookup("https://a").unwrap();
        assert_eq!(hit.max_age, Duration::from_secs(60));
    }

    #[test]
    fn is_fresh_boundaries() {
        let hit = Hit {
            etag: "e".to_string(),
            brotli_body: vec![],
            fetched_at: SystemTime::now(),
            max_age: Duration::from_secs(1),
        };
        assert!(hit.is_fresh(SystemTime::now()));
        assert!(!hit.is_fresh(SystemTime::now() + Duration::from_secs(2)));
    }

    #[test]
    fn touch_last_used_updates_ordering() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://old", "e", &[0u8; 10], 300);
        std::thread::sleep(Duration::from_millis(1100)); // seconds-resolution timestamp
        c.insert("https://new", "e", &[0u8; 10], 300);
        std::thread::sleep(Duration::from_millis(1100));
        c.touch_last_used("https://old");
        let entries = c.list();
        assert_eq!(entries[0].url, "https://old");
    }

    #[test]
    fn lru_evicts_least_recently_used_on_overflow() {
        // Cap = 30 bytes; insert three 20-byte entries: overflow after 2nd, evict oldest.
        let (_d, c) = open_cache(30, 600);
        c.insert("https://a", "e", &[0u8; 20], 300);
        std::thread::sleep(Duration::from_millis(1100));
        c.insert("https://b", "e", &[0u8; 20], 300);
        assert!(c.lookup("https://a").is_none(), "oldest should have been evicted");
        assert!(c.lookup("https://b").is_some());
    }

    #[test]
    fn delete_and_clear() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "e", &[1], 300);
        c.insert("https://b", "e", &[1], 300);
        c.delete("https://a");
        assert!(c.lookup("https://a").is_none());
        assert!(c.lookup("https://b").is_some());
        c.clear();
        assert!(c.lookup("https://b").is_none());
    }

    #[test]
    fn touch_fresh_bumps_fetched_at() {
        let (_d, c) = open_cache(1_000_000, 600);
        c.insert("https://a", "e", &[1], 300);
        let before = c.lookup("https://a").unwrap().fetched_at;
        std::thread::sleep(Duration::from_millis(1100));
        c.touch_fresh("https://a");
        let after = c.lookup("https://a").unwrap().fetched_at;
        assert!(after > before);
    }
}
```

- [ ] **Step 3: Register the module**

Modify `client/src/main.rs`. Near the other `mod` lines at the top, add:

```rust
mod cache;
```

- [ ] **Step 4: Run tests — expect PASS**

Run: `cargo test -p packet-browser-client cache::tests -- --test-threads=1`
Expected: all PASS.

- [ ] **Step 5: Commit**

```bash
git add client/Cargo.toml client/src/cache.rs client/src/main.rs Cargo.lock
git commit -m "client: add SQLite persistent response cache with LRU eviction"
```

---

### Task 6: Client config gains a [cache] section

**Files:**
- Modify: `client/src/config.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct CacheSection { pub enabled: bool, pub max_bytes: u64, pub max_ttl_seconds: u64, pub dir: Option<PathBuf> }`
  - `FileConfig.cache: CacheSection`
  - `impl CacheSection { pub fn effective_dir(&self) -> Result<PathBuf, ConfigError> }` — returns the configured dir or the XDG-cache default `${XDG_CACHE_HOME:-~/.cache}/packet-browser/`.

- [ ] **Step 1: Write failing tests**

Add to the existing `mod tests` in `client/src/config.rs`:

```rust
#[test]
fn cache_defaults_are_applied_when_section_absent() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.ini");
    let cfg = FileConfig::default();
    cfg.save(&path).unwrap();
    let loaded = FileConfig::load(&path).unwrap();
    assert!(loaded.cache.enabled);
    assert_eq!(loaded.cache.max_bytes, 209_715_200);
    assert_eq!(loaded.cache.max_ttl_seconds, 86_400);
    assert!(loaded.cache.dir.is_none());
}

#[test]
fn cache_section_roundtrip() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("config.ini");
    let cfg = FileConfig {
        cache: CacheSection {
            enabled: false,
            max_bytes: 42,
            max_ttl_seconds: 7,
            dir: Some(std::path::PathBuf::from("/tmp/pb-cache")),
        },
        ..FileConfig::default()
    };
    cfg.save(&path).unwrap();
    let loaded = FileConfig::load(&path).unwrap();
    assert!(!loaded.cache.enabled);
    assert_eq!(loaded.cache.max_bytes, 42);
    assert_eq!(loaded.cache.max_ttl_seconds, 7);
    assert_eq!(
        loaded.cache.dir.as_deref().map(|p| p.to_string_lossy().into_owned()),
        Some("/tmp/pb-cache".to_string())
    );
}
```

- [ ] **Step 2: Run tests — expect compile failure**

Run: `cargo test -p packet-browser-client config::tests::cache_defaults_are_applied_when_section_absent`
Expected: FAIL — `FileConfig` has no `cache` field.

- [ ] **Step 3: Add CacheSection and thread it through FileConfig**

Modify `client/src/config.rs`:

```rust
#[derive(Debug, Clone)]
pub struct CacheSection {
    pub enabled: bool,
    pub max_bytes: u64,
    pub max_ttl_seconds: u64,
    pub dir: Option<PathBuf>,
}

impl Default for CacheSection {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 209_715_200, // 200 MiB
            max_ttl_seconds: 86_400,
            dir: None,
        }
    }
}

impl CacheSection {
    pub fn effective_dir(&self) -> Result<PathBuf, ConfigError> {
        if let Some(d) = &self.dir {
            return Ok(d.clone());
        }
        let cache_root = dirs::cache_dir().ok_or(ConfigError::NoConfigDir)?;
        Ok(cache_root.join("packet-browser"))
    }
}
```

Add `cache: CacheSection` to `FileConfig` and its `Default` impl:

```rust
#[derive(Debug, Clone)]
pub struct FileConfig {
    pub agwpe_host: String,
    pub agwpe_port: u16,
    pub my_callsign: String,
    pub target_callsign: String,
    pub bpq_command: String,
    pub skip_bpq_app: bool,
    pub cache: CacheSection,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            agwpe_host: "127.0.0.1".to_string(),
            agwpe_port: 8000,
            my_callsign: String::new(),
            target_callsign: String::new(),
            bpq_command: "WEB".to_string(),
            skip_bpq_app: false,
            cache: CacheSection::default(),
        }
    }
}
```

Update `FileConfig::load` to parse the `[cache]` section:

```rust
let cache_enabled = ini
    .get("cache", "enabled")
    .map(|v| matches!(v.to_lowercase().as_str(), "true" | "1" | "yes" | "on"))
    .unwrap_or(true);
let cache_max_bytes = ini
    .get("cache", "max_bytes")
    .and_then(|v| v.parse().ok())
    .unwrap_or(209_715_200);
let cache_max_ttl_seconds = ini
    .get("cache", "max_ttl_seconds")
    .and_then(|v| v.parse().ok())
    .unwrap_or(86_400);
let cache_dir = ini
    .get("cache", "dir")
    .filter(|s| !s.trim().is_empty())
    .map(PathBuf::from);

Ok(Self {
    agwpe_host,
    agwpe_port,
    my_callsign,
    target_callsign,
    bpq_command,
    skip_bpq_app,
    cache: CacheSection {
        enabled: cache_enabled,
        max_bytes: cache_max_bytes,
        max_ttl_seconds: cache_max_ttl_seconds,
        dir: cache_dir,
    },
})
```

Update `FileConfig::save` to emit the `[cache]` section:

```rust
ini.set("cache", "enabled", Some(self.cache.enabled.to_string()));
ini.set("cache", "max_bytes", Some(self.cache.max_bytes.to_string()));
ini.set("cache", "max_ttl_seconds", Some(self.cache.max_ttl_seconds.to_string()));
if let Some(d) = &self.cache.dir {
    ini.set("cache", "dir", Some(d.to_string_lossy().into_owned()));
}
```

If existing tests instantiate `FileConfig` with struct-literal syntax (search for `FileConfig {` in the file), add `cache: CacheSection::default(),` to each.

- [ ] **Step 4: Run all client tests — expect PASS**

Run: `cargo test -p packet-browser-client config::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add client/src/config.rs
git commit -m "client: add [cache] config section with defaults"
```

---

### Task 7: Wire the Cache into AppContext

**Files:**
- Modify: `client/src/main.rs`
- Modify: `client/src/proxy.rs` (the `AppContext` struct only — no handler changes here)

**Interfaces:**
- Consumes: `Cache::open`, `FileConfig::cache`.
- Produces:
  - `pub struct AppContext` extended with `pub cache: Option<Arc<Cache>>` and `pub cache_max_ttl: Duration`.

- [ ] **Step 1: Extend AppContext struct**

Modify `client/src/proxy.rs`:

```rust
use std::time::Duration;
use crate::cache::Cache;

pub struct AppContext {
    pub state: SharedState,
    pub agwpe: AgwpeManager,
    pub log_tx: broadcast::Sender<DebugLogEntry>,
    pub host_allowlist: HostAllowlist,
    pub cache: Option<Arc<Cache>>,
    pub cache_max_ttl: Duration,
}
```

- [ ] **Step 2: Open the Cache in main and pass it into AppContext**

Modify `client/src/main.rs`. Locate the `AppContext { ... }` construction. Just above it, add:

```rust
let cache_max_ttl = Duration::from_secs(file_config.cache.max_ttl_seconds);
let cache = if file_config.cache.enabled {
    match file_config
        .cache
        .effective_dir()
        .map_err(|e| e.to_string())
        .and_then(|d| {
            crate::cache::Cache::open(&d, file_config.cache.max_bytes, cache_max_ttl)
                .map_err(|e| e.to_string())
        }) {
        Ok(c) => Some(std::sync::Arc::new(c)),
        Err(e) => {
            tracing::warn!("cache disabled for this session: {}", e);
            None
        }
    }
} else {
    None
};
```

Then include the fields in the `AppContext { ... }` initializer:

```rust
let ctx = std::sync::Arc::new(AppContext {
    state,
    agwpe,
    log_tx,
    host_allowlist,
    cache,
    cache_max_ttl,
});
```

Import `std::time::Duration` at the top of the file if not present.

- [ ] **Step 3: Build**

Run: `cargo build -p packet-browser-client`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add client/src/main.rs client/src/proxy.rs
git commit -m "client: plumb Cache into AppContext with best-effort open"
```

---

### Task 8: Client browse handler uses the cache

**Files:**
- Modify: `client/src/proxy.rs`
- Modify: `client/Cargo.toml` (version bump)

**Interfaces:**
- Consumes: `AppContext.cache`, `Cache::{lookup, insert, touch_fresh, touch_last_used}`, `Request::Get { url, if_none_match }`, `Response { status, etag, max_age, payload }`.
- Produces:
  - `browse_get_handler` accepts an optional `nocache=1` query parameter.
  - `handle_browse(ctx, url, post_body, nocache, browser_if_none_match)` implements the new flow.
  - Responses on the OK path carry `ETag` + `Cache-Control` headers.
  - On matching browser `If-None-Match` with a fresh entry, the handler returns `304 Not Modified` with an empty body.

- [ ] **Step 1: Bump client package version**

Modify `client/Cargo.toml`:

```toml
[package]
name = "packet-browser-client"
version = "0.2.0"
edition = "2021"
```

- [ ] **Step 2: Update browse_get_handler to read nocache and browser If-None-Match**

Replace `browse_get_handler` in `client/src/proxy.rs` with a version that pulls the browser's `If-None-Match` off the raw `HeaderMap` (avoids the typed-header extractor to keep axum feature use minimal):

```rust
#[derive(Deserialize)]
struct BrowseParams {
    url: Option<String>,
    #[serde(default)]
    nocache: Option<String>,
}

async fn browse_get_handler(
    Query(params): Query<BrowseParams>,
    Extension(ctx): Extension<Arc<AppContext>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let url = match params.url {
        Some(u) if !u.is_empty() => u,
        _ => {
            {
                let state = ctx.state.lock_or_poisoned();
                if state.connection_state != ConnectionState::Connected {
                    return Redirect::to("/connect").into_response();
                }
            }
            return Html(ui::browse_page("", "")).into_response();
        }
    };

    let nocache = params.nocache.as_deref() == Some("1");
    let browser_inm = headers
        .get(axum::http::header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_matches('"').to_string());

    handle_browse(&ctx, &url, None, nocache, browser_inm).await
}
```

Also update `browse_post_handler` to pass through the new params:

```rust
async fn browse_post_handler(
    Query(params): Query<BrowsePostParams>,
    Extension(ctx): Extension<Arc<AppContext>>,
    body: String,
) -> Response {
    let url = match params.url {
        Some(u) if !u.is_empty() => u,
        _ => return Redirect::to("/connect").into_response(),
    };
    handle_browse(&ctx, &url, Some(body.into_bytes()), true, None).await
}
```

(POST always bypasses the cache; `nocache = true` here means "don't consult", the handler doesn't write for POST anyway.)

- [ ] **Step 3: Add dispatch_ax25, helpers, and rewrite handle_browse**

Append the following helpers to `client/src/proxy.rs`:

```rust
/// Send a request over AX.25 and render the response, optionally writing the
/// result to the cache on `Status::Ok`.
async fn dispatch_ax25(
    ctx: &AppContext,
    url: &str,
    request: packet_browser_shared::protocol::Request,
    cache_for_write: Option<Arc<crate::cache::Cache>>,
) -> Response {
    use packet_browser_shared::compress::brotli_decompress;
    use packet_browser_shared::protocol::{Response as ProtocolResponse, Status};

    let encoded = request.encode();
    let cached_etag = match &request {
        packet_browser_shared::protocol::Request::Get { if_none_match, .. } => if_none_match.clone(),
        _ => None,
    };

    match ctx.agwpe.send_request(encoded).await {
        Ok(response_data) => {
            let (status, b64_len, etag, max_age, header_end) =
                match ProtocolResponse::decode_header(&response_data) {
                    Ok(Some(t)) => t,
                    Ok(None) => return Html(ui::error_page("Incomplete response header")).into_response(),
                    Err(e) => return Html(ui::error_page(&format!("Invalid response header: {}", e))).into_response(),
                };

            let b64_end = header_end + b64_len as usize;
            if response_data.len() < b64_end {
                return Html(ui::error_page("Incomplete response payload")).into_response();
            }

            match status {
                Status::NotModified => {
                    // Server confirms our cached etag is still valid. This only
                    // makes sense when we actually had a cache entry.
                    if let (Some(cache), Some(etag_sent)) = (cache_for_write.as_ref(), cached_etag) {
                        if etag_sent == etag {
                            cache.touch_fresh(url);
                            if let Some(hit) = cache.lookup(url) {
                                return serve_from_hit(&hit, url);
                            }
                        }
                    }
                    // No cache entry to serve — treat as an error.
                    Html(ui::error_page("Server sent NotModified but no cache entry is available")).into_response()
                }
                Status::Ok => {
                    let compressed = match ProtocolResponse::decode_payload(&response_data[header_end..b64_end]) {
                        Ok(b) => b,
                        Err(e) => return Html(ui::error_page(&format!("Base64 decode failed: {}", e))).into_response(),
                    };
                    if let Some(cache) = cache_for_write.as_ref() {
                        cache.insert(url, &etag, &compressed, max_age);
                    }
                    let decompressed = match brotli_decompress(&compressed) {
                        Ok(d) => d,
                        Err(e) => return Html(ui::error_page(&format!("Decompression failed: {}", e))).into_response(),
                    };
                    let html = match String::from_utf8(decompressed) {
                        Ok(h) => h,
                        Err(_) => return Html(ui::error_page("Invalid UTF-8 in response")).into_response(),
                    };
                    match crate::rewrite::rewrite_html(&html, url) {
                        Ok(rewritten) => {
                            let body = ui::browse_page(&rewritten, url);
                            build_cached_html_response(body, &etag, effective_ttl_secs(max_age, ctx.cache_max_ttl.as_secs()))
                        }
                        Err(e) => Html(ui::error_page(&format!("Failed to rewrite HTML: {}", e))).into_response(),
                    }
                }
                Status::Err | Status::Blocked => {
                    let compressed = match ProtocolResponse::decode_payload(&response_data[header_end..b64_end]) {
                        Ok(b) => b,
                        Err(e) => return Html(ui::error_page(&format!("Base64 decode failed: {}", e))).into_response(),
                    };
                    let decompressed = match brotli_decompress(&compressed) {
                        Ok(d) => d,
                        Err(e) => return Html(ui::error_page(&format!("Decompression failed: {}", e))).into_response(),
                    };
                    let text = String::from_utf8(decompressed).unwrap_or_else(|_| "Invalid UTF-8".to_string());
                    Html(ui::error_page(&text)).into_response()
                }
            }
        }
        Err(e) => Html(ui::error_page(&format!("Request failed: {}", e))).into_response(),
    }
}

pub(crate) fn effective_ttl_secs(server_max_age: i32, config_cap_secs: u64) -> u64 {
    if server_max_age <= 0 {
        return 0;
    }
    (server_max_age as u64).min(config_cap_secs)
}

fn serve_from_hit(hit: &crate::cache::Hit, url: &str) -> Response {
    use packet_browser_shared::compress::brotli_decompress;
    let decompressed = match brotli_decompress(&hit.brotli_body) {
        Ok(d) => d,
        Err(e) => return Html(ui::error_page(&format!("Decompression failed: {}", e))).into_response(),
    };
    let html = match String::from_utf8(decompressed) {
        Ok(h) => h,
        Err(_) => return Html(ui::error_page("Invalid UTF-8 in cached response")).into_response(),
    };
    let rewritten = match crate::rewrite::rewrite_html(&html, url) {
        Ok(r) => r,
        Err(e) => return Html(ui::error_page(&format!("Failed to rewrite HTML: {}", e))).into_response(),
    };
    let body = ui::browse_page(&rewritten, url);
    let remaining = std::time::SystemTime::now()
        .duration_since(hit.fetched_at)
        .map(|age| hit.max_age.checked_sub(age).unwrap_or_default().as_secs())
        .unwrap_or(0);
    build_cached_html_response(body, &hit.etag, remaining)
}

pub(crate) fn build_cached_html_response(body: String, etag: &str, ttl_secs: u64) -> Response {
    let mut resp = Html(body).into_response();
    let headers = resp.headers_mut();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_str(&format!("private, max-age={}", ttl_secs))
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("private, max-age=0")),
    );
    headers.insert(
        axum::http::header::ETAG,
        axum::http::HeaderValue::from_str(&format!("\"{}\"", etag))
            .unwrap_or_else(|_| axum::http::HeaderValue::from_static("\"-\"")),
    );
    resp
}
```

Now replace the existing `handle_browse` in `client/src/proxy.rs` with:

```rust
async fn handle_browse(
    ctx: &AppContext,
    url: &str,
    post_body: Option<Vec<u8>>,
    nocache: bool,
    browser_if_none_match: Option<String>,
) -> Response {
    use std::time::SystemTime;
    use packet_browser_shared::protocol::Request;

    {
        let state = ctx.state.lock_or_poisoned();
        if state.connection_state != ConnectionState::Connected {
            return Redirect::to("/connect").into_response();
        }
    }

    if let Some(body) = post_body {
        let request = Request::Post { url: url.to_string(), body };
        return dispatch_ax25(ctx, url, request, None).await;
    }

    let cache = ctx.cache.clone();

    if !nocache {
        if let Some(cache) = cache.as_ref() {
            if let Some(hit) = cache.lookup(url) {
                if hit.is_fresh(SystemTime::now()) {
                    if browser_if_none_match.as_deref() == Some(&hit.etag) {
                        return axum::http::Response::builder()
                            .status(StatusCode::NOT_MODIFIED)
                            .header("etag", format!("\"{}\"", hit.etag))
                            .body(axum::body::Body::empty())
                            .unwrap();
                    }
                    cache.touch_last_used(url);
                    return serve_from_hit(&hit, url);
                }
            }
        }
    }

    let cached_etag = if !nocache {
        cache.as_ref().and_then(|c| c.lookup(url).map(|h| h.etag))
    } else {
        None
    };

    let request = Request::Get {
        url: url.to_string(),
        if_none_match: cached_etag,
    };
    dispatch_ax25(ctx, url, request, cache).await
}
```

- [ ] **Step 4: Build**

Run: `cargo build -p packet-browser-client`
Expected: PASS.

- [ ] **Step 5: Run all client tests**

Run: `cargo test -p packet-browser-client -- --test-threads=1`
Expected: PASS. (No new unit tests here — flow is covered by module-level tests in Task 5 plus the shared protocol tests.)

- [ ] **Step 6: Commit**

```bash
git add client/Cargo.toml client/src/proxy.rs
git commit -m "client: browse handler serves from cache, revalidates via IF-NONE-MATCH"
```

---

### Task 9: Cache admin page and Reload button

**Files:**
- Modify: `client/src/proxy.rs`
- Modify: `client/src/ui.rs`

**Interfaces:**
- Consumes: `Cache::{list, delete, clear}`.
- Produces:
  - `GET /cache` returns an HTML table of cache entries.
  - `POST /api/cache/clear` empties the store.
  - `POST /api/cache/delete` (JSON: `{ "url": "…" }`) removes one entry.
  - The browse page shows a "Reload" link that appends `&nocache=1`.

- [ ] **Step 1: Add the Reload link to the browse header**

In `client/src/ui.rs`, inside `browse_page`, add a Reload link next to the existing Connect / Config links. Replace the header block:

```rust
    <div class="browse-header">
        <a href="/connect">Connect</a>
        <a href="/configuration">Config</a>
        <a href="/cache">Cache</a>
        <a href="/browse?url={url}&amp;nocache=1" title="Bypass cache and refetch">Reload</a>
        <form action="/browse" method="GET" style="display:flex;gap:0.5em;flex:1;margin:0">
            <input type="text" name="url" value="{url}" placeholder="Enter a URL, e.g. https://example.com" autocomplete="off" autofocus>
            <button type="submit">Go</button>
        </form>
    </div>
```

- [ ] **Step 2: Add cache_page UI function**

Append to `client/src/ui.rs`:

```rust
pub struct CachePageRow {
    pub url: String,
    pub size_bytes: u64,
    pub fetched_at_iso: String,
    pub last_used_iso: String,
    pub ttl_remaining_secs: i64,
    pub etag: String,
}

pub fn cache_page(rows: &[CachePageRow], total_bytes: u64, cap_bytes: u64) -> String {
    let mut body = String::new();
    body.push_str(&format!(
        "<p>{} entries — {} / {} bytes used.</p>",
        rows.len(),
        total_bytes,
        cap_bytes,
    ));
    body.push_str(r#"<form method="POST" action="/api/cache/clear" style="margin-bottom:1em"><button class="danger" type="submit">Clear all</button></form>"#);
    body.push_str(r#"<table style="width:100%;border-collapse:collapse">"#);
    body.push_str(r#"<thead><tr><th style="text-align:left">URL</th><th>Size</th><th>Fetched</th><th>Last used</th><th>TTL left</th><th></th></tr></thead><tbody>"#);
    for row in rows {
        body.push_str(&format!(
            r#"<tr>
                <td style="max-width:40em;overflow:hidden;text-overflow:ellipsis">{url}</td>
                <td>{size}</td>
                <td>{fetched}</td>
                <td>{used}</td>
                <td>{ttl}s</td>
                <td>
                    <form method="POST" action="/api/cache/delete" style="margin:0">
                        <input type="hidden" name="url" value="{url_attr}">
                        <button class="danger" type="submit">Delete</button>
                    </form>
                </td>
            </tr>"#,
            url = h(&row.url),
            size = row.size_bytes,
            fetched = h(&row.fetched_at_iso),
            used = h(&row.last_used_iso),
            ttl = row.ttl_remaining_secs,
            url_attr = h(&row.url),
        ));
    }
    body.push_str("</tbody></table>");

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><title>Cache</title><style>{css}</style></head>
<body><h1>Cache</h1><p><a href="/browse">Back to browse</a></p>{body}</body></html>"#,
        css = CSS,
        body = body,
    )
}
```

- [ ] **Step 3: Register /cache routes**

In `client/src/proxy.rs`, extend `create_router`:

```rust
    Router::new()
        // ... existing routes ...
        .route("/cache", get(cache_page_handler))
        .route("/api/cache/clear", post(api_cache_clear))
        .route("/api/cache/delete", post(api_cache_delete))
        .layer(middleware::from_fn_with_state(ctx.clone(), security_guard))
        .layer(Extension(ctx))
```

- [ ] **Step 4: Add the handlers**

Append to `client/src/proxy.rs`:

```rust
async fn cache_page_handler(Extension(ctx): Extension<Arc<AppContext>>) -> impl IntoResponse {
    let now = std::time::SystemTime::now();
    let mut rows = Vec::new();
    let (total, cap) = match ctx.cache.as_ref() {
        Some(cache) => {
            let entries = cache.list();
            let total: u64 = entries.iter().map(|e| e.size).sum();
            for e in entries {
                let remaining = now
                    .duration_since(e.fetched_at)
                    .map(|age| e.max_age.checked_sub(age).unwrap_or_default().as_secs() as i64)
                    .unwrap_or(0);
                rows.push(ui::CachePageRow {
                    url: e.url,
                    size_bytes: e.size,
                    fetched_at_iso: iso_from_system_time(e.fetched_at),
                    last_used_iso: iso_from_system_time(e.last_used),
                    ttl_remaining_secs: remaining,
                    etag: e.etag,
                });
            }
            (total, cache.cap_bytes())
        }
        None => (0, 0),
    };
    Html(ui::cache_page(&rows, total, cap))
}

fn iso_from_system_time(t: std::time::SystemTime) -> String {
    use chrono::{DateTime, Utc};
    let dt: DateTime<Utc> = t.into();
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[derive(Deserialize)]
struct CacheDeleteRequest {
    url: String,
}

async fn api_cache_delete(
    Extension(ctx): Extension<Arc<AppContext>>,
    axum::extract::Form(req): axum::extract::Form<CacheDeleteRequest>,
) -> Redirect {
    if let Some(cache) = ctx.cache.as_ref() {
        cache.delete(&req.url);
    }
    Redirect::to("/cache")
}

async fn api_cache_clear(Extension(ctx): Extension<Arc<AppContext>>) -> Redirect {
    if let Some(cache) = ctx.cache.as_ref() {
        cache.clear();
    }
    Redirect::to("/cache")
}
```

- [ ] **Step 5: Build**

Run: `cargo build -p packet-browser-client`
Expected: PASS.

- [ ] **Step 6: Run client tests**

Run: `cargo test -p packet-browser-client -- --test-threads=1`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add client/src/proxy.rs client/src/ui.rs
git commit -m "client: add /cache admin page and Reload bypass"
```

---

### Task 10: Header contract tests (Rust + e2e)

**Files:**
- Modify: `client/src/proxy.rs` (add unit tests for pure header helpers)
- Modify: `e2e/test_e2e.py`

**Interfaces:**
- Consumes: `build_cached_html_response`, `effective_ttl_secs` (now `pub(crate)` after Task 8), `pb_client` fixture and `test_http_server` fixture (existing).
- Produces:
  - Rust unit tests asserting the pure helpers emit the expected `ETag` and `Cache-Control` headers with the correct values.
  - A pytest case that drives a real fetch through the demo stack and asserts the client's response carries an `ETag` and a `Cache-Control: private, max-age=…` header on both first and second fetch.

- [ ] **Step 1: Add Rust unit tests for the pure helpers**

Append to `client/src/proxy.rs` inside the existing `#[cfg(test)] mod tests` block:

```rust
#[test]
fn build_cached_html_response_sets_etag_and_cache_control() {
    let resp = super::build_cached_html_response(
        "<p>hi</p>".to_string(),
        "aBcDeFgHiJkLmNoP",
        1800,
    );
    let etag = resp.headers().get("etag").unwrap().to_str().unwrap().to_string();
    assert_eq!(etag, "\"aBcDeFgHiJkLmNoP\"");
    let cc = resp.headers().get("cache-control").unwrap().to_str().unwrap().to_string();
    assert_eq!(cc, "private, max-age=1800");
}

#[test]
fn effective_ttl_clamps_to_config_cap() {
    assert_eq!(super::effective_ttl_secs(600, 300), 300);
    assert_eq!(super::effective_ttl_secs(60, 300), 60);
    assert_eq!(super::effective_ttl_secs(0, 300), 0);
    assert_eq!(super::effective_ttl_secs(-1, 300), 0);
}
```

- [ ] **Step 2: Run the Rust unit tests**

Run: `cargo test -p packet-browser-client proxy::tests -- --test-threads=1`
Expected: PASS.

- [ ] **Step 3: Add the e2e cache-header test**

Existing browse tests in `e2e/test_e2e.py` use `pb_client` (a fixture yielding a dict with `web_port` and `proc`) and `linbpq_instance` for the full AX.25 stack. Model the new case on `test_browse_portal_page_direct` (around line 313). Append to the same test class:

```python
    @needs_chromium
    @needs_linbpq
    def test_repeat_fetch_carries_cache_headers(
        self, direwolf_pair, pb_server, pb_client, linbpq_instance, test_http_server
    ):
        """A URL fetched twice through the demo stack emits ETag + Cache-Control on both hits."""
        web_port = pb_client["web_port"]

        # Bring AGWPE up.
        post(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=10)

        # Initiate the AX.25 connection through the API.
        post(
            f"http://127.0.0.1:{web_port}/api/connect",
            json={"target_callsign": "N0CALL-7", "port_num": 1},
            timeout=10,
        )
        # Accept the logging disclaimer.
        post(f"http://127.0.0.1:{web_port}/api/consent", json={"accepted": True}, timeout=10)

        # Wait until Connected.
        for _ in range(60):
            data = requests.get(f"http://127.0.0.1:{web_port}/api/agwpe-status", timeout=5).json()
            if data.get("state") == "Connected":
                break
            time.sleep(0.5)
        else:
            pytest.fail("Client never reached Connected state")

        target = test_http_server["url"] + "/portal"

        first = requests.get(
            f"http://127.0.0.1:{web_port}/browse",
            params={"url": target},
            timeout=120,
        )
        assert first.status_code == 200
        assert first.headers.get("ETag", "").startswith('"'), first.headers
        first_cc = first.headers.get("Cache-Control", "")
        assert "private" in first_cc and "max-age" in first_cc, first_cc

        second = requests.get(
            f"http://127.0.0.1:{web_port}/browse",
            params={"url": target},
            timeout=120,
        )
        assert second.status_code == 200
        assert second.headers.get("ETag", "") == first.headers.get("ETag", ""), (
            "Second fetch should have same ETag as first",
            first.headers.get("ETag"),
            second.headers.get("ETag"),
        )
        second_cc = second.headers.get("Cache-Control", "")
        assert "private" in second_cc and "max-age" in second_cc, second_cc
```

Add `import time` and `import pytest` at the top of the file if not already present. If the `test_http_server` fixture yields a different key than `"url"`, inspect its `yield` statement in `conftest.py` and use the correct key.

- [ ] **Step 4: Verify the test collects**

Run: `cd e2e && python -m pytest --collect-only test_e2e.py -q | grep test_repeat_fetch_carries_cache_headers`
Expected: one line matching the new test name.

The full run requires the demo stack (Direwolf, PipeWire, LinBPQ) and matches the `@needs_chromium @needs_linbpq` gates that skip in bare CI environments.

- [ ] **Step 5: Commit**

```bash
git add client/src/proxy.rs e2e/test_e2e.py
git commit -m "test: assert cache headers on first and repeat fetches"
```

---

## Verification checklist (after Task 10)

Run once from the workspace root before considering the feature done:

- [ ] `nix develop -c cargo test --all-features -- --test-threads=1` — all workspace tests pass.
- [ ] `nix develop -c cargo build --release` — release builds succeed for both binaries.
- [ ] Start `./demo.sh`, browse to a URL twice. First fetch shows AX.25 traffic; second fetch is instant with no radio traffic. Reload button triggers a fresh fetch. `/cache` lists both entries. Deleting an entry makes it fetch fresh next time.
- [ ] Test with an origin that sends `Cache-Control: no-store` (a login page works). Confirm nothing is written to `~/.cache/packet-browser/cache.sqlite` for that URL via `sqlite3 ~/.cache/packet-browser/cache.sqlite 'SELECT url FROM entries'`.
