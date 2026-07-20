# Graceful Reconnect on Session Drop

## Problem

When the server tears down an AX.25 session mid-conversation — e.g. after a
headless-Firefox JS error, an idle read timeout, or a `Resource temporarily
unavailable` on the server's side of the pipe — the client keeps showing
**Connected** and continues transmitting into a dead session. The next `/browse`
request goes out on the wire, gets a small garbage frame back (or nothing at
all), and either hangs indefinitely or renders a broken page. The operator has
no way to recover short of clicking **Disconnect** and manually re-running the
whole handshake.

Observed live during a demo run: server logged
`[PROTO] Read error from W1TEST: Resource temporarily unavailable (os error 11)`
followed by `[CONNECT] Session ended for W1TEST`. Client's debug log continued
to report `Status: Connected` and the subsequent Reload sent 24 bytes and
received 31 bytes of unparseable data at the same millisecond timestamp.

## Goals

- Detect server-side session tear-downs in the client Rx path.
- Automatically recover once per pending request, transparent to the operator
  when the recovery succeeds.
- Preserve the AGREE consent audit trail — the server continues to receive an
  AGREE line on every new AX.25 session.
- Serialize reconnect attempts: at most one AGWPE + BPQ + AGREE handshake in
  flight per client process at any time.
- Surface an actionable error page when auto-recovery fails or is disallowed.

## Non-goals

- AGWPE-layer TCP drop recovery (Direwolf disappearing) — separate issue.
- Backoff, jitter, or multi-attempt retry loops. One retry per request is the
  ceiling; further failures require operator action.
- Auto-reconnect from admin pages like `/cache` or `/configuration`. Only
  `/browse` requests trigger the recovery path.
- Cross-operator-session consent memory. `last_agreed_disclaimer` lives in
  process state and is cleared on operator-initiated disconnect.

## Detection

Three failure modes must trigger the same `SessionDied` internal event:

1. **AGWPE remote-disconnect notification.** AGWPE sends a `'d'` (0x64) frame
   whose data payload starts with `*** DISCONNECTED FROM ` when the remote
   station closes the AX.25 link. Today `client/src/agwpe.rs` maps 0x64 to
   `FrameType::DataReceived` unconditionally and hands the payload to the
   response framer, which is where the 31-byte garbage in the debug log came
   from. Add pattern-matching against `b"*** DISCONNECTED"` (mirroring the
   existing `starts_with(b"***")` check for `Connect` frames at ~line 1054)
   and treat matches as `SessionDied` rather than data.
2. **Response-side read timeout.** `handle_send_request` currently waits
   forever after logging `Waiting for response...`. Wrap the read in
   `tokio::time::timeout` with a configurable duration (default 30s). On
   timeout, emit `SessionDied` with reason `"no response after Ns"`.
3. **Malformed response payload.** If the framer scans through more bytes than
   the maximum plausible header without finding a `RESP` marker, or if the
   payload contains an embedded `*** DISCONNECTED` marker, emit `SessionDied`
   with reason `"malformed response"`.

Each `SessionDied` event transitions state to
`ConnectionState::Reconnecting { reason }` and returns
`AgwpeError::SessionDied { reason }` from the in-flight `handle_send_request`
call.

## Reconnect flow

`/browse` handlers in `client/src/proxy.rs` (the two call sites at ~line 291
and ~line 337) call the new `send_with_reconnect(request, ctx)` helper instead
of `handle_send_request` directly.

`send_with_reconnect` behavior:

1. Attempt the request.
2. On `AgwpeError::SessionDied` (only that variant — other errors propagate
   directly):
   1. Acquire the reconnect lock (see "Serialization" below).
   2. If the caller won the lock, drive the reconnect:
      - Send an AX.25 close frame to clean up the client's side.
      - Transition state to `Reconnecting { reason }`.
      - Run the same handshake code as the user-initiated AX.25 Connect:
        AX.25 connect → wait for callsign prompt → send callsign → wait for
        disclaimer.
      - **Auto-consent check.** `AppState.last_agreed_disclaimer:
        Option<String>` was set inside the consent-approval handler in
        `proxy.rs` when the operator first clicked I Agree. If it equals the
        disclaimer text just received, the background task sends `AGREE\n`
        without notifying the UI, and state transitions to `Connected`. If it
        differs, or is `None`, the reconnect fails with
        `AgwpeError::NeedsReconsent` (see error surfaces below). The
        disclaimer comparison is exact-string equality; even whitespace
        differences require manual re-consent.
      - Release the lock, notifying waiters.
   3. If the caller lost the lock, wait on a `tokio::sync::Notify` published
      when the flag clears, then check state: if `Connected`, retry the
      request; if `Error(_)`, fail with the recovery error.
