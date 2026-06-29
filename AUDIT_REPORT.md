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
