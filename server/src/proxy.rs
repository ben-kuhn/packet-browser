//! In-process HTTP + HTTPS forward proxy.
//!
//! Firefox is directed to route all its traffic through this proxy via the
//! `network.proxy.*` prefs set in `browser.rs`. That way every request the
//! browser issues — top-level navigation, stylesheets, fonts, images, inline
//! `fetch()` from `JS_SCRUB_HTML`, anything — passes through
//! [`filter::resolve_and_pin`] before touching the network, and the resolved
//! IP is used for both the block check and the outbound connection. This
//! closes both the subresource-SSRF gap (previously only the top-level nav
//! URL was filtered) and the DNS-rebinding TOCTOU (previously the filter and
//! Chromium each did their own lookup).
//!
//! For plain HTTP we parse the request, run the URL through the filter,
//! connect to the pinned IP, forward the request, and stream the response
//! back.
//!
//! For HTTPS the browser sends `CONNECT example.com:443`. We validate the
//! target hostname, resolve DNS once, connect to that pinned IP:port, and
//! bidirectionally splice the two TCP sockets. We cannot inspect the
//! encrypted payload but we control *where the connection lands*, which is
//! what the SSRF policy cares about.

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Incoming;
use hyper::client::conn::http1 as client_http1;
use hyper::server::conn::http1 as server_http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::TokioIo;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};

use crate::filter::{resolve_and_pin, UrlError};

const OUTBOUND_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Start the filtering proxy on 127.0.0.1 and return its bound port so the
/// browser can be pointed at it. Runs forever in a Tokio task.
pub fn start_proxy(blocked_ranges: Vec<String>) -> std::io::Result<u16> {
    // The server binary is otherwise sync; spin a dedicated runtime for the
    // proxy so it doesn't fight with per-BrowserInstance runtimes.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("proxy")
        .build()?;

    let listener = runtime.block_on(async {
        TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await
    })?;
    let port = listener.local_addr()?.port();
    let blocked = Arc::new(blocked_ranges);

    // Move the runtime onto its own thread so it lives for the process
    // lifetime without borrowing the main thread.
    std::thread::Builder::new()
        .name("proxy-rt".into())
        .spawn(move || {
            runtime.block_on(async move {
                loop {
                    let (stream, peer) = match listener.accept().await {
                        Ok(x) => x,
                        Err(e) => {
                            eprintln!("[PROXY] accept error: {e}");
                            continue;
                        }
                    };
                    let blocked = Arc::clone(&blocked);
                    tokio::spawn(async move {
                        if let Err(e) = serve_conn(stream, blocked).await {
                            tracing::debug!(peer=?peer, "[PROXY] connection error: {e:?}");
                        }
                    });
                }
            });
        })?;

    Ok(port)
}

async fn serve_conn(
    stream: TcpStream,
    blocked: Arc<Vec<String>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let io = TokioIo::new(stream);
    server_http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(
            io,
            service_fn(move |req| handle(req, Arc::clone(&blocked))),
        )
        .with_upgrades()
        .await?;
    Ok(())
}

async fn handle(
    mut req: Request<Incoming>,
    blocked: Arc<Vec<String>>,
) -> Result<Response<BoxBody>, Infallible> {
    match req.method() {
        &Method::CONNECT => match handle_connect(&mut req, &blocked) {
            Ok((host, port, pinned_ip)) => {
                // Take ownership of the upgrade future *before* we return the
                // 200 response, then spawn the tunnel once the upgrade lands.
                let upgrade = hyper::upgrade::on(&mut req);
                tokio::spawn(async move {
                    match upgrade.await {
                        Ok(upgraded) => {
                            if let Err(e) =
                                tunnel(upgraded, pinned_ip, port).await
                            {
                                tracing::debug!(
                                    "[PROXY] CONNECT tunnel to {host} ({pinned_ip}:{port}) closed: {e}"
                                );
                            }
                        }
                        Err(e) => {
                            eprintln!("[PROXY] CONNECT upgrade failed: {e}");
                        }
                    }
                });
                Ok(empty_response(StatusCode::OK))
            }
            Err(url_err) => Ok(error_response(&url_err)),
        },
        _ => match forward_http(req, &blocked).await {
            Ok(resp) => Ok(resp),
            Err(ForwardError::Blocked(e)) => Ok(error_response(&e)),
            Err(ForwardError::BadRequest) => Ok(empty_response(StatusCode::BAD_REQUEST)),
            Err(ForwardError::Upstream(msg)) => {
                tracing::warn!("[PROXY] upstream error: {msg}");
                Ok(empty_response(StatusCode::BAD_GATEWAY))
            }
        },
    }
}

