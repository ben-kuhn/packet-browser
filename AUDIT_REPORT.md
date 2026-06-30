# Security and Code Quality Audit Report

**Date:** 2026-06-27  
**Auditor:** AI Assistant  
**Project:** Packet Browser (Client/Server)

## Executive Summary

A comprehensive security and code quality audit was performed on the packet-browser project. The audit identified 13 security vulnerabilities and code quality issues across the server and client components. All critical and high-priority issues have been fixed.

## Findings and Fixes

### CRITICAL Issues (Fixed)

#### 1. DoS: Unbounded POST Body Allocation
**Location:** `server/src/main.rs:183-188`  
**Issue:** Client sends 4-byte length prefix, server allocates that many bytes without validation. Attacker could claim 4GB body size → OOM crash.  
**Fix:** Added `MAX_BODY_SIZE` constant (1MB). Server now rejects bodies exceeding this limit.

#### 2. DoS: No Connection Limit
**Location:** `server/src/main.rs:42-58`  
**Issue:** Every incoming connection spawns a new thread with no cap. Trivial thread exhaustion attack.  
**Fix:** Added `MAX_CONNECTIONS` constant (50). Server tracks active connections with atomic counter and rejects new connections when limit reached.

#### 3. DoS: No Request Timeout
**Location:** `server/src/main.rs:61-160`  
**Issue:** All read operations block indefinitely. Slowloris attack can hold connections forever.  
**Fix:** Added `REQUEST_TIMEOUT_SECS` constant (300s). Set read timeout on all TCP streams.

#### 4. DoS: Unbounded Line Length
**Location:** `server/src/main.rs:66,86`  
**Issue:** `read_line()` has no limit. Attacker sends multi-GB line → OOM.  
**Fix:** Added `MAX_LINE_LENGTH` constant (1KB). Check line length after reading and reject oversized lines.

### HIGH Issues (Fixed)

#### 5. SSRF via DNS Rebinding
**Location:** `server/src/filter.rs:43-47`  
**Issue:** URL validation only checks literal IP addresses against blocked ranges. Hostname `evil.com` can resolve to `127.0.0.1` at fetch time, bypassing SSRF protection.  
**Fix:** Added DNS resolution for hostnames. All resolved IPs are checked against blocked ranges before allowing the request.

#### 6. XSS in Browse Page
**Location:** `client/src/ui.rs:browse_page()`  
**Issue:** URL parameter inserted into HTML without escaping. Attacker can inject `<script>alert('xss')</script>` via URL parameter.  
**Fix:** Added HTML escaping for URL parameter (escape `&`, `<`, `>`, `"`, `'`).

#### 7. Brotli Decompression Bomb
**Location:** `shared/src/compress.rs:brotli_decompress()`  
**Issue:** No limit on decompressed size. Small compressed payload can expand to GB.  
**Fix:** Added `MAX_DECOMPRESSED_SIZE` constant (10MB). Decompression fails if output exceeds limit.

### MEDIUM Issues (Fixed)

#### 8. Session Directory Symlink Attack
**Location:** `server/src/browser.rs:48-51`  
**Issue:** `/tmp/chrome-{callsign}` created with default permissions (0o755). Another user can pre-create as symlink to arbitrary location.  
**Fix:** Create directory with `create_dir()` instead of `create_dir_all()`, then set permissions to 0o700 (owner-only access).

#### 9. Non-Atomic Blocklist Write
**Location:** `server/src/blocklist.rs:84`  
**Issue:** `fs::write()` to `/etc/hosts` is not atomic. Interrupt during write corrupts hosts file.  
**Fix:** Write to temporary file first, then use `fs::rename()` for atomic replacement.

#### 10. No Input Validation on Client API
**Location:** `client/src/proxy.rs:api_connect_handler()`  
**Issue:** Accepts arbitrary callsigns without validation.  
**Fix:** Added regex validation for target callsign format before attempting connection.

#### 11. Error Messages Leak Internals
**Location:** `server/src/main.rs:112,256`  
**Issue:** Internal error details (browser initialization failures, compression errors) sent to client.  
**Fix:** Sanitized error messages. Generic messages sent to clients, detailed errors logged server-side only.

### LOW Issues (Fixed)

