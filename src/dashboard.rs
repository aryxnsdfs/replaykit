//! Local web dashboard: a single-page UI served by the binary that lets the
//! user step through a recorded run and see where (and how) a replay diverged.
//! All assets are embedded into the binary, so there is nothing extra to serve.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use owo_colors::OwoColorize;
use rust_embed::RustEmbed;
use serde::Serialize;
use serde_json::json;
use tokio::net::TcpListener;
use tracing::debug;

use crate::cassette::{CassetteReader, Interaction};
use crate::proxy::{full, Resp};
use crate::util;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Asset;

/// Serve the dashboard for `reader` until the process stops.
pub async fn serve(
    addr: SocketAddr,
    reader: Arc<CassetteReader>,
    open_browser: bool,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding dashboard on {addr}"))?;
    let local = listener.local_addr()?;
    let url = format!("http://{local}");
    println!();
    println!(
        "  {}  {}",
        "replaykit".bold().on_bright_black(),
        "DASHBOARD".bold()
    );
    println!("  {} {}", "run ".dimmed(), reader.root().display());
    println!("  {} {}", "open".dimmed(), url.cyan().underline());
    println!("\n  Press {} to stop.\n", "Ctrl-C".bold());

    if open_browser {
        try_open(&url);
    }

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(p) => p, Err(_) => continue };
                let reader = reader.clone();
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        let reader = reader.clone();
                        async move { Ok::<_, std::convert::Infallible>(route(req, reader).await) }
                    });
                    if let Err(e) = http1::Builder::new().serve_connection(io, service).await {
                        debug!("dashboard connection ended: {e}");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\n{} dashboard stopped", "•".dimmed());
                break;
            }
        }
    }
    Ok(())
}

async fn route(req: Request<Incoming>, reader: Arc<CassetteReader>) -> Resp {
    let path = req.uri().path().to_string();
    match path.as_str() {
        "/" | "/index.html" => asset("index.html"),
        "/api/run" => api_run(&reader),
        p if p.starts_with("/api/interaction/") => {
            let step = p
                .trim_start_matches("/api/interaction/")
                .parse::<usize>()
                .ok();
            match step {
                Some(s) => api_interaction(&reader, s),
                None => json_response(StatusCode::BAD_REQUEST, &json!({"error": "bad step"})),
            }
        }
        p => asset(p.trim_start_matches('/')),
    }
}

fn asset(path: &str) -> Resp {
    match Asset::get(path) {
        Some(file) => {
            let mime = file.metadata.mimetype();
            let mut resp = Response::new(full(file.data.into_owned()));
            resp.headers_mut().insert(
                hyper::header::CONTENT_TYPE,
                hyper::header::HeaderValue::from_str(mime).unwrap_or_else(|_| {
                    hyper::header::HeaderValue::from_static("application/octet-stream")
                }),
            );
            resp
        }
        None => {
            let mut resp = Response::new(full("not found"));
            *resp.status_mut() = StatusCode::NOT_FOUND;
            resp
        }
    }
}

#[derive(Serialize)]
struct InteractionSummary {
    step: usize,
    method: String,
    host: String,
    path: String,
    endpoint: String,
    status: u16,
    req_bytes: u64,
    resp_bytes: u64,
    stream: bool,
    timestamp: String,
    duration_ms: u64,
}

fn summarize(i: &Interaction) -> InteractionSummary {
    InteractionSummary {
        step: i.step,
        method: i.request.method.clone(),
        host: i.request.host.clone(),
        path: i.request.path.clone(),
        endpoint: i.keys.endpoint.clone(),
        status: i.response.status,
        req_bytes: i.request.body_len,
        resp_bytes: i.response.body_len,
        stream: i.response.stream,
        timestamp: i.timestamp.clone(),
        duration_ms: i.duration_ms,
    }
}

fn api_run(reader: &CassetteReader) -> Resp {
    let manifest = reader.manifest();
    let interactions: Vec<InteractionSummary> =
        reader.interactions().iter().map(summarize).collect();
    let report = std::fs::read_to_string(reader.root().join("last-replay.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let body = json!({
        "manifest": manifest,
        "interactions": interactions,
        "report": report,
    });
    json_response(StatusCode::OK, &body)
}

fn api_interaction(reader: &CassetteReader, step: usize) -> Resp {
    let interaction = match reader.interactions().iter().find(|i| i.step == step) {
        Some(i) => i,
        None => return json_response(StatusCode::NOT_FOUND, &json!({"error": "no such step"})),
    };
    let req_body = reader.request_body(interaction).unwrap_or_default();
    let resp_body = reader.response_body(interaction).unwrap_or_default();
    let body = json!({
        "step": interaction.step,
        "request": {
            "method": interaction.request.method,
            "url": interaction.request.url,
            "headers": interaction.request.headers,
            "body": util::pretty_json_or_text(&req_body),
        },
        "response": {
            "status": interaction.response.status,
            "headers": interaction.response.headers,
            "stream": interaction.response.stream,
            "body": util::pretty_json_or_text(&resp_body),
        },
        "keys": interaction.keys,
    });
    json_response(StatusCode::OK, &body)
}

fn json_response(status: StatusCode, value: &serde_json::Value) -> Resp {
    let mut resp = Response::new(full(serde_json::to_vec(value).unwrap_or_default()));
    *resp.status_mut() = status;
    resp.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    resp
}

fn try_open(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}