fn handle_connect(
    req: &Request<Incoming>,
    blocked: &Arc<Vec<String>>,
) -> Result<(String, u16, IpAddr), UrlError> {
    // CONNECT target is authority-form: "example.com:443".
    let auth = req
        .uri()
        .authority()
        .ok_or(UrlError::InvalidUrl)?
        .as_str()
        .to_string();
    let (host, port_str) = auth.rsplit_once(':').ok_or(UrlError::InvalidUrl)?;
    let port: u16 = port_str.parse().map_err(|_| UrlError::InvalidUrl)?;

    // Only CONNECT to :443 (HTTPS) or :80 (rare HTTP-over-CONNECT). Anything
    // else is either an SSRF probe or a misconfiguration.
    if port != 443 && port != 80 {
        return Err(UrlError::BlockedProtocol(format!("CONNECT :{port}")));
    }

    let host_clean = host.trim_start_matches('[').trim_end_matches(']');
    let pinned = resolve_and_pin(host_clean, port, blocked)?;
    Ok((host.to_string(), port, pinned))
}

async fn tunnel(
    upgraded: hyper::upgrade::Upgraded,
    ip: IpAddr,
    port: u16,
) -> std::io::Result<()> {
    let upstream = tokio::time::timeout(
        OUTBOUND_CONNECT_TIMEOUT,
        TcpStream::connect(SocketAddr::new(ip, port)),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))??;

    let mut upgraded = TokioIo::new(upgraded);
    let mut upstream = upstream;
    tokio::io::copy_bidirectional(&mut upgraded, &mut upstream).await?;
    Ok(())
}

#[derive(Debug)]
enum ForwardError {
    Blocked(UrlError),
    BadRequest,
    Upstream(String),
}

async fn forward_http(
    req: Request<Incoming>,
    blocked: &Arc<Vec<String>>,
) -> Result<Response<BoxBody>, ForwardError> {
    let uri = req.uri().clone();
    // For a forward proxy the request-line carries an absolute URL. If the
    // browser sent a relative one, we can't proxy it (bad client behavior).
    if uri.scheme_str().is_none() {
        return Err(ForwardError::BadRequest);
    }
    // Reject anything that isn't HTTP -- HTTPS goes via CONNECT above.
    if uri.scheme_str() != Some("http") {
        return Err(ForwardError::Blocked(UrlError::BlockedProtocol(
            uri.scheme_str().unwrap_or("").to_string(),
        )));
    }

    let host = uri
        .host()
        .ok_or(ForwardError::BadRequest)?
        .to_string();
    let port = uri.port_u16().unwrap_or(80);

    let pinned = resolve_and_pin(&host, port, blocked).map_err(ForwardError::Blocked)?;

    let stream = tokio::time::timeout(
        OUTBOUND_CONNECT_TIMEOUT,
        TcpStream::connect(SocketAddr::new(pinned, port)),
    )
    .await
    .map_err(|_| ForwardError::Upstream("connect timeout".into()))?
    .map_err(|e| ForwardError::Upstream(e.to_string()))?;

    let (mut sender, conn) = client_http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| ForwardError::Upstream(e.to_string()))?;

    // Drive the client connection in the background.
    tokio::spawn(async move {
        if let Err(e) = conn.await {
            tracing::debug!("[PROXY] client conn closed: {e}");
        }
    });

    // Rebuild the outbound request with an origin-form URI (path+query only)
    // and a Host header. Hyper's client requires origin-form.
    let path_and_query = uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());

    let (mut parts, body) = req.into_parts();
    parts.uri = Uri::builder()
        .path_and_query(path_and_query)
        .build()
        .map_err(|e| ForwardError::Upstream(e.to_string()))?;

    // Ensure Host is set (some clients only send it via the absolute URI).
    let host_header = if port == 80 {
        host.clone()
    } else {
        format!("{host}:{port}")
    };
    parts.headers.insert(
        hyper::header::HOST,
        host_header
            .parse()
            .map_err(|e: hyper::header::InvalidHeaderValue| {
                ForwardError::Upstream(e.to_string())
            })?,
    );
    // Strip hop-by-hop headers.
    remove_hop_by_hop(&mut parts.headers);

    let outbound = Request::from_parts(parts, body);

    let resp = sender
        .send_request(outbound)
        .await
        .map_err(|e| ForwardError::Upstream(e.to_string()))?;

    let (parts, body) = resp.into_parts();
    let body: BoxBody = body
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed();
    Ok(Response::from_parts(parts, body))
}

fn remove_hop_by_hop(headers: &mut hyper::HeaderMap) {
    // RFC 7230 §6.1 — plus a few we don't want to pass through.
    for name in [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "te",
        "trailer",
        "upgrade",
        "proxy-authenticate",
        "proxy-authorization",
    ] {
        headers.remove(name);
    }
}

type BoxBody =
    http_body_util::combinators::BoxBody<Bytes, Box<dyn std::error::Error + Send + Sync>>;

fn empty_response(code: StatusCode) -> Response<BoxBody> {
    let body: BoxBody = Empty::<Bytes>::new()
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed();
    let mut r = Response::new(body);
    *r.status_mut() = code;
    r
}

fn error_response(err: &UrlError) -> Response<BoxBody> {
    let msg = format!("packet-browser proxy: {err}\n");
    let body: BoxBody = Full::new(Bytes::from(msg))
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })
        .boxed();
    let mut r = Response::new(body);
    *r.status_mut() = StatusCode::FORBIDDEN;
    r
}
