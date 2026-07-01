use axum::{
    extract::{Extension, Query},
    http::{Method, Request as HttpRequest, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event, Sse},
        Html, IntoResponse, Redirect, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::{Arc, LazyLock};
use tokio::sync::broadcast;

static CALLSIGN_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[a-zA-Z0-9]{1,3}[0-9][a-zA-Z0-9]{0,3}[a-zA-Z]$").unwrap()
});

use crate::agwpe::AgwpeManager;
use crate::config::FileConfig;
use crate::rewrite::rewrite_html;
use crate::state::{ConnectionState, DebugLogEntry, LockExt, SharedState};
use crate::ui;
use packet_browser_shared::compress::brotli_decompress;
use packet_browser_shared::protocol::{Request, Response as ProtocolResponse, Status};

pub struct AppContext {
    pub state: SharedState,
    pub agwpe: AgwpeManager,
    pub log_tx: broadcast::Sender<DebugLogEntry>,
    pub host_allowlist: HostAllowlist,
}

/// A whitelist of hostnames we are prepared to serve on. Used to block DNS
/// rebinding: an attacker's page whose DNS flips to the client's IP would send
/// a Host header with the attacker's hostname, which we reject up-front.
///
/// The default set is derived from `--listen-addr`:
///   - Always: "localhost", plus any IP literal that is loopback.
///   - If bound to a specific non-loopback IP: that literal IP is added.
///   - If bound to the unspecified address (0.0.0.0 / ::): any IP literal on
///     a local network (RFC1918 v4, ULA v6, or link-local for either) passes.
///     This is the "shelter LAN" case; the operator opted into wide binding.
///   - `--allowed-hosts` appends any additional hostnames (e.g. mDNS names).
#[derive(Debug, Clone)]
pub struct HostAllowlist {
    hostnames: HashSet<String>,
    bound_ip: Option<IpAddr>,
    allow_lan: bool,
}

impl HostAllowlist {
    pub fn new(listen_ip: IpAddr, extra_hostnames: Vec<String>) -> Self {
        let mut hostnames: HashSet<String> =
            std::iter::once("localhost".to_string()).collect();
        for h in extra_hostnames {
            let h = h.trim().to_ascii_lowercase();
            if !h.is_empty() {
                hostnames.insert(h);
            }
        }
        Self {
            hostnames,
            bound_ip: (!listen_ip.is_loopback() && !listen_ip.is_unspecified())
                .then_some(listen_ip),
            allow_lan: listen_ip.is_unspecified(),
        }
    }

    pub fn contains_host_header(&self, host_header: &str) -> bool {
        let hostname = strip_port_from_host(host_header);

        // Try to parse as an IP literal (also handles bracketed IPv6).
        let ip_str = hostname
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(hostname);
        if let Ok(ip) = ip_str.parse::<IpAddr>() {
            if ip.is_loopback() {
                return true;
            }
            if let Some(bound) = self.bound_ip {
                if bound == ip {
                    return true;
                }
            }
            if self.allow_lan && is_lan_ip(&ip) {
                return true;
            }
            return false;
        }

        // Not an IP: match against configured hostnames (case-insensitive).
        self.hostnames.contains(&hostname.to_ascii_lowercase())
    }
}

fn strip_port_from_host(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[') {
        // "[v6]:port" or "[v6]"
        if let Some(end) = rest.find(']') {
            return &host[..end + 2];
        }
    }
    // "host:port" or bare "host". IPv6 without brackets doesn't come from
    // a browser Host header, so ignore that case.
    host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host)
}

fn is_lan_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            let o = v6.octets();
            // ULA: fc00::/7 -- first byte 1111 111x.
            let ula = (o[0] & 0xfe) == 0xfc;
            // Link-local: fe80::/10 -- first byte 0xfe, second byte's top two bits 10.
            let link_local = o[0] == 0xfe && (o[1] & 0xc0) == 0x80;
            ula || link_local
        }
    }
}

