//! The proxy core: a tape recorder at the egress boundary.
//!
//! One server handles every way an agent might reach the outside world:
//!
//! * **CONNECT** (HTTPS via `HTTPS_PROXY`) → we MITM the TLS session with a leaf
//!   cert minted by the local CA, then read the plaintext HTTP inside.
//! * **absolute-form** request (HTTP via `HTTP_PROXY`) → forward to the host in
//!   the request URI.
//! * **origin-form** request (the agent's `base_url` points straight at us) →
//!   forward to the configured `--upstream`.
//!
//! In every case the same [`handle`] path runs: reconstruct the full request,
//! hand it to the active [`Engine`] (record or replay), and return the response.

pub mod record;
pub mod replay;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, warn};

use crate::ca::LocalCa;
use crate::config::Upstream;
use record::RecordEngine;
use replay::ReplayEngine;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type Resp = Response<BoxBody<Bytes, BoxError>>;

/// Any byte stream we can speak HTTP over (plain TCP or a TLS stream).
pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The active mode. Both variants share the same dispatch front end.
pub enum Engine {
    Record(Arc<RecordEngine>),
    Replay(Arc<ReplayEngine>),
}

/// Everything a connection task needs, shared behind an `Arc`.
pub struct ProxyState {
    pub engine: Engine,
    /// Present whenever TLS interception is possible (cloud presets / MITM).
    pub ca: Option<Arc<LocalCa>>,
    /// Default upstream for origin-form (reverse-proxy) requests.
    pub default_upstream: Option<Upstream>,
}

/// Where a single request should be sent.
#[derive(Debug, Clone)]
pub struct Target {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

impl Target {
    pub fn url_for(&self, path_and_query: &str) -> String {
        let default_port = if self.scheme == "https" { 443 } else { 80 };
        if self.port == default_port {
            format!("{}://{}{}", self.scheme, self.host, path_and_query)
        } else {
            format!(
                "{}://{}:{}{}",
                self.scheme, self.host, self.port, path_and_query
            )
        }
    }
}

/// Run the proxy until the process is asked to stop. `on_ready` is called once
/// the listener is bound, with the actual local address.
pub async fn serve(
    listen: SocketAddr,
    state: Arc<ProxyState>,
    on_ready: impl FnOnce(SocketAddr),
) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding proxy listener on {listen}"))?;
    let local = listener.local_addr()?;
    on_ready(local);

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                warn!("accept failed: {e}");
                continue;
            }
        };
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(stream, state).await {
                debug!("connection from {peer} ended: {e}");
            }
        });
    }
}

/// Serve one client connection. The outer service handles CONNECT (by upgrading
/// into a MITM TLS session) and plain forwarding.
async fn serve_connection(stream: TcpStream, state: Arc<ProxyState>) -> Result<()> {
    let io = TokioIo::new(stream);
    let service = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        async move { Ok::<_, std::convert::Infallible>(outer_dispatch(req, state).await) }
    });
    http1::Builder::new()
        .preserve_header_case(true)
        .title_case_headers(true)
        .serve_connection(io, service)
        .with_upgrades()
        .await
        .map_err(|e| anyhow::anyhow!("serving connection: {e}"))?;
    Ok(())
}

/// Public wrapper around the outer dispatch so other modules (e.g. the
/// `replaykit run` orchestrator that owns its own accept loop) can serve
/// connections without duplicating the request-routing layer.
pub async fn outer_dispatch_pub(req: Request<Incoming>, state: Arc<ProxyState>) -> Resp {
    outer_dispatch(req, state).await
}

/// Outer request dispatch (before any TLS interception).
async fn outer_dispatch(req: Request<Incoming>, state: Arc<ProxyState>) -> Resp {
    if req.method() == Method::CONNECT {
        return handle_connect(req, state);
    }
    // Absolute-form (forward proxy) or origin-form (reverse proxy).
    let target = match resolve_target(&req, None, &state) {
        Ok(t) => t,
        Err(msg) => return error_response(StatusCode::BAD_GATEWAY, &msg),
    };
    handle(req, target, state).await
}

/// Handle a CONNECT by replying 200 and upgrading the socket into a TLS session
/// we terminate with a minted leaf cert, then serving the inner HTTP.
fn handle_connect(req: Request<Incoming>, state: Arc<ProxyState>) -> Resp {
    let authority = match req.uri().authority().cloned() {
        Some(a) => a,
        None => return error_response(StatusCode::BAD_REQUEST, "CONNECT without authority"),
    };
    let host = authority.host().to_string();
    let port = authority.port_u16().unwrap_or(443);

    let ca = match &state.ca {
        Some(ca) => ca.clone(),
        None => {
            return error_response(
                StatusCode::NOT_IMPLEMENTED,
                "this proxy is running without a CA; HTTPS interception is disabled (use a local/HTTP preset or run `replaykit setup`)",
            )
        }
    };

    let server_cfg = match ca.server_config_for(&host) {
        Ok(cfg) => cfg,
        Err(e) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("minting cert for {host}: {e}"),
            )
        }
    };

    let state2 = state.clone();
    let host2 = host.clone();
    tokio::spawn(async move {
        match hyper::upgrade::on(req).await {
            Ok(upgraded) => {
                let io = TokioIo::new(upgraded);
                let acceptor = TlsAcceptor::from(server_cfg);
                match acceptor.accept(io).await {
                    Ok(tls) => {
                        if let Err(e) = serve_mitm(tls, host2.clone(), port, state2).await {
                            debug!("mitm session for {host2} ended: {e}");
                        }
                    }
                    Err(e) => debug!("tls accept for {host2} failed: {e}"),
                }
            }
            Err(e) => debug!("upgrade failed: {e}"),
        }
    });

    // 200 with an empty body tells the client the tunnel is established.
    Response::new(empty())
}

