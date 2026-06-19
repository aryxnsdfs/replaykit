//! Record mode: forward to the real upstream, return the real response, and
//! persist the full request + response to the cassette as it streams by.

use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::Frame;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tracing::debug;

use super::{connect_upstream, full, header_value, BoxError, CapturedRequest, Resp};
use crate::cassette::{
    CassetteWriter, ChunkRef, Header, Interaction, MatchKeys, RequestRecord, ResponseRecord,
};
use crate::matcher::{self, MatchConfig, RequestView};
use crate::util;

/// Hop-by-hop headers that must not be forwarded, plus length/encoding headers
/// we recompute ourselves.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "proxy-connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "proxy-authorization",
    "content-length",
];

pub struct RecordEngine {
    writer: Arc<CassetteWriter>,
    client_tls: Arc<rustls::ClientConfig>,
    match_config: MatchConfig,
}

impl RecordEngine {
    pub fn new(
        writer: Arc<CassetteWriter>,
        client_tls: Arc<rustls::ClientConfig>,
        match_config: MatchConfig,
    ) -> Self {
        RecordEngine {
            writer,
            client_tls,
            match_config,
        }
    }

    /// Forward `captured` to the upstream, recording everything, and return the
    /// upstream's response to the agent.
    pub async fn handle(&self, captured: CapturedRequest) -> Result<Resp> {
        let step = self.writer.next_step();
        let started_at = util::now_rfc3339();
        let clock = Instant::now();

        // Persist the request body up front (content-defined chunks, deduped).
        let req_body_refs = self.writer.store().put_body(&captured.body)?;
        let keys = self.compute_keys(&captured);
        let request_record = RequestRecord {
            method: captured.method.clone(),
            url: captured.url.clone(),
            host: captured.target.host.clone(),
            path: captured.path.clone(),
            query: captured.query.clone(),
            headers: to_headers(&captured.headers),
            body: req_body_refs,
            body_len: captured.body.len() as u64,
        };

        // Build and send the upstream request.
        let upstream_req = build_upstream_request(&captured)?;
        let io = connect_upstream(&captured.target, &self.client_tls).await?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
            .await
            .context("http handshake with upstream")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("upstream connection closed: {e}");
            }
        });
        let upstream_resp = sender
            .send_request(upstream_req)
            .await
            .context("sending request upstream")?;

        let status = upstream_resp.status();
        let resp_headers = upstream_resp.headers().clone();
        let is_stream = is_event_stream(&resp_headers);

        if is_stream {
            self.stream_and_record(
                upstream_resp,
                status,
                resp_headers,
                step,
                started_at,
                clock,
                request_record,
                keys,
            )
            .await
        } else {
            self.buffer_and_record(
                upstream_resp,
                status,
                resp_headers,
                step,
                started_at,
                clock,
                request_record,
                keys,
            )
            .await
        }
    }

    fn compute_keys(&self, captured: &CapturedRequest) -> MatchKeys {
        let view = RequestView {
            method: &captured.method,
            url: &captured.url,
            host: &captured.target.host,
            path: &captured.path,
            query: &captured.query,
            headers: &captured.headers,
            body: &captured.body,
        };
        matcher::compute_keys(&view, &self.match_config)
    }

    /// Non-streaming response: collect fully, store with dedup, return Full.
    #[allow(clippy::too_many_arguments)]
    async fn buffer_and_record(
        &self,
        resp: Response<hyper::body::Incoming>,
        status: StatusCode,
        resp_headers: HeaderMap,
        step: usize,
        started_at: String,
        clock: Instant,
        request: RequestRecord,
        keys: MatchKeys,
    ) -> Result<Resp> {
        let body = resp
            .into_body()
            .collect()
            .await
            .context("reading upstream body")?
            .to_bytes();
        let refs = self.writer.store().put_body(&body)?;
        let duration_ms = clock.elapsed().as_millis() as u64;

        let interaction = Interaction {
            step,
            timestamp: started_at,
            duration_ms,
            request,
            response: ResponseRecord {
                status: status.as_u16(),
                headers: header_map_to_vec(&resp_headers),
                body: refs,
                body_len: body.len() as u64,
                stream: false,
            },
            keys,
        };
        self.writer.append(&interaction)?;

        let mut out = Response::new(full(body));
        *out.status_mut() = status;
        copy_response_headers(&resp_headers, out.headers_mut());
        Ok(out)
    }

    /// Streaming (SSE) response: forward frames live while recording each frame
    /// as a chunk with its inter-frame timing.
    #[allow(clippy::too_many_arguments)]
    async fn stream_and_record(
        &self,
        resp: Response<hyper::body::Incoming>,
        status: StatusCode,
        resp_headers: HeaderMap,
        step: usize,
        started_at: String,
        clock: Instant,
        request: RequestRecord,
        keys: MatchKeys,
    ) -> Result<Resp> {
        let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Frame<Bytes>, BoxError>>();
        let writer = self.writer.clone();
        // Snapshot the recorded headers before the response is moved into the task.
        let recorded_headers = header_map_to_vec(&resp_headers);

        tokio::spawn(async move {
            let mut body = resp.into_body();
            let mut refs: Vec<ChunkRef> = Vec::new();
            let mut total: u64 = 0;
            let mut last = clock;
            loop {
                match body.frame().await {
                    Some(Ok(frame)) => {
                        if let Ok(data) = frame.into_data() {
                            let now = Instant::now();
                            let delay = now.duration_since(last).as_millis() as u64;
                            last = now;
                            if let Ok(mut chunk_ref) = writer.store().put_chunk(&data) {
                                chunk_ref.delay_ms = Some(delay);
                                total += chunk_ref.len;
                                refs.push(chunk_ref);
                            }
                            // Forward to the agent; stop if it hung up.
                            if tx.unbounded_send(Ok(Frame::data(data))).is_err() {
                                break;
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let _ = tx.unbounded_send(Err(Box::new(e) as BoxError));
                        break;
                    }
                    None => break,
                }
            }
            let duration_ms = clock.elapsed().as_millis() as u64;
            let interaction = Interaction {
                step,
                timestamp: started_at,
                duration_ms,
                request,
                response: ResponseRecord {
                    status: status.as_u16(),
                    headers: recorded_headers,
                    body: refs,
                    body_len: total,
                    stream: true,
                },
                keys,
            };
            if let Err(e) = writer.append(&interaction) {
                debug!("failed to append streamed interaction: {e}");
            }
        });

        let body = StreamBody::new(rx).boxed();
        let mut out = Response::new(body);
        *out.status_mut() = status;
        copy_response_headers(&resp_headers, out.headers_mut());
        Ok(out)
    }
}

fn is_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Build the origin-form request to send upstream, copying agent headers but
/// dropping hop-by-hop ones and fixing the Host header.
fn build_upstream_request(captured: &CapturedRequest) -> Result<Request<Full<Bytes>>> {
    let path_and_query = if captured.query.is_empty() {
        captured.path.clone()
    } else {
        format!("{}?{}", captured.path, captured.query)
    };
    let mut builder = Request::builder()
        .method(captured.method.as_str())
        .uri(path_and_query);
    let headers = builder.headers_mut().expect("builder has headers");
    for (name, value) in &captured.headers {
        if HOP_BY_HOP.contains(&name.to_lowercase().as_str()) || name.eq_ignore_ascii_case("host") {
            continue;
        }
        if let (Ok(n), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            headers.append(n, v);
        }
    }
    // Correct Host for the upstream.
    let host_value = if (captured.target.scheme == "https" && captured.target.port == 443)
        || (captured.target.scheme == "http" && captured.target.port == 80)
    {
        captured.target.host.clone()
    } else {
        format!("{}:{}", captured.target.host, captured.target.port)
    };
    headers.insert(hyper::header::HOST, header_value(&host_value));

    builder
        .body(Full::new(captured.body.clone()))
        .context("building upstream request")
}

/// Public wrapper so the replay engine's passthrough path can reuse the exact
/// same upstream-request construction.
pub fn build_upstream_request_pub(captured: &CapturedRequest) -> Result<Request<Full<Bytes>>> {
    build_upstream_request(captured)
}

fn to_headers(headers: &[(String, String)]) -> Vec<Header> {
    headers
        .iter()
        .map(|(n, v)| Header::new(n.clone(), v.clone()))
        .collect()
}

fn header_map_to_vec(headers: &HeaderMap) -> Vec<Header> {
    headers
        .iter()
        .map(|(k, v)| {
            Header::new(
                k.as_str(),
                String::from_utf8_lossy(v.as_bytes()).to_string(),
            )
        })
        .collect()
}

/// Copy upstream response headers to the client response, dropping hop-by-hop
/// and length/encoding headers (hyper recomputes framing).
pub fn copy_response_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (k, v) in from.iter() {
        if HOP_BY_HOP.contains(&k.as_str().to_lowercase().as_str()) {
            continue;
        }
        to.append(k.clone(), v.clone());
    }
}