pub fn create_router(ctx: Arc<AppContext>) -> Router {
    Router::new()
        .route("/", get(root_handler))
        .route("/connect", get(connect_page_handler))
        .route("/configuration", get(configuration_page_handler))
        .route("/browse", get(browse_get_handler))
        .route("/browse", post(browse_post_handler))
        .route("/api/agwpe-status", get(api_agwpe_status_get))
        .route("/api/agwpe-status", post(api_agwpe_status_post))
        .route("/api/connect", post(api_connect_handler))
        .route("/api/disconnect", post(api_disconnect_handler))
        .route("/api/config", get(api_config_get))
        .route("/api/config", post(api_config_post))
        .route("/events", get(events_handler))
        .layer(middleware::from_fn_with_state(ctx.clone(), security_guard))
        .layer(Extension(ctx))
}

// Two-part gate against DNS rebinding and CSRF:
//
//   1. The Host header must be one we recognize as ours (loopback, our
//      listen IP, LAN IPs when bound to 0.0.0.0, or an explicit
//      --allowed-hosts entry). This blocks DNS rebinding: a hostile page
//      whose DNS flips to our IP mid-session still carries Host: evil.com,
//      which is not in the allowlist.
//
//   2. On POST additionally, Origin's authority must match the Host header.
//      Textbook same-origin check for classic CSRF.
//
// The Referer fallback was removed on purpose: modern browsers always send
// Origin on cross-origin POSTs, and Referer is easier for an attacker to
// suppress, so a Referer-only fallback re-opens the bypass this guard is
// closing.
async fn security_guard(
    axum::extract::State(ctx): axum::extract::State<Arc<AppContext>>,
    req: HttpRequest<axum::body::Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let headers = req.headers();
    let host = headers.get("host").and_then(|v| v.to_str().ok());

    let host = match host {
        Some(h) if ctx.host_allowlist.contains_host_header(h) => h,
        _ => {
            tracing::warn!(
                "Host allowlist rejected {} {} (host={:?})",
                req.method(),
                req.uri().path(),
                host
            );
            return Err(StatusCode::FORBIDDEN);
        }
    };

    if req.method() == Method::POST {
        let origin = headers.get("origin").and_then(|v| v.to_str().ok());
        let ok = match origin {
            Some(o) => origin_matches_host(o, host),
            None => false,
        };
        if !ok {
            tracing::warn!(
                "CSRF guard rejected {} {} (origin={:?}, host={:?})",
                req.method(),
                req.uri().path(),
                origin,
                host
            );
            return Err(StatusCode::FORBIDDEN);
        }
    }

    Ok(next.run(req).await)
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    // Strip "http://" / "https://" then compare the authority (host:port)
    // against the Host header verbatim.
    let authority = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    match authority {
        Some(a) => a == host,
        None => false,
    }
}

async fn root_handler() -> impl IntoResponse {
    Redirect::to("/connect")
}

async fn connect_page_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> impl IntoResponse {
    let state = ctx.state.lock_or_poisoned();
    let my_callsign = state.config.my_callsign.clone();
    let target_callsign = state.config.target_callsign.clone();
    let connection_state = state.connection_state.to_string();
    let connection_state_class = match state.connection_state {
        ConnectionState::Disconnected => "status-disconnected",
        ConnectionState::AgwpeConnected => "status-agwpe-connected",
        ConnectionState::Connecting => "status-connecting",
        ConnectionState::Connected => "status-connected",
        ConnectionState::Error(_) => "status-error",
    };
    let ports_json = serde_json::to_string(&state.available_ports).unwrap_or_else(|_| "[]".to_string());
    drop(state);

    Html(ui::connect_page(
        &my_callsign,
        &target_callsign,
        &connection_state,
        connection_state_class,
        &ports_json,
    ))
}

