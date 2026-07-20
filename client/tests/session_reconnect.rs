// Integration test for the graceful-reconnect flow. Spins up a fake AGWPE
// server that scripts: (1) accepts the initial handshake, (2) delivers a
// canned response to the first request, (3) sends *** DISCONNECTED on the
// second request, (4) accepts a reconnect handshake, (5) delivers a canned
// response to the retried request.
//
// The test asserts:
//  - Exactly one reconnect happened (server saw two AGREE lines total: one
//    from initial connect, one from auto-consent).
//  - The disclaimer text stored after the first AGREE was sent back as-is
//    for auto-consent — no operator interaction required.
//  - The retried request returned the canned response.

use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

// If the client crate doesn't yet expose helpers that let a test drive
// AgwpeManager end-to-end, this test is skipped (marked #[ignore]) with a
// TODO to expose them. Prefer to expose the minimum needed rather than
// exercising the full HTTP proxy — this test is about the AGWPE path.

#[tokio::test]
#[ignore = "requires test-only exposure of client bind points; enable when Task 8 wires them"]
async fn test_reconnect_on_disconnected_payload() {
    // NOTE: this test is intentionally left as an ignored skeleton. Enabling
    // it requires exposing enough of the client's connect + send pipeline
    // that a test can drive AgwpeManager against a mock TCP listener.
    // See docs/superpowers/specs/2026-07-20-graceful-reconnect-design.md
    // for the acceptance criteria this test proves.
}
