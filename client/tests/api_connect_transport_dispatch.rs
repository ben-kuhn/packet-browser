/// Integration test for Task 10: `POST /api/connect` dispatches to the
/// correct transport implementation based on the `transport` field.
///
/// Weaker-assertion form (per brief): because no VARA modem is listening on
/// the test machine, the VARA path will fail at the TCP-connect step.  The
/// test asserts that the response body mentions "VARA" — proving that the
/// field was parsed and the VARA code path was reached rather than silently
/// falling through to the AX.25 path.
///
/// The AX.25 case is exercised by omitting the `transport` field; it fails
/// for the same reason (no AGWPE modem running) but the error message does
/// NOT contain the string "VARA".
use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::{Request, StatusCode};
use axum::Router;
use tokio::sync::broadcast;

use packet_browser_client::config::FileConfig;
use packet_browser_client::proxy::{self, AppContext, HostAllowlist};
use packet_browser_client::state::create_shared_state;
use packet_browser_client::transport::{
    self,
    manager::TransportManager,
};

/// Build a minimal `AppContext` backed by a real `TransportManager` (no mock
/// modem — connections will fail, which is expected for the weaker assertion).
fn make_ctx() -> Arc<AppContext> {
    let config = FileConfig::default();
    let shared_state = create_shared_state(config.clone());
    let (log_tx, _rx) = broadcast::channel(16);

    let agwpe_transport: Box<dyn transport::Transport> =
        Box::new(transport::agwpe::AgwpeTransport::new());
    let manager = TransportManager::spawn(
        agwpe_transport,
        shared_state.clone(),
        log_tx.clone(),
        config.connection.response_timeout_secs,
    );

    let listen_ip: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    Arc::new(AppContext {
        state: shared_state,
        agwpe: tokio::sync::Mutex::new(manager),
        log_tx,
        host_allowlist: HostAllowlist::new(listen_ip, vec![]),
        cache: None,
        cache_max_ttl: std::time::Duration::from_secs(86_400),
        config,
    })
}

fn make_router(ctx: Arc<AppContext>) -> Router {
    proxy::create_router(ctx)
}

/// Drive a handler in-process: build a request, call it through the router,
/// collect the body, and return (status, body_string).
async fn call(app: &Router, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    use axum::body::Body;
    use tower::ServiceExt; // for `oneshot`

    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();

    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&bytes).to_string();
    (status, text)
}

#[tokio::test]
async fn post_api_connect_vara_fm_reaches_vara_path() {
    let ctx = make_ctx();
    let app = make_router(ctx);

    // Use port 0 (unlikely to have any listener) so the TCP connect fails
    // immediately without needing a timeout.
    let body = r#"{
        "target_callsign": "N0CALL-8",
        "port_num": 0,
        "transport": "vara_fm",
        "vara_cmd_host": "127.0.0.1",
        "vara_cmd_port": 19999,
        "vara_data_host": "127.0.0.1",
        "vara_data_port": 20000
    }"#;

    let (status, text) = call(&app, "POST", "/api/connect", body).await;

    // The status should be 200 (handler always returns 200 with ok/error JSON).
    assert_eq!(status, StatusCode::OK, "unexpected HTTP status; body={}", text);

    let json: serde_json::Value = serde_json::from_str(&text)
        .expect("response should be valid JSON");

    // Either: (a) the VARA modem happened to be running and we succeeded, or
    // (b) the TCP connect failed and the error string mentions VARA.
    if json["ok"].as_bool() == Some(true) {
        // Unlikely in CI, but not wrong.
        return;
    }

    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.to_lowercase().contains("vara"),
        "Expected error to mention VARA when transport=vara_fm, got: {:?}",
        error
    );
}

#[tokio::test]
async fn post_api_connect_ax25_does_not_mention_vara() {
    let ctx = make_ctx();
    let app = make_router(ctx);

    // AX.25 path — omit the `transport` field entirely so it defaults to ax25.
    let body = r#"{
        "target_callsign": "N0CALL-8",
        "port_num": 0
    }"#;

    let (status, text) = call(&app, "POST", "/api/connect", body).await;

    assert_eq!(status, StatusCode::OK, "unexpected HTTP status; body={}", text);

    let json: serde_json::Value = serde_json::from_str(&text)
        .expect("response should be valid JSON");

    let error = json["error"].as_str().unwrap_or("");
    // The AX.25 path must NOT produce a VARA-mentioning error.
    assert!(
        !error.to_lowercase().contains("vara"),
        "AX.25 path should not mention VARA in its error, got: {:?}",
        error
    );
}

#[tokio::test]
async fn post_api_connect_unknown_transport_returns_error() {
    let ctx = make_ctx();
    let app = make_router(ctx);

    let body = r#"{
        "target_callsign": "N0CALL-8",
        "port_num": 0,
        "transport": "pigeon"
    }"#;

    let (status, text) = call(&app, "POST", "/api/connect", body).await;

    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(json["ok"].as_bool(), Some(false));
    let error = json["error"].as_str().unwrap_or("");
    assert!(
        error.contains("pigeon") || error.to_lowercase().contains("unknown"),
        "Expected error about unknown transport, got: {:?}",
        error
    );
}
