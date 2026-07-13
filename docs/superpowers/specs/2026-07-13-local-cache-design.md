# Local response cache for the Packet Browser client

**Status:** design approved, pending implementation plan
**Date:** 2026-07-13

## Problem

Every URL the operator visits triggers a full AX.25 round trip: the
`packet-browser-client` proxy in `client/src/proxy.rs` (`handle_browse`) sends a
`GET <url>\n` over the radio to the server on every hit, and the server
re-renders and re-sends the whole page. A typical modern page — even after
Firefox sanitisation and brotli — is 30-500 KB. At 1200 baud that's tens of
seconds of air time per page. Reload the same URL, or bounce between two pages,
and you pay it every time.

The client currently maintains zero cache state. The operator's real browser
also has no useful cache headers to work with, because the proxy's `Html(...)`
responses have no `Cache-Control` or `ETag`.

## Goal

Two coordinated caches, both keyed by URL:

1. **Operator's real browser cache** — served for free by adding
   `Cache-Control` and `ETag` headers to the client's proxy responses. Saves
   even the localhost round trip.
2. **Persistent cache inside `packet-browser-client`** — a SQLite-backed
   `url → (etag, brotli_body, fetched_at, last_used, size, max_age)` store on
   disk. Survives client restarts. When the browser-cache misses, the
   client-persistent cache catches the request before it hits AX.25.

Both layers share the same etag, so a browser-side revalidation and an
AX.25-side revalidation both compare against the same identity.

Freshness is negotiated with two mechanisms:

- **Per-URL TTL sourced from the origin's `Cache-Control` header** (parsed by
  the server via a companion HEAD request against the origin), capped by a
  client-configured maximum.
- **Wire-level conditional revalidation** using `If-None-Match` in the request
  line and a new `Status::NotModified` response, so a stale entry costs ~30
  bytes on the air instead of a full page when it's still valid.

## Non-goals for v1

- **End-to-end origin conditional GET** — client stale → server sends origin
  its `If-None-Match` → possible server 304 without a Firefox render. This
  would save origin bandwidth and Firefox work; it requires tracking origin
  ETag/Last-Modified separately from our client-side sanitized-HTML hash and a
  second etag format on the wire. Deferred.
- **`Vary` support** — none of our requests differ meaningfully on request
  headers; keying only on URL is fine.
- **Cache-encrypted-at-rest.**
- **Prefetching linked pages during idle.**
- **Cross-machine cache sharing.**
- **Cache stampede protection** for concurrent identical URLs from a single
  operator.

## Architecture

```
              Operator's real browser
                    │
                    │  (1) Cache-Control / ETag / If-None-Match
                    ▼
       ┌──────────────────────────┐
       │   packet-browser-client   │
       │   proxy.rs::handle_browse │
       │                          │
       │   ┌──────────────────┐   │
       │   │  cache.rs        │   │  (2) SQLite: url → (etag, brotli, …)
       │   │  (new module)    │◄──┼──── ~/.cache/packet-browser/cache.sqlite
       │   └────────┬─────────┘   │
       └────────────┼─────────────┘
                    │  (3) miss / stale → AX.25 with optional IF-NONE-MATCH
                    ▼
              ─── AX.25 link ───
                    │
                    ▼
       ┌──────────────────────────┐
       │   packet-browser-server   │
       │   main.rs / browser.rs   │
       │                          │
       │   ┌──────────────────┐   │  (4) reqwest HEAD to origin
       │   │  origin CC probe │──┼──── reads Cache-Control / Expires
       │   └──────────────────┘   │
       │   ┌──────────────────┐   │
       │   │  Firefox render  │──┼──── sanitised HTML
       │   └──────────────────┘   │
       └──────────────────────────┘
```

## Wire-protocol change

`shared/src/protocol.rs`.

### Request line — one new optional field

```
GET <url>\n
GET <url> IF-NONE-MATCH <etag>\n        ← new form
POST <url>\n<body_length_be_u32><body>   (unchanged; POSTs are never cached)
```

- `<etag>` is 16 ASCII chars from the base64url alphabet.
- Absence of `IF-NONE-MATCH` means "I have no cached copy — send the full
  page".
- POST requests never carry an etag.

### Response header — one new field, one new status

```
RESP<status> <base64_len> <etag> <max_age>\n<base64_payload>\n
```

- `<status>`: single ASCII digit:
  - `0` — `Ok`
  - `1` — `Err`
  - `2` — `Blocked`
  - `3` — `NotModified` **(new)** — payload is empty, `<base64_len> = 0`.
- `<etag>` — 16-char base64url string on `Ok` and `NotModified`. On `Err` /
  `Blocked` the field is present but is a single ASCII `-` placeholder so the
  header is always four space-separated fields.