/// Serve plaintext HTTP requests inside an intercepted TLS session. Every
/// request here is origin-form and targets the CONNECT host over HTTPS.
async fn serve_mitm<S>(tls: S, host: String, port: u16, state: Arc<ProxyState>) -> Result<()>
where
    S: Io + 'static,
{
    let io = TokioIo::new(tls);
    let service = service_fn(move |req: Request<Incoming>| {
        let state = state.clone();
        let target = Target {
            scheme: "https".into(),
            host: host.clone(),
            port,
        };
        async move { Ok::<_, std::convert::Infallible>(handle(req, target, state).await) }
    });
    http1::Builder::new()
        .preserve_header_case(true)
        .serve_connection(io, service)
        .await
        .map_err(|e| anyhow::anyhow!("serving mitm connection: {e}"))?;
    Ok(())
}

/// Decide where a (non-CONNECT) request should go.
fn resolve_target(
    req: &Request<Incoming>,
    forced: Option<&Target>,
    state: &ProxyState,
) -> std::result::Result<Target, String> {
    if let Some(t) = forced {
        return Ok(t.clone());
    }
    // Absolute-form: the URI carries scheme + authority.
    if let Some(authority) = req.uri().authority() {
        let scheme = req.uri().scheme_str().unwrap_or("http").to_string();
        let default_port = if scheme == "https" { 443 } else { 80 };
        return Ok(Target {
            host: authority.host().to_string(),
            port: authority.port_u16().unwrap_or(default_port),
            scheme,
        });
    }
    // Origin-form: fall back to the configured upstream (reverse-proxy mode).
    match &state.default_upstream {
        Some(u) => Ok(Target { scheme: u.scheme.clone(), host: u.host.clone(), port: u.port }),
        None => Err(
            "origin-form request but no --upstream/--preset configured; set HTTPS_PROXY or pass a preset"
                .to_string(),
        ),
    }
}

/// The shared request path: capture the request, run the active engine.
async fn handle(req: Request<Incoming>, target: Target, state: Arc<ProxyState>) -> Resp {
    let captured = match CapturedRequest::from_hyper(req, &target).await {
        Ok(c) => c,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("reading request: {e}")),
    };
    match &state.engine {
        Engine::Record(rec) => match rec.handle(captured).await {
            Ok(resp) => resp,
            Err(e) => {
                error!("record error: {e}");
                error_response(StatusCode::BAD_GATEWAY, &format!("upstream error: {e}"))
            }
        },
        Engine::Replay(rep) => rep.handle(captured).await,
    }
}

/// A fully-read outgoing request, ready for matching, recording or forwarding.
pub struct CapturedRequest {
    pub method: String,
    pub target: Target,
    pub url: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl CapturedRequest {
    pub async fn from_hyper(req: Request<Incoming>, target: &Target) -> Result<Self> {
        let method = req.method().to_string();
        let path = req.uri().path().to_string();
        let query = req.uri().query().unwrap_or("").to_string();
        let path_and_query = req
            .uri()
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| path.clone());
        let url = target.url_for(&path_and_query);
        let headers = req
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).to_string(),
                )
            })
            .collect();
        let body = req
            .into_body()
            .collect()
            .await
            .context("reading request body")?
            .to_bytes();
        Ok(CapturedRequest {
            method,
            target: target.clone(),
            url,
            path,
            query,
            headers,
            body,
        })
    }

    /// Header value lookup (case-insensitive).
    #[allow(dead_code)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Connect to an upstream, returning an HTTP-capable byte stream.
pub async fn connect_upstream(
    target: &Target,
    client_tls: &Arc<rustls::ClientConfig>,
) -> Result<Box<dyn Io>> {
    let tcp = TcpStream::connect((target.host.as_str(), target.port))
        .await
        .with_context(|| format!("connecting to {}:{}", target.host, target.port))?;
    tcp.set_nodelay(true).ok();
    if target.scheme == "https" {
        let connector = tokio_rustls::TlsConnector::from(client_tls.clone());
        let server_name = rustls::pki_types::ServerName::try_from(target.host.clone())
            .with_context(|| format!("invalid upstream host {}", target.host))?;
        let tls = connector
            .connect(server_name, tcp)
            .await
            .with_context(|| format!("TLS handshake with {}", target.host))?;
        Ok(Box::new(tls))
    } else {
        Ok(Box::new(tcp))
    }
}

// ----- small body helpers -------------------------------------------------

pub fn full(data: impl Into<Bytes>) -> BoxBody<Bytes, BoxError> {
    Full::new(data.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn empty() -> BoxBody<Bytes, BoxError> {
    Full::new(Bytes::new())
        .map_err(|never| match never {})
        .boxed()
}

/// Build a small error response carrying a `replaykit` explanation header.
pub fn error_response(status: StatusCode, message: &str) -> Resp {
    let body = format!("replaykit: {message}\n");
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    resp.headers_mut()
        .insert("x-replaykit-error", header_value(message));
    resp
}

pub fn header_value(s: &str) -> hyper::header::HeaderValue {
    hyper::header::HeaderValue::from_str(&s.chars().filter(|c| !c.is_control()).collect::<String>())
        .unwrap_or_else(|_| hyper::header::HeaderValue::from_static("replaykit"))
}
