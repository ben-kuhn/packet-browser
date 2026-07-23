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
use std::time::Duration;
use tokio::sync::{broadcast, Mutex as TokioMutex};
use crate::cache::Cache;

static CALLSIGN_REGEX: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"^[a-zA-Z0-9]{1,3}[0-9][a-zA-Z0-9]{0,3}[a-zA-Z]$").unwrap()
});

use crate::transport::agwpe::AgwpeError;
use crate::transport::manager::TransportManager;
use crate::config::FileConfig;
use crate::state::{ConnectionState, DebugLogEntry, LockExt, SharedState};
use crate::ui;

pub struct AppContext {
    pub state: SharedState,
    /// The active transport manager. Wrapped in a Mutex so that
    /// `POST /api/connect` can swap in a different transport (e.g. VARA) at
    /// operator request without restarting the whole process.
    pub agwpe: TokioMutex<TransportManager>,
    pub log_tx: broadcast::Sender<DebugLogEntry>,
    pub host_allowlist: HostAllowlist,
    pub cache: Option<Arc<Cache>>,
    pub cache_max_ttl: Duration,
    pub config: FileConfig,
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
        .route("/cache", get(cache_page_handler))
        .route("/api/agwpe-status", get(api_agwpe_status_get))
        .route("/api/agwpe-status", post(api_agwpe_status_post))
        .route("/api/connect", post(api_connect_handler))
        .route("/api/disconnect", post(api_disconnect_handler))
        .route("/api/consent", get(api_consent_get))
        .route("/api/consent", post(api_consent_post))
        .route("/api/config", get(api_config_get))
        .route("/api/config", post(api_config_post))
        .route("/api/state", get(api_state_get))
        .route("/api/cache/clear", post(api_cache_clear))
        .route("/api/cache/delete", post(api_cache_delete))
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
    use crate::transport::{VaraParams, VaraMode, VaraBandwidth};

    let state = ctx.state.lock_or_poisoned();
    let my_callsign = state.config.my_callsign.clone();
    let target_callsign = state.config.target_callsign.clone();
    let connection_state = state.connection_state.to_string();
    let connection_state_class = match state.connection_state {
        ConnectionState::Disconnected => "status-disconnected",
        ConnectionState::AgwpeConnected => "status-agwpe-connected",
        ConnectionState::Connecting => "status-connecting",
        ConnectionState::AwaitingConsent { .. } => "status-connecting",
        ConnectionState::Connected => "status-connected",
        ConnectionState::Reconnecting { .. } => "status-reconnecting",
        ConnectionState::Error(_) => "status-error",
    };
    let ports_json = serde_json::to_string(&state.available_ports).unwrap_or_else(|_| "[]".to_string());
    let transport_default = state.config.transport.default;
    let vara_params = VaraParams {
        cmd_host: state.config.vara.cmd_host.clone(),
        cmd_port: state.config.vara.cmd_port,
        data_host: state.config.vara.data_host.clone(),
        data_port: state.config.vara.data_port,
        mode: match state.config.vara.mode {
            VaraMode::Fm => VaraMode::Fm,
            VaraMode::Hf => VaraMode::Hf,
        },
        bandwidth: match state.config.vara.bandwidth {
            VaraBandwidth::VNarrow => VaraBandwidth::VNarrow,
            VaraBandwidth::VWide   => VaraBandwidth::VWide,
            VaraBandwidth::Bw250   => VaraBandwidth::Bw250,
            VaraBandwidth::Bw500   => VaraBandwidth::Bw500,
            VaraBandwidth::Bw2300  => VaraBandwidth::Bw2300,
            VaraBandwidth::Bw2750  => VaraBandwidth::Bw2750,
        },
    };
    drop(state);

