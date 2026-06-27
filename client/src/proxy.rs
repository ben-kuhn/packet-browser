use axum::{
    extract::{Extension, Query},
    response::{
        sse::{Event, Sse},
        Html, IntoResponse, Redirect, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::agwpe::AgwpeManager;
use crate::config::FileConfig;
use crate::rewrite::rewrite_html;
use crate::state::{ConnectionState, DebugLogEntry, SharedState};
use crate::ui;
use packet_browser_shared::compress::brotli_decompress;
use packet_browser_shared::protocol::{Request, Response as ProtocolResponse, Status};

pub struct AppContext {
    pub state: SharedState,
    pub agwpe: AgwpeManager,
    pub log_tx: broadcast::Sender<DebugLogEntry>,
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
        .layer(Extension(ctx))
}

async fn root_handler() -> impl IntoResponse {
    Redirect::to("/connect")
}

async fn connect_page_handler(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> impl IntoResponse {
    let state = ctx.state.lock().unwrap();
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
    let state = ctx.state.lock().unwrap();
    let agwpe_host = state.config.agwpe_host.clone();
    let agwpe_port = state.config.agwpe_port;
    drop(state);

    Html(ui::configuration_page(&agwpe_host, agwpe_port))
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
            let state = ctx.state.lock().unwrap();
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
        let state = ctx.state.lock().unwrap();
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
    let state = ctx.state.lock().unwrap();
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
        let state = ctx.state.lock().unwrap();
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

            let state = ctx.state.lock().unwrap();
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
    match ctx
        .agwpe
        .ax25_connect(req.target_callsign, req.port_num)
        .await
    {
        Ok(()) => {
            let state = ctx.state.lock().unwrap();
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
}

async fn api_config_get(
    Extension(ctx): Extension<Arc<AppContext>>,
) -> Json<ConfigResponse> {
    let state = ctx.state.lock().unwrap();
    Json(ConfigResponse {
        agwpe_host: state.config.agwpe_host.clone(),
        agwpe_port: state.config.agwpe_port,
        my_callsign: state.config.my_callsign.clone(),
        target_callsign: state.config.target_callsign.clone(),
        bpq_command: state.config.bpq_command.clone(),
    })
}

#[derive(Deserialize)]
struct ConfigUpdate {
    agwpe_host: Option<String>,
    agwpe_port: Option<u16>,
    my_callsign: Option<String>,
    bpq_command: Option<String>,
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
        let state = ctx.state.lock().unwrap();
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
    if let Some(cmd) = update.bpq_command {
        config.bpq_command = cmd;
    }

    match config.save(&path) {
        Ok(()) => {
            {
                let mut state = ctx.state.lock().unwrap();
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
        let state = ctx.state.lock().unwrap();
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