- `<max_age>` — signed decimal integer:
  - **positive N** — cache for N seconds (client caps at its configured max).
  - **`0`** — cache but must-revalidate on every use (origin `no-cache`).
  - **negative** — do not cache at all (origin `no-store`, `private`, or
    origin CC probe failed and server config declined to substitute a
    default).

Both changes are backward-incompatible on the wire. Client and server ship
together and share a version bump; no dual-format parsing.

### Etag definition

Etag = first 12 bytes of `SHA-256(sanitized_html_utf8_bytes)`, encoded as
base64url without padding → 16 ASCII chars. Computed on the sanitized HTML
*before* brotli compression, so it's stable across brotli quality tuning.
Collision probability: ~2⁻⁴⁸ per pair — ample.

### Backward compatibility on the wire

Both fields are unconditional in the new format. There is no attempt to parse
the old three-field header format; a decoder that gets fewer than four
space-separated fields returns `ProtocolError::InvalidResponse`. Since
`packet-browser-client` and `packet-browser-server` are released as a matched
pair (they share `packet-browser-shared`), version skew is out of scope.

## Server changes

### New: origin cache-control probe (`server/src/origin_cc.rs`)

Public API:

```rust
pub struct OriginDirectives {
    /// Effective max_age to send on the wire (already clamped/mapped).
    pub max_age: i32,
}

pub fn probe_origin_cc(url: &str, config: &Config) -> OriginDirectives;
```

Behaviour:

1. Issue an HTTP `HEAD` against `<url>` using `reqwest`, routed through the
   in-process filtering proxy so `validate_url` / SSRF policy still applies.
   Timeout: `ORIGIN_CC_HEAD_TIMEOUT_MS` (default `3000`).
2. If the origin returns 405 or an unhelpful body-carrying error, retry once
   with `GET` and discard the body once headers are read.
3. Parse `Cache-Control`, `Expires`, and `Date` from the response headers.
4. Priority order:
   - `Cache-Control` present:
     - `no-store` or `private` → `max_age = -1`.
     - `no-cache` → `max_age = 0`.
     - `s-maxage=N` (takes precedence over `max-age` — we behave as a shared
       cache from the origin's POV) → `max_age = min(N,
       MAX_MAX_AGE_SECONDS)`.
     - `max-age=N` → `max_age = min(N, MAX_MAX_AGE_SECONDS)`.
   - Otherwise, if `Expires` and `Date` both parse as HTTP-dates and
     `Expires > Date` → `max_age = min(Expires - Date, MAX_MAX_AGE_SECONDS)`.
   - Otherwise → `max_age = DEFAULT_MAX_AGE_SECONDS`.
5. If the probe request itself fails or times out → `max_age =
   DEFAULT_MAX_AGE_SECONDS`.

### Config additions (`server/src/config.rs`)

| Env | Default | Meaning |
|-----|---------|---------|
| `ORIGIN_CC_HEAD_TIMEOUT_MS` | `3000` | HEAD timeout so a slow origin doesn't stall the render pipeline. |
| `DEFAULT_MAX_AGE_SECONDS` | `3600` | Emitted when origin gave no useful directive. |
| `MAX_MAX_AGE_SECONDS` | `2592000` (30 days) | Server-side cap on values forwarded to the client. |

### Request handler changes (`server/src/main.rs`)

Where the server currently does `fetch → sanitize → brotli → respond`, the
new flow is:

1. Decode request. If it's a `Request::Get { url, if_none_match }`:
   1. Run `probe_origin_cc(url)` and `browser_render_and_sanitize(url)` in
      parallel (both are I/O-bound).
   2. Compute `etag = sha256_prefix(sanitized_html)`.
   3. If `if_none_match == Some(etag)`: respond with `Status::NotModified`,
      empty payload, current `etag`, current `max_age`. No brotli work.
   4. Otherwise: brotli-compress the sanitized HTML and respond with
      `Status::Ok`, the compressed bytes, `etag`, `max_age`.
2. If it's a `Request::Post { url, body }`: the CC-probe is skipped (POST
   responses are never cached anyway); respond with `Status::Ok`, `etag =
   "-"`, `max_age = -1` so the client won't try to cache it.
3. `Err` / `Blocked` responses: `etag = "-"`, `max_age = -1`.

## Client changes

### New: `client/src/cache.rs`

```rust
pub struct Cache {
    conn:      Mutex<rusqlite::Connection>,
    cap_bytes: u64,
    max_ttl:   Duration,   // per-entry cap; overrides server max_age when smaller
}

pub struct Hit {
    pub etag:        String,
    pub brotli_body: Vec<u8>,
    pub fetched_at:  SystemTime,
    pub max_age:     Duration,  // effective, already min(server_max_age, config.max_ttl)
}