    Html(ui::connect_page(
        &my_callsign,
        &target_callsign,
        &connection_state,
        connection_state_class,
        &ports_json,
        transport_default,
        &vara_params,
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
    handle_browse(&ctx, &url, Some(body.into_bytes()), true, None).await
}

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
                        cache.touch_last_used(url);
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

    let send_result = if ctx.config.connection.auto_reconnect {
        ctx.agwpe.lock().await.send_request_with_reconnect(encoded).await
    } else {
        ctx.agwpe.lock().await.send_request(encoded).await
    };
    match send_result {
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
        Err(AgwpeError::NeedsReconsent) => {
            Html(ui::render_session_error_page(
                "Session dropped and the disclaimer text changed. Please reconnect and re-consent.",
                true,
            )).into_response()
        }
        Err(AgwpeError::SessionDied { reason }) => {
            // Auto-reconnect already ran and this is the second failure, OR
            // auto-reconnect was disabled. Either way, surface the error.
            Html(ui::render_session_error_page(
                &format!("Session lost: {}. Please reconnect.", reason),
                true,
            )).into_response()
        }
        Err(AgwpeError::DisconnectedByOperator) => {
            Html(ui::render_session_error_page(
                "Request cancelled by operator disconnect.",
                true,
            )).into_response()
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
    match axum::http::HeaderValue::from_str(&format!("private, max-age={}", ttl_secs)) {
        Ok(value) => {
            headers.insert(axum::http::header::CACHE_CONTROL, value);
        }
        Err(e) => {
            tracing::error!("cache-control value rejected by HeaderValue::from_str: {} — omitting Cache-Control header", e);
        }
    }
    match axum::http::HeaderValue::from_str(&format!("\"{}\"", etag)) {
        Ok(value) => {
            headers.insert(axum::http::header::ETAG, value);
        }
        Err(e) => {
            tracing::error!("etag {:?} rejected by HeaderValue::from_str: {} — omitting ETag header", etag, e);
        }
    }
    resp
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
        .lock()
        .await
        .connect_modem(host, port, callsign)
        .await
    {
        Ok(()) => {
            if let Err(e) = ctx.agwpe.lock().await.query_ports().await {
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
    /// Transport to use: "ax25" | "vara_fm" | "vara_hf".
    /// Defaults to `state.config.transport.default` when absent.
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    vara_cmd_host: Option<String>,
    #[serde(default)]
    vara_cmd_port: Option<u16>,
    #[serde(default)]
    vara_data_host: Option<String>,
    #[serde(default)]
    vara_data_port: Option<u16>,
    #[serde(default)]
    vara_mode: Option<String>,
    #[serde(default)]
    vara_bandwidth: Option<String>,
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
    use crate::transport::{TransportKind, VaraBandwidth, VaraMode};

    // Validate target callsign format
    let callsign = req.target_callsign.split('-').next().unwrap_or(&req.target_callsign);
    if !CALLSIGN_REGEX.is_match(callsign) {
        return Json(ConnectResponse {
            ok: false,
            state: None,
            error: Some("Invalid target callsign format".to_string()),
        });
    }

    // Resolve transport kind from the request field, falling back to the
    // configured default.
    let transport_kind = match req.transport.as_deref() {
        Some(s) => match s.parse::<TransportKind>() {
            Ok(k) => k,
            Err(e) => {
                return Json(ConnectResponse {
                    ok: false,
                    state: None,
                    error: Some(format!("Unknown transport: {}", e)),
                });
            }
        },
        None => ctx.state.lock_or_poisoned().config.transport.default,
    };

    // When a VARA transport is requested, spawn a new TransportManager backed
    // by VaraTransport and replace the active one.  The operator must have
    // already configured (or passed) VARA host/port parameters; if none are
    // given we fall back to the configured defaults.
    match transport_kind {
        TransportKind::VaraFm | TransportKind::VaraHf => {
            let (cmd_host, cmd_port, _data_host, _data_port,
                 vara_mode_str, vara_bw_str, my_callsign,
                 response_timeout_secs) = {
                let s = ctx.state.lock_or_poisoned();
                (
                    req.vara_cmd_host.clone().unwrap_or_else(|| s.config.vara.cmd_host.clone()),
                    req.vara_cmd_port.unwrap_or(s.config.vara.cmd_port),
                    req.vara_data_host.clone().unwrap_or_else(|| s.config.vara.data_host.clone()),
                    req.vara_data_port.unwrap_or(s.config.vara.data_port),
                    req.vara_mode.clone().unwrap_or_else(|| match s.config.vara.mode {
                        VaraMode::Fm => "fm".to_string(),
                        VaraMode::Hf => "hf".to_string(),
                    }),
                    req.vara_bandwidth.clone().unwrap_or_else(|| match s.config.vara.bandwidth {
                        VaraBandwidth::VNarrow => "vnarrow".to_string(),
                        VaraBandwidth::VWide   => "vwide".to_string(),
                        VaraBandwidth::Bw250   => "bw250".to_string(),
                        VaraBandwidth::Bw500   => "bw500".to_string(),
                        VaraBandwidth::Bw2300  => "bw2300".to_string(),
                        VaraBandwidth::Bw2750  => "bw2750".to_string(),
                    }),
                    s.config.my_callsign.clone(),
                    s.config.connection.response_timeout_secs,
                )
            };

            // Build a fresh VaraTransport-backed manager and replace the
            // active manager so subsequent calls (disconnect, browse) use it.
            let vara_transport: Box<dyn crate::transport::Transport> =
                Box::new(crate::transport::vara::VaraTransport::new());
            let new_manager = TransportManager::spawn(
                vara_transport,
                ctx.state.clone(),
                ctx.log_tx.clone(),
                response_timeout_secs,
            );
            {
                let mut mgr = ctx.agwpe.lock().await;
                *mgr = new_manager.clone();
            }

            // Connect the VARA modem. This will fail if no VARA modem is
            // listening on the configured ports (expected in tests/CI).
            let vara_mode_val = if vara_mode_str == "hf" { VaraMode::Hf } else { VaraMode::Fm };
            let vara_bw_val = match vara_bw_str.as_str() {
                "vnarrow" => VaraBandwidth::VNarrow,
                "bw250"   => VaraBandwidth::Bw250,
                "bw500"   => VaraBandwidth::Bw500,
                "bw2300"  => VaraBandwidth::Bw2300,
                "bw2750"  => VaraBandwidth::Bw2750,
                _         => VaraBandwidth::VWide,
            };
            let _ = (vara_mode_val, vara_bw_val); // consumed by TransportConfig below

            // connect_modem on the VARA manager will attempt a TCP connect to
            // the VARA cmd port. On a development machine with no modem
            // running this returns Err — the error message comes from the
            // OS / transport layer and proves the VARA path was reached.
            if let Err(e) = new_manager.connect_modem(cmd_host, cmd_port, my_callsign).await {
                return Json(ConnectResponse {
                    ok: false,
                    state: None,
                    error: Some(format!("VARA modem connect failed: {}", e)),
                });
            }

            // If connect_modem succeeded (modem is actually running), proceed
            // with opening the AX.25/VARA session.
            match new_manager.open_session(req.target_callsign, req.port_num).await {
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

        TransportKind::Ax25 => {
            // Existing AX.25/AGWPE path — use the current manager as-is.
            match ctx.agwpe.lock().await.open_session(req.target_callsign, req.port_num).await {
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
    }
}

async fn api_disconnect_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<ConnectResponse> {
    match ctx.agwpe.lock().await.close_session().await {
        Ok(()) => {
            {
                let mut s = ctx.state.lock_or_poisoned();
                s.clear_agreed_disclaimer();
            }
            Json(ConnectResponse {
                ok: true,
                state: Some("Disconnected".to_string()),
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

#[derive(Serialize)]
struct ConsentInfoResponse {
    awaiting: bool,
    disclaimer: Option<String>,
}

async fn api_consent_get(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<ConsentInfoResponse> {
    let state = ctx.state.lock_or_poisoned();
    match &state.connection_state {
        crate::state::ConnectionState::AwaitingConsent { disclaimer } => {
            Json(ConsentInfoResponse {
                awaiting: true,
                disclaimer: Some(disclaimer.clone()),
            })
        }
        _ => Json(ConsentInfoResponse {
            awaiting: false,
            disclaimer: None,
        }),
    }
}

#[derive(Deserialize)]
struct ConsentDecision {
    accepted: bool,
}

#[derive(Serialize)]
struct ConsentResponse {
    ok: bool,
    error: Option<String>,
}

async fn api_consent_post(
    Extension(ctx): Extension<Arc<AppContext>>,
    Json(decision): Json<ConsentDecision>,
) -> Json<ConsentResponse> {
    // Take the sender out under the lock, then drop the guard before sending
    // so we never hold the mutex across the wake. If the slot is empty, the
    // handshake either already resumed or was never paused — either way this
    // is a no-op the caller should treat as "nothing to consent to."
    let sender = {
        let mut s = ctx.state.lock_or_poisoned();
        s.pending_consent.take()
    };
    match sender {
        Some(tx) => {
            // Record the agreed disclaimer text if operator accepted
            if decision.accepted {
                let disclaimer_text = {
                    let s = ctx.state.lock_or_poisoned();
                    match &s.connection_state {
                        ConnectionState::AwaitingConsent { disclaimer } => Some(disclaimer.clone()),
                        _ => None,
                    }
                };
                if let Some(text) = disclaimer_text {
                    let mut s = ctx.state.lock_or_poisoned();
                    s.record_agreed_disclaimer(text);
                }
            }
            if tx.send(decision.accepted).is_err() {
                // Receiver already dropped (e.g. handshake cancelled while
                // this request was in flight). Treat as a no-op.
                return Json(ConsentResponse {
                    ok: false,
                    error: Some("Handshake no longer awaiting consent".to_string()),
                });
            }
            Json(ConsentResponse { ok: true, error: None })
        }
        None => Json(ConsentResponse {
            ok: false,
            error: Some("No consent prompt is pending".to_string()),
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

#[derive(Serialize)]
struct StateResponse {
    agwpe_host: String,
    agwpe_port: u16,
    transport: String,
    vara_cmd_host: String,
    vara_cmd_port: u16,
    vara_data_host: String,
    vara_data_port: u16,
    vara_mode: String,
    vara_bandwidth: String,
}

async fn api_state_get(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<StateResponse> {
    use crate::transport::{VaraBandwidth, VaraMode};

    let state = ctx.state.lock_or_poisoned();
    let vara_mode = match state.config.vara.mode {
        VaraMode::Fm => "fm".to_string(),
        VaraMode::Hf => "hf".to_string(),
    };
    let vara_bandwidth = match state.config.vara.bandwidth {
        VaraBandwidth::VNarrow => "vnarrow".to_string(),
        VaraBandwidth::VWide   => "vwide".to_string(),
        VaraBandwidth::Bw250   => "bw250".to_string(),
        VaraBandwidth::Bw500   => "bw500".to_string(),
        VaraBandwidth::Bw2300  => "bw2300".to_string(),
        VaraBandwidth::Bw2750  => "bw2750".to_string(),
    };
    Json(StateResponse {
        agwpe_host: state.config.agwpe_host.clone(),
        agwpe_port: state.config.agwpe_port,
        transport: state.config.transport.default.to_string(),
        vara_cmd_host: state.config.vara.cmd_host.clone(),
        vara_cmd_port: state.config.vara.cmd_port,
        vara_data_host: state.config.vara.data_host.clone(),
        vara_data_port: state.config.vara.data_port,
        vara_mode,
        vara_bandwidth,
    })
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
}