async fn configuration_page_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> impl IntoResponse {
    let state = ctx.state.lock_or_poisoned();
    let agwpe_host = state.config.agwpe_host.clone();
    let agwpe_port = state.config.agwpe_port;
    let my_callsign = state.config.my_callsign.clone();
    let target_callsign = state.config.target_callsign.clone();
    let bpq_command = state.config.bpq_command.clone();
    let skip_bpq_app = state.config.skip_bpq_app;
    drop(state);

    Html(ui::configuration_page(
        &agwpe_host,
        agwpe_port,
        &my_callsign,
        &target_callsign,
        &bpq_command,
        skip_bpq_app,
    ))
}

#[derive(Deserialize)]
struct BrowseParams {
    url: Option<String>,
}

async fn browse_get_handler(
    Query(params): Query<BrowseParams>,
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Response {
    let url = match params.url {
        Some(u) if !u.is_empty() => u,
        _ => {
            let state = ctx.state.lock_or_poisoned();
            let portal_url = state.config.target_callsign.clone();
            drop(state);
            if portal_url.is_empty() {
                return Redirect::to("/connect").into_response();
            }
            return Html(ui::error_page("No URL provided")).into_response();
        }
    };

    handle_browse(&ctx, &url, None).await
}

#[derive(Deserialize)]
struct BrowsePostParams {
    url: Option<String>,
}

async fn browse_post_handler(
    Query(params): Query<BrowsePostParams>,
    Extension(ctx): Extension<Arc<AppContext>>,
    body: String,
) -> Response {
    let url = match params.url {
        Some(u) if !u.is_empty() => u,
        _ => return Redirect::to("/connect").into_response(),
    };

    handle_browse(&ctx, &url, Some(body.into_bytes())).await
}

async fn handle_browse(
    ctx: &AppContext,
    url: &str,
    post_body: Option<Vec<u8>>,
) -> Response {
    {
        let state = ctx.state.lock_or_poisoned();
        if state.connection_state != ConnectionState::Connected {
            return Redirect::to("/connect").into_response();
        }
    }

    let request = match post_body {
        Some(body) => Request::Post {
            url: url.to_string(),
            body,
        },
        None => Request::Get {
            url: url.to_string(),
        },
    };

    let encoded = request.encode();

    match ctx.agwpe.send_request(encoded).await {
        Ok(response_data) => {
            if response_data.len() < 5 {
                return Html(ui::error_page("Invalid response from server")).into_response();
            }

            match ProtocolResponse::decode_header(&response_data) {
                Ok((status, payload_len)) => {
                    let payload = &response_data[5..];
                    if payload.len() < payload_len as usize {
                        return Html(ui::error_page("Incomplete response")).into_response();
                    }

                    match brotli_decompress(&payload[..payload_len as usize]) {
                        Ok(decompressed) => {
                            match String::from_utf8(decompressed) {
                                Ok(html) => {
                                    match status {
                                        Status::Ok => {
                                            match rewrite_html(&html, url) {
                                                Ok(rewritten) => {
                                                    Html(ui::browse_page(&rewritten, url)).into_response()
                                                }
                                                Err(e) => {
                                                    Html(ui::error_page(&format!("Failed to rewrite HTML: {}", e)))
                                                        .into_response()
                                                }
                                            }
                                        }
                                        Status::Err => {
                                            Html(ui::error_page(&format!("Server error: {}", html))).into_response()
                                        }
                                        Status::Blocked => {
                                            Html(ui::error_page(&format!("URL blocked: {}", html))).into_response()
                                        }
                                    }
                                }
                                Err(_) => Html(ui::error_page("Invalid UTF-8 in response")).into_response(),
                            }
                        }
                        Err(e) => Html(ui::error_page(&format!("Decompression failed: {}", e))).into_response(),
                    }
                }
                Err(e) => Html(ui::error_page(&format!("Invalid response header: {}", e))).into_response(),
            }
        }
        Err(e) => Html(ui::error_page(&format!("Request failed: {}", e))).into_response(),
    }
}

#[derive(Serialize)]
struct AgwpeStatusResponse {
    ok: bool,
    state: String,
    ports: Option<Vec<PortInfoJson>>,
    error: Option<String>,
}

#[derive(Serialize, Clone)]
struct PortInfoJson {
    port_num: u8,
    description: String,
}

async fn api_agwpe_status_get(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<AgwpeStatusResponse> {
    let state = ctx.state.lock_or_poisoned();
    let ports = state
        .available_ports
        .iter()
        .map(|p| PortInfoJson {
            port_num: p.port_num,
            description: p.description.clone(),
        })
        .collect();
    let state_str = state.connection_state.to_string();
    drop(state);

    Json(AgwpeStatusResponse {
        ok: true,
        state: state_str,
        ports: Some(ports),
        error: None,
    })
}

async fn api_agwpe_status_post(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<AgwpeStatusResponse> {
    let (host, port, callsign) = {
        let state = ctx.state.lock_or_poisoned();
        (
            state.config.agwpe_host.clone(),
            state.config.agwpe_port,
            state.config.my_callsign.clone(),
        )
    };

    if callsign.is_empty() {
        return Json(AgwpeStatusResponse {
            ok: false,
            state: "Error".to_string(),
            ports: None,
            error: Some("My callsign not configured".to_string()),
        });
    }

    match ctx
        .agwpe
        .connect_to_agwpe(host, port, callsign)
        .await
    {
        Ok(()) => {
            if let Err(e) = ctx.agwpe.query_ports().await {
                return Json(AgwpeStatusResponse {
                    ok: false,
                    state: "Error".to_string(),
                    ports: None,
                    error: Some(format!("Connected but port query failed: {}", e)),
                });
            }

            let state = ctx.state.lock_or_poisoned();
            let ports = state
                .available_ports
                .iter()
                .map(|p| PortInfoJson {
                    port_num: p.port_num,
                    description: p.description.clone(),
                })
                .collect();
            let state_str = state.connection_state.to_string();
            drop(state);

            Json(AgwpeStatusResponse {
                ok: true,
                state: state_str,
                ports: Some(ports),
                error: None,
            })
        }
        Err(e) => Json(AgwpeStatusResponse {
            ok: false,
            state: "Error".to_string(),
            ports: None,
            error: Some(e.to_string()),
        }),
    }
}

#[derive(Deserialize)]
struct ConnectRequest {
    target_callsign: String,
    port_num: u8,
}

#[derive(Serialize)]
struct ConnectResponse {
    ok: bool,
    state: Option<String>,
    error: Option<String>,
}

async fn api_connect_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
    Json(req): Json<ConnectRequest>,
) -> Json<ConnectResponse> {
    // Validate target callsign format
    let callsign = req.target_callsign.split('-').next().unwrap_or(&req.target_callsign);
    if !CALLSIGN_REGEX.is_match(callsign) {
        return Json(ConnectResponse {
            ok: false,
            state: None,
            error: Some("Invalid target callsign format".to_string()),
        });
    }

    match ctx
        .agwpe
        .ax25_connect(req.target_callsign, req.port_num)
        .await
    {
        Ok(()) => {
            let state = ctx.state.lock_or_poisoned();
            let state_str = state.connection_state.to_string();
            drop(state);

            Json(ConnectResponse {
                ok: true,
                state: Some(state_str),
                error: None,
            })
        }
        Err(e) => Json(ConnectResponse {
            ok: false,
            state: None,
            error: Some(e.to_string()),
        }),
    }
}

async fn api_disconnect_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<ConnectResponse> {
    match ctx.agwpe.ax25_disconnect().await {
        Ok(()) => Json(ConnectResponse {
            ok: true,
            state: Some("Disconnected".to_string()),
            error: None,
        }),
        Err(e) => Json(ConnectResponse {
            ok: false,
            state: None,
            error: Some(e.to_string()),
        }),
    }
}

#[derive(Serialize)]
struct ConfigResponse {
    agwpe_host: String,
    agwpe_port: u16,
    my_callsign: String,
    target_callsign: String,
    bpq_command: String,
    skip_bpq_app: bool,
}

async fn api_config_get(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<ConfigResponse> {
    let state = ctx.state.lock_or_poisoned();
    Json(ConfigResponse {
        agwpe_host: state.config.agwpe_host.clone(),
        agwpe_port: state.config.agwpe_port,
        my_callsign: state.config.my_callsign.clone(),
        target_callsign: state.config.target_callsign.clone(),
        bpq_command: state.config.bpq_command.clone(),
        skip_bpq_app: state.config.skip_bpq_app,
    })
}

#[derive(Deserialize)]
struct ConfigUpdate {
    agwpe_host: Option<String>,
    agwpe_port: Option<u16>,
    my_callsign: Option<String>,
    target_callsign: Option<String>,
    bpq_command: Option<String>,
    skip_bpq_app: Option<bool>,
}

#[derive(Serialize)]
struct ConfigSaveResponse {
    ok: bool,
    error: Option<String>,
}

async fn api_config_post(
    Extension(ctx): Extension<Arc<AppContext>>,
    Json(update): Json<ConfigUpdate>,
) -> Json<ConfigSaveResponse> {
    let path = match FileConfig::default_path() {
        Ok(p) => p,
        Err(e) => {
            return Json(ConfigSaveResponse {
                ok: false,
                error: Some(format!("Failed to get config path: {}", e)),
            });
        }
    };

    let mut config = {
        let state = ctx.state.lock_or_poisoned();
        state.config.clone()
    };

    if let Some(host) = update.agwpe_host {
        config.agwpe_host = host;
    }
    if let Some(port) = update.agwpe_port {
        config.agwpe_port = port;
    }
    if let Some(callsign) = update.my_callsign {
        config.my_callsign = callsign;
    }
    if let Some(target) = update.target_callsign {
        config.target_callsign = target;
    }
    if let Some(cmd) = update.bpq_command {
        config.bpq_command = cmd;
    }
    if let Some(skip) = update.skip_bpq_app {
        config.skip_bpq_app = skip;
    }

    match config.save(&path) {
        Ok(()) => {
            {
                let mut state = ctx.state.lock_or_poisoned();
                state.config = config;
            }
            Json(ConfigSaveResponse {
                ok: true,
                error: None,
            })
        }
        Err(e) => Json(ConfigSaveResponse {
            ok: false,
            error: Some(format!("Failed to save config: {}", e)),
        }),
    }
}

async fn events_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let existing_entries = {
        let state = ctx.state.lock_or_poisoned();
        state.get_logs(None)
    };

    let mut rx = ctx.log_tx.subscribe();

    let stream = async_stream::stream! {
        for entry in existing_entries {
            if let Ok(json) = serde_json::to_string(&entry) {
                yield Ok(Event::default().data(json));
            }
        }

        loop {
            match rx.recv().await {
                Ok(entry) => {
                    if let Ok(json) = serde_json::to_string(&entry) {
                        yield Ok(Event::default().data(json));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("SSE client lagged, missed {} entries", n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Sse::new(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn loopback_allowlist() -> HostAllowlist {
        HostAllowlist::new(IpAddr::V4(Ipv4Addr::LOCALHOST), vec![])
    }

    fn unspecified_allowlist() -> HostAllowlist {
        HostAllowlist::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), vec![])
    }

    fn specific_lan_allowlist() -> HostAllowlist {
        HostAllowlist::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)), vec![])
    }

    #[test]
    fn loopback_hosts_always_allowed() {
        for wl in [
            loopback_allowlist(),
            unspecified_allowlist(),
            specific_lan_allowlist(),
        ] {
            assert!(wl.contains_host_header("127.0.0.1"));
            assert!(wl.contains_host_header("127.0.0.1:8080"));
            assert!(wl.contains_host_header("[::1]:8080"));
            assert!(wl.contains_host_header("localhost"));
            assert!(wl.contains_host_header("localhost:8080"));
            assert!(wl.contains_host_header("LocalHost:9"));
        }
    }

    #[test]
    fn arbitrary_hostnames_rejected_by_default() {
        let wl = loopback_allowlist();
        assert!(!wl.contains_host_header("evil.com"));
        assert!(!wl.contains_host_header("evil.com:8080"));
        assert!(!wl.contains_host_header("attacker.example.com"));
    }

    #[test]
    fn extra_hostnames_added_via_constructor() {
        let wl = HostAllowlist::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            vec!["raspberrypi.local".to_string(), "Radio.LAN".to_string()],
        );
        assert!(wl.contains_host_header("raspberrypi.local:8080"));
        // Case-insensitive.
        assert!(wl.contains_host_header("RASPBERRYPI.LOCAL:8080"));
        assert!(wl.contains_host_header("radio.lan"));
        // Still rejects other names.
        assert!(!wl.contains_host_header("evil.com:8080"));
    }

    #[test]
    fn loopback_bind_rejects_lan_ip_hosts() {
        let wl = loopback_allowlist();
        assert!(!wl.contains_host_header("192.168.1.10:8080"));
        assert!(!wl.contains_host_header("10.0.0.5"));
        assert!(!wl.contains_host_header("[fc00::1]:8080"));
    }

    #[test]
    fn unspecified_bind_allows_lan_ip_hosts() {
        let wl = unspecified_allowlist();
        assert!(wl.contains_host_header("192.168.1.10:8080"));
        assert!(wl.contains_host_header("10.0.0.5"));
        assert!(wl.contains_host_header("172.16.0.1:80"));
        assert!(wl.contains_host_header("169.254.10.5"));
        assert!(wl.contains_host_header("[fc00::1]:8080"));
        assert!(wl.contains_host_header("[fe80::1]:8080"));
    }

    #[test]
    fn unspecified_bind_still_rejects_public_ip_hosts() {
        let wl = unspecified_allowlist();
        // 8.8.8.8 in a Host header is not something we should serve as ourselves.
        assert!(!wl.contains_host_header("8.8.8.8:8080"));
        assert!(!wl.contains_host_header("[2606:4700::1]:8080"));
    }

    #[test]
    fn specific_bind_accepts_only_that_ip() {
        let wl = specific_lan_allowlist();
        assert!(wl.contains_host_header("192.168.1.10:8080"));
        // Different LAN IP -- the operator didn't bind here.
        assert!(!wl.contains_host_header("192.168.1.11:8080"));
        // Loopback still allowed for the operator on the box.
        assert!(wl.contains_host_header("127.0.0.1:8080"));
    }

    #[test]
    fn dns_rebinding_case_rejected() {
        // Attacker page whose DNS flips to our IP would send Host: evil.com
        // regardless of what our listen_addr is.
        for wl in [
            loopback_allowlist(),
            unspecified_allowlist(),
            specific_lan_allowlist(),
        ] {
            assert!(!wl.contains_host_header("evil.com:8080"));
            assert!(!wl.contains_host_header("attacker.local"));
        }
    }

    #[test]
    fn origin_authority_matches_host() {
        assert!(origin_matches_host("http://127.0.0.1:8080", "127.0.0.1:8080"));
        assert!(origin_matches_host("https://raspberrypi.local", "raspberrypi.local"));
        assert!(!origin_matches_host("http://evil.com", "127.0.0.1:8080"));
        // Missing scheme -> not a valid Origin header.
        assert!(!origin_matches_host("127.0.0.1:8080", "127.0.0.1:8080"));
    }
}