impl Cache {
    pub fn open(dir: &Path, cap_bytes: u64, max_ttl: Duration) -> anyhow::Result<Self>;
    pub fn lookup(&self, url: &str) -> Option<Hit>;
    pub fn insert(
        &self,
        url: &str,
        etag: &str,
        brotli_body: &[u8],
        server_max_age_secs: i32,   // may be negative → skip write
    );
    pub fn touch_fresh(&self, url: &str);        // on 304: bump fetched_at
    pub fn touch_last_used(&self, url: &str);    // on hit-serve: bump last_used
    pub fn delete(&self, url: &str);
    pub fn clear(&self);
    pub fn list(&self) -> Vec<CacheEntry>;       // for /cache admin page
}
```

Freshness helper on `Hit`:

```rust
impl Hit {
    pub fn is_fresh(&self, now: SystemTime) -> bool {
        now.duration_since(self.fetched_at)
           .map(|age| age < self.max_age)
           .unwrap_or(false)
    }
}
```

### Storage schema

```sql
CREATE TABLE entries (
    url         TEXT PRIMARY KEY,
    etag        TEXT NOT NULL,
    brotli_body BLOB NOT NULL,
    fetched_at  INTEGER NOT NULL,   -- unix seconds
    last_used   INTEGER NOT NULL,   -- unix seconds
    size        INTEGER NOT NULL,   -- bytes of brotli_body
    max_age     INTEGER NOT NULL    -- effective TTL in seconds
);
CREATE INDEX idx_last_used ON entries(last_used);
```

Stored value is the brotli-compressed sanitized HTML, exactly as it came off
the wire. On hit-serve the client decompresses and re-runs `rewrite_html` (a
pure function of `(html, base_url)`), which is cheap compared to any radio
work.

Eviction runs inside `insert` in one transaction: if
`(SELECT SUM(size) FROM entries) + new_size > cap_bytes`, delete rows ordered
by `last_used ASC` until under cap.

### Config additions (`client/src/config.rs`)

New `[cache]` section in the INI file:

| Key | Default | Meaning |
|-----|---------|---------|
| `enabled` | `true` | Master on/off toggle. |
| `max_bytes` | `209_715_200` (200 MB) | LRU cap for the on-disk store. |
| `max_ttl_seconds` | `86_400` (24h) | Per-entry TTL cap. Effective TTL for an entry is `min(server_max_age, max_ttl_seconds)`. |
| `dir` | `${XDG_CACHE_HOME:-~/.cache}/packet-browser/` | On-disk location. |

### `handle_browse` flow

Pseudocode replacing the current body of `handle_browse` in `proxy.rs`:

```
fn handle_browse(ctx, url, post_body, nocache):
    if not connected: redirect /connect

    // POST is uncacheable in both directions.
    if post_body is Some:
        response = send_over_ax25(POST url, body, if_none_match = None)
        return render_and_serve(response, url, cache_write = false)

    // nocache=1 skips the lookup but DOES write, so the next normal
    // navigation is fast. Same for the "cache disabled by config" case:
    // treat like a bypass but don't write (config says stay out entirely).
    if not ctx.cache.enabled:
        response = send_over_ax25(GET url, if_none_match = None)
        return render_and_serve(response, url, cache_write = false)

    if nocache:
        response = send_over_ax25(GET url, if_none_match = None)
        if response.status == Ok:
            ctx.cache.insert(url, response.etag, response.brotli_body,
                             response.max_age)
        return render_and_serve(response, url, cache_write = already_done)

    hit = ctx.cache.lookup(url)

    if hit and hit.is_fresh(now):
        ctx.cache.touch_last_used(url)
        return serve_from_hit(hit, url, revalidate_headers_for_browser = true)

    request_etag = hit.map(|h| h.etag)
    wire_response = send_over_ax25(GET url, if_none_match = request_etag)

    match wire_response.status:
      NotModified:
          ctx.cache.touch_fresh(url)
          return serve_from_hit(hit.unwrap(), url, revalidate_headers = true)
      Ok:
          ctx.cache.insert(url, wire_response.etag, wire_response.brotli_body,
                           wire_response.max_age)
          return render_and_serve(wire_response, url, cache_write = already_done)
      Err | Blocked:
          return error_page(wire_response.payload_as_text)