3. Retry the pending request once (only the caller that drove the reconnect,
   and any waiters that saw a successful recovery). If the retry also fails
   with `SessionDied`, propagate the error without a second reconnect attempt.

Server-side compliance is preserved: the server still receives and logs an
`AGREE` line for every new AX.25 session, whether AGREE originated from a
modal click or from the auto-consent path.

## Serialization

At most one reconnect handshake may be in flight per client process at any
time. This is a first-class invariant, not an optimization.

Implementation lives in the AGWPE background actor:

- `BackgroundState.reconnect_in_progress: bool` guarded by the same actor
  mutex that already serializes AGWPE writes.
- `BackgroundState.reconnect_done: tokio::sync::Notify` published when the
  flag clears.
- `send_with_reconnect` CAS's the flag on `SessionDied`. Winner drives, loser
  awaits the `Notify`. All losers wake when the winner finishes and read the
  post-reconnect state to decide whether to retry or fail.

Interactions with operator actions:

- **Operator clicks Disconnect during reconnect.** Disconnect wins. Reconnect
  aborts, `reconnect_in_progress` clears, all pending waiters wake and fail
  with `AgwpeError::DisconnectedByOperator`.
- **Operator clicks AX.25 Connect during reconnect.** No-op; reconnect is
  already driving toward the same terminal state.

## State, UI, and config changes

**`client/src/state.rs`:**
- Add `ConnectionState::Reconnecting { reason: String }` variant. `Display`
  renders it as `"Reconnecting: <reason>"`.
- Add `AppState.last_agreed_disclaimer: Option<String>`. Set by the consent
  handler in `proxy.rs`, cleared by the AX.25 Disconnect handler.

**`client/src/agwpe.rs`:**
- New `AgwpeError` variants: `SessionDied { reason: String }`,
  `NeedsReconsent`, `DisconnectedByOperator`.
- `BackgroundState` gains `reconnect_in_progress: bool` and `reconnect_done:
  Arc<Notify>`, both guarded by the existing actor lock.
- `handle_send_request` wraps its read in `tokio::time::timeout` and returns
  `SessionDied` on the three failure modes above.
- New `handle_reconnect` function reuses the AX.25 connect + BPQ handshake
  paths; branches on the disclaimer comparison.

**`client/src/proxy.rs`:**
- New `send_with_reconnect(request, ctx)` wrapper called from the two
  existing `/browse` send sites (~L291, ~L337).
- Consent-approval handler stores accepted disclaimer text on `AppState`.
- Error rendering:
  - `NeedsReconsent` → error page with a **Reconnect** link to `/connect` and
    explanatory copy ("Session dropped and disclaimer text changed — please
    re-consent").
  - Other post-retry errors → standard error page with reason string and a
    Reconnect link.

**`client/src/ui.rs`:**
- New status pill class `status-reconnecting` (yellow, matching
  `status-connecting`).

**`client/src/config.rs`:**
- New optional `[connection]` section:
  - `response_timeout_secs: u64 = 30`
  - `auto_reconnect: bool = true` — kill switch. When `false`, `SessionDied`
    propagates directly as an error without any reconnect attempt (useful for
    debugging and for operators who prefer to control every re-consent
    manually).

## Testing

Unit tests:
- `*** DISCONNECTED FROM ...` payload is classified as `SessionDied`, not
  handed to the response framer.
- `last_agreed_disclaimer` comparison: exact match, whitespace-different
  match (should NOT match), `None` (should NOT match).
- Response-timeout `tokio::time::timeout` fires within tolerance and returns
  `SessionDied`.

Integration tests (in-process, mock AGWPE stream):
- Mid-request session drop with matching disclaimer → exactly one full
  handshake occurs in the log, request succeeds on retry.
- Mid-request session drop with differing disclaimer → `NeedsReconsent`
  surfaces, no auto-AGREE frame emitted on the wire.
- Two concurrent `/browse` requests hit a dead session → exactly one
  handshake sequence in the log, both requests observe the same terminal
  outcome (both succeed or both fail).
- Operator clicks Disconnect mid-reconnect → all pending requests fail with
  `DisconnectedByOperator`, no AGREE frame emitted for the aborted attempt.

The existing e2e demo path (`e2e/`) should continue to pass unchanged.

## Out of scope for this spec

- AGWPE TCP drop (Direwolf process death). Requires a separate reconnect
  policy for the AGWPE socket itself.
- Retry policies beyond one attempt per request. Multi-attempt behavior can
  be revisited if operational experience shows single-retry insufficient.
- Reconnect surfaces from the `/cache` admin page or configuration edits.