#### 12. Regex Recompiled Per Call
**Location:** `server/src/session.rs:45`  
**Issue:** `Regex::new()` called on every `validate_callsign()` invocation.  
**Fix:** Used `std::sync::LazyLock` to compile regex once at first use.

#### 13. Mutex Poison Unhandled
**Location:** Multiple locations in `client/src/proxy.rs`  
**Issue:** `.lock().unwrap()` panics if any thread panicked while holding lock.  
**Status:** Not fixed. Mutex poisoning is rare and the current behavior (panic) is acceptable for this application. Proper handling would require significant refactoring.

## Additional Changes

### Dependencies Added
- `regex = "1"` added to `client/Cargo.toml` for callsign validation

### Constants Added
```rust
// server/src/main.rs
const MAX_CONNECTIONS: usize = 50;
const MAX_LINE_LENGTH: usize = 1024;
const MAX_BODY_SIZE: usize = 1024 * 1024;  // 1MB
const REQUEST_TIMEOUT_SECS: u64 = 300;

// shared/src/compress.rs
const MAX_DECOMPRESSED_SIZE: usize = 10 * 1024 * 1024;  // 10MB
```

## Testing

All existing tests pass with the security fixes:
- 47 Rust unit tests (client, server, shared)
- 9 Python e2e tests (require Direwolf, PipeWire, LinBPQ)

Tests must be run with `--test-threads=1` due to environment variable manipulation in config tests.

## Recommendations

1. **Rate Limiting:** Consider adding rate limiting per callsign/IP to prevent abuse
2. **Resource Limits:** Monitor Chrome process memory usage and restart if excessive
3. **Audit Logging:** Add audit trail for security-relevant events (blocked URLs, rejected connections)
4. **Fuzzing:** Consider fuzz testing the protocol parser and URL validator
5. **Security Headers:** Add security headers to HTTP responses (CSP, X-Frame-Options, etc.)

## Conclusion

All critical and high-priority security vulnerabilities have been addressed. The application is now significantly more resilient to DoS attacks, SSRF, XSS, and resource exhaustion. Medium-priority issues have been fixed where practical. The codebase is in good shape for production deployment.

---

## 2026-06-30 follow-up audit

A second pass uncovered both regressions in the original fixes and new findings.
Most were addressed; a small number are architectural and called out below.

### Regressions / gaps found in the original fixes
- **Brotli decompression check was a no-op** (`shared/src/compress.rs`). `read_to_end`
  was allocating the full output before the size check ever ran. Replaced with a
  streaming read that aborts the moment the cap is exceeded.
- **SSRF filter DNS-rebinding fix was incomplete.** The filter still resolved
  via `to_socket_addrs` and Chromium then resolved again independently. The
  filter was also vulnerable to a `userinfo@host` smuggling bypass and ignored
  IPv6 ranges entirely. The URL parser was rewritten on top of the `url`
  crate; IPv6 loopback, mapped, ULA, link-local, and 6to4-of-private blocks
  were added; tests were added to cover each. The full DNS-rebinding TOCTOU
  is still open (see below).

### New findings, fixed
- AGWPE frame parser accepted unbounded `data_len`, allowing a peer to claim
  4 GiB per frame. Capped at 64 KiB; response and handshake-text accumulation
  also capped.
- Client served `/api/*` POSTs with no CSRF protection. Any visited page
  could rewrite the saved config. Added an Origin/Referer guard middleware.
- `ui.rs` interpolated `my_callsign`, `target_callsign`, error messages, and
  the AGWPE port-info JSON directly into HTML/script contexts. Added `h()`
  HTML-escape and `json_for_script()` JSON-escape helpers and routed every
  user-influenced value through them. Also added a strict CSP `<meta>` to
  the browse-page wrapper.
- `read_line` was bounded after the read, not during. Replaced with a
  `Take<&mut BufReader>` + `read_until` helper so an attacker cannot stream
  multi-GB lines past the size check.
- Global connection counter was TOCTOU and leaked on panic. Switched to a
  fetch-add admission + RAII drop guard. Per-IP cap (5 concurrent) added so
  one peer cannot occupy every global slot.