```

`serve_from_hit` decompresses the stored brotli, calls `rewrite_html`, wraps
in the browse UI, and adds these HTTP headers on the axum response:

```
Cache-Control: private, max-age=<remaining_ttl_seconds>
ETag: "<etag>"
```

where `remaining_ttl_seconds = max(0, hit.max_age - (now - hit.fetched_at))`.

### Browser-side revalidation

`handle_browse` inspects the incoming `If-None-Match` request header. If it
matches the current cached etag for that URL and the entry is fresh, respond
`304 Not Modified` with no body. The operator's browser then reuses its own
copy — zero decompression, zero rewrite, zero radio.

### `nocache=1` (Reload button)

The browse page's Reload button links to `/browse?url=<url>&nocache=1`. The
handler treats this like a POST for the purpose of bypassing cache: skip the
lookup, do the full AX.25 fetch, and (this is the point) still write the
result to the cache so the *next* normal navigation is fast.

### Cache-management page (`/cache`)

Route: `GET /cache`. Same CSRF middleware as everything else in
`create_router`.

Table columns: `URL`, `size (KB)`, `fetched`, `last used`, `TTL remaining`,
`delete`. Sorted by `last_used desc`. Paginated at 200 rows.

Actions:
- `POST /api/cache/clear` — empty the store.
- `POST /api/cache/delete` with body `{ "url": "..." }` — remove one entry.

Both actions require CSRF (existing `security_guard` middleware handles it).

## Concurrency

`Cache` wraps a single `rusqlite::Connection` in a `Mutex`. Cache operations
are fast (single-row primary-key lookups, single-row inserts) compared to
AX.25 delays measured in seconds, so contention is not a concern.

## Testing

### Protocol (`shared/src/protocol.rs`)

- `Request::encode/decode` roundtrip with `IF-NONE-MATCH` present and absent.
- `Response::encode/decode_header` roundtrip for each status, verifying the
  four-field header format (status, base64_len, etag, max_age).
- New round-trip for `Status::NotModified` with empty payload.
- Wire-safety check (existing test extended): every byte on the wire must be
  in `[0x20..=0x7e]` or `\n`, including the new fields.

### Server

- `parse_origin_cc` unit tests over these header inputs:
  - `Cache-Control: no-store`
  - `Cache-Control: private, max-age=600`
  - `Cache-Control: no-cache`
  - `Cache-Control: max-age=600`
  - `Cache-Control: s-maxage=60, max-age=600` (→ 60)
  - `Cache-Control: max-age=99999999` (→ clamped to `MAX_MAX_AGE_SECONDS`)
  - No `Cache-Control`, valid `Expires` in the future
  - No `Cache-Control`, no `Expires`
  - Malformed `Cache-Control`
- Integration test via `wiremock` (or similar): main handler issues the HEAD
  probe and forwards the parsed `max_age` on the wire.
- Given a fixed sanitized HTML, `etag` matches the expected 16-char SHA
  prefix.
- `IF-NONE-MATCH` matching the current etag → `Status::NotModified`, empty
  payload.
- `IF-NONE-MATCH` not matching → `Status::Ok`, fresh etag.

### Client cache module (`cache.rs`)

- Insert then lookup returns the same tuple.
- `touch_last_used` updates ordering; oldest entry is evicted first when
  `insert` exceeds `cap_bytes`.
- `max_age < 0` → `insert` is a no-op; subsequent `lookup` returns `None`.
- `max_age = 0` → `insert` writes, but `Hit::is_fresh` returns `false`
  immediately after.
- `max_age > 0` → `is_fresh` transitions from `true` to `false` at the TTL
  boundary.
- Concurrent `insert` and `lookup` from two threads don't deadlock (basic
  smoke test).

### Client `handle_browse` (table-driven)

Cases:
- Cold miss → full fetch, cache write.
- Fresh hit → no radio, `Cache-Control` + `ETag` in browser response.
- Fresh hit + browser `If-None-Match` matches → `304`, no body.
- Stale hit → conditional GET → `NotModified` → cache `fetched_at` bumped.
- Stale hit → conditional GET → `Ok` → cache replaced.
- POST → bypasses cache, no write.
- `nocache=1` → bypasses cache lookup, still writes result.
- `Err` / `Blocked` response → no cache write, existing entry (if any)
  untouched.

### E2E (`e2e/`)

Extend the demo-mode scenarios:

- Fetch the same URL twice; assert the second fetch does not exchange a
  `RESP0` frame over AX.25 (log inspection or fixture-based).
- Origin returns `Cache-Control: no-store`; assert both fetches go
  end-to-end.
- Origin returns `Cache-Control: max-age=1`; assert first fetch is `RESP0`,
  wait 2s, second fetch is a conditional GET that returns `RESP0` (content
  unchanged path).

## Rollout

- Version bumps in both `packet-browser-client` and `packet-browser-server`
  `Cargo.toml`. Operators must upgrade both sides together — call this out
  in the release notes.
- Cache directory is created on first run; missing / corrupted database is
  logged and the client falls back to no-cache mode for the session rather
  than refusing to start.
- Config additions are all optional with defaults; existing `config.ini`
  files continue to work.

## Open questions

None blocking. Follow-ups worth tracking:

- End-to-end origin conditional GET (see non-goals).
- Whether the client should offer a per-page "cached at <timestamp>"
  indicator in the browse UI. Nice to have; deferred.