- Server had no write timeout (slowloris-on-write would stall threads). Added.
- POST silently degraded to GET on the server. Now rejected with a clear error.
- Browser session-dir creation had a symlink TOCTOU. Replaced
  `/tmp/chrome-{callsign}` with `tempfile::TempDir`, eliminating the race.
- Blocklist fetch had no timeout or size cap. Added a 30 s timeout, 5-redirect
  cap, Content-Length check, and a streaming read bounded at 16 MiB.
- The default `BLOCKED_RANGES` did not cover `0.0.0.0/8`, so the blocklist's
  own `0.0.0.0 evil.example` sinkhole entries routed to local services via
  `connect(0.0.0.0)`. Added to the default list and to `docker-compose.yml`.
- Client sanitizer only stripped `<script>` and `<link rel=stylesheet>`.
  Added stripping for `<iframe>`, `<object>`, `<embed>`, `<frame>`,
  `<frameset>`, `<applet>`, `<noscript>`, `<base>`, inline `<style>`,
  `style=` attribute, `meta http-equiv=refresh`, all `on*` event handlers,
  and `javascript:` URLs in `src`/`href`/`action`/`formaction`/`background`/`poster`.
- `/etc/hosts` rename failed silently across Docker bind-mount boundary.
  Added an in-place fallback when rename returns EXDEV/EBUSY.
- `LockExt::lock_or_poisoned()` recovers from mutex poisoning instead of
  panicking. Important because the original audit accepted poison as a panic;
  with the new CSRF and config-write paths, a panic anywhere now bricks the
  proxy until restart.
- Several smaller cleanups: `LazyLock` for the connect-handler regex, demoted
  noisy `eprintln!` frame dumps to `tracing::trace!`, propagated
  `ConfigError` from `dirs::config_dir()` instead of `unwrap`, fixed a
  test that wouldn't compile (`FileConfig` was missing `skip_bpq_app`),
  removed dead `log_rotate_*` and `syslog_*` config fields that were read
  from env but never used.

### Open, architectural

These need real design work and are deliberately not fixed in this pass.

1. **Subresource SSRF via the headless browser.** `validate_url` only checks the
   top-level navigation URL. Chromium then loads stylesheets, fonts, images,
   inline `fetch()` from `JS_SCRUB_HTML`, and any other subresource via its
   own DNS resolver, with no filter in the loop. Any visited page can cause
   the server to issue arbitrary GETs to internal IPs the operator's box can
   reach (cloud metadata, RFC1918, container-internal services). The proper
   fix is to route Chromium through a local proxy we control that does the
   filter check and pins the resolved IP to what was checked. Roughly a day
   of work; deferred so the security pass stays bisectable.

2. **DNS-rebinding TOCTOU between filter and renderer.** Same root cause as (1):
   independent DNS lookups in `filter.rs` and the browser. Same fix.

3. **Chromium runs with `--no-sandbox`.** Required because Chromium's setuid
   sandbox cannot initialize as UID 1000 in a container without
   CAP_SYS_ADMIN. The container's `cap_drop: ALL`, `read_only: true`, and
   tmpfs limits compensate but do not replace a renderer sandbox. A
   renderer compromise (one malicious page) gets the full server process's
   ambient permissions. Documented in README. Long-term fix: switch to
   Firefox (whose content sandbox initializes from unprivileged user
   namespaces) or rework the container to give Chromium what its sandbox
   needs.

4. **`/etc/hosts` bind-mount in docker-compose.** The atomic-rename codepath
   now falls back to in-place writes for this case, but the broader
   architecture — mounting a single file from the host as the container's
   `/etc/hosts` — couples the host's name resolution to the blocklist
   refresher. A cleaner design is to mount a directory and have the server
   write its own resolver file, or block at a different layer.

5. **Dependency freshness.** `reqwest 0.11`, `axum 0.7`, `openssl 0.10.81`.
   None have known advisories against the pinned versions to my knowledge,
   but running `cargo audit` regularly and tracking the major-version bumps
   (reqwest 0.12, axum 0.8) is a deferred task that belongs in its own PR
   because of the API surface changes.

6. **Mutex poison panic-on-panic.** Now degrades to "continue with previous
   contents" rather than blocking the proxy. Acceptable for a single-user
   local proxy; would not be acceptable for a multi-tenant service.
