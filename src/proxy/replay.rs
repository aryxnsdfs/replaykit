//! Replay mode: do not hit the network. For each outgoing request, find the
//! matching recorded response and return it. No match (or out-of-order) is a
//! divergence, handled per the configured policy.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::header::{HeaderMap, HeaderName, HeaderValue};
use hyper::{Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use tracing::{debug, warn};

use super::record::copy_response_headers;
use super::{
    connect_upstream, error_response, full, header_value, BoxError, CapturedRequest, Resp,
};
use crate::cassette::{CassetteReader, Interaction};
use crate::config::Upstream;
use crate::divergence::{Decision, Divergence, DivergenceKind, DivergencePolicy, ReplayCursor};
use crate::matcher::{self, MatchConfig, RequestView};
use crate::util;

const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "content-length",
];

/// Per-step record of how replay resolved a request, persisted for the CLI and
/// dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct StepOutcome {
    pub step: usize,
    pub endpoint: String,
    pub matched_step: Option<usize>,
    pub tier: Option<String>,
    pub in_order: bool,
    pub diverged: bool,
}

/// The replay report written to `<run-dir>/last-replay.json`.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    pub run_id: String,
    pub policy: String,
    pub total_recorded: usize,
    pub steps_seen: usize,
    pub divergence_count: usize,
    pub divergences: Vec<Divergence>,
    pub outcomes: Vec<StepOutcome>,
}

struct ReplayState {
    cursor: ReplayCursor,
    outcomes: Vec<StepOutcome>,
    divergences: Vec<Divergence>,
    failed: bool,
}

pub struct ReplayEngine {
    reader: Arc<CassetteReader>,
    policy: DivergencePolicy,
    match_config: MatchConfig,
    preserve_timing: bool,
    client_tls: Option<Arc<rustls::ClientConfig>>,
    default_upstream: Option<Upstream>,
    report_path: PathBuf,
    state: Mutex<ReplayState>,
}

impl ReplayEngine {
    pub fn new(
        reader: Arc<CassetteReader>,
        policy: DivergencePolicy,
        match_config: MatchConfig,
        preserve_timing: bool,
        client_tls: Option<Arc<rustls::ClientConfig>>,
        default_upstream: Option<Upstream>,
    ) -> Self {
        let len = reader.interactions().len();
        let report_path = reader.root().join("last-replay.json");
        ReplayEngine {
            reader,
            policy,
            match_config: match_config.clone(),
            preserve_timing,
            client_tls,
            default_upstream,
            report_path,
            state: Mutex::new(ReplayState {
                cursor: ReplayCursor::new(len, match_config),
                outcomes: Vec::new(),
                divergences: Vec::new(),
                failed: false,
            }),
        }
    }

    /// True if any hard (fail-fast) divergence has occurred.
    pub fn failed(&self) -> bool {
        self.state.lock().unwrap().failed
    }

    pub fn divergences(&self) -> Vec<Divergence> {
        self.state.lock().unwrap().divergences.clone()
    }

    /// Snapshot the current report.
    pub fn report(&self) -> ReplayReport {
        let st = self.state.lock().unwrap();
        ReplayReport {
            run_id: self.reader.manifest().run_id.clone(),
            policy: format!("{:?}", self.policy),
            total_recorded: self.reader.interactions().len(),
            steps_seen: st.outcomes.len(),
            divergence_count: st.divergences.len(),
            divergences: st.divergences.clone(),
            outcomes: st.outcomes.clone(),
        }
    }

    /// Persist the report to disk (best-effort).
    pub fn write_report(&self) {
        let report = self.report();
        if let Ok(json) = serde_json::to_string_pretty(&report) {
            let _ = std::fs::write(&self.report_path, json);
        }
    }

    pub async fn handle(&self, captured: CapturedRequest) -> Resp {
        let view = RequestView {
            method: &captured.method,
            url: &captured.url,
            host: &captured.target.host,
            path: &captured.path,
            query: &captured.query,
            headers: &captured.headers,
            body: &captured.body,
        };
        let diff_text = format!(
            "{} {}\n{}",
            captured.method,
            captured.url,
            matcher::extract_prompt_text(&captured.body)
        );

        let interactions = self.reader.interactions();
        let decision = {
            let mut st = self.state.lock().unwrap();
            st.cursor.resolve(interactions, &view, &diff_text)
        };

        match decision {
            Decision::Serve {
                interaction_index,
                tier,
                in_order,
            } => {
                let endpoint = matcher::compute_keys(&view, &self.match_config).endpoint;
                let interaction = &interactions[interaction_index];
                if !in_order {
                    // Soft divergence: served, but the agent reached it out of order.
                    let step = {
                        let mut st = self.state.lock().unwrap();
                        let step = st.cursor.current_step().saturating_sub(1);
                        let div = Divergence {
                            step,
                            kind: DivergenceKind::OutOfOrder {
                                expected_step: step,
                                found_step: interaction.step,
                            },
                            closest_step: Some(interaction.step),
                            actual_endpoint: endpoint.clone(),
                            diff: String::new(),
                            summary: format!(
                                "diverged at step {step}: request matched recorded step {} out of order",
                                interaction.step
                            ),
                        };
                        st.divergences.push(div);
                        step
                    };
                    warn!(
                        "out-of-order match at replay step {step} -> recorded step {}",
                        interaction.step
                    );
                }
                self.record_outcome(
                    &endpoint,
                    Some(interaction.step),
                    Some(tier.label()),
                    in_order,
                    false,
                );
                self.build_response(interaction, tier.label(), in_order)
            }
            Decision::Diverged(div) => self.handle_divergence(div, captured).await,
        }
    }

    async fn handle_divergence(&self, div: Divergence, captured: CapturedRequest) -> Resp {
        warn!("{}", div.summary);
        let endpoint = div.actual_endpoint.clone();
        match self.policy {
            DivergencePolicy::FailFast => {
                {
                    let mut st = self.state.lock().unwrap();
                    st.failed = true;
                    st.divergences.push(div.clone());
                }
                self.record_outcome(&endpoint, None, None, false, true);
                self.write_report();
                let body = format!(
                    "{}\n\n--- closest recorded request diff ---\n{}",
                    div.summary, div.diff
                );
                let mut resp = error_response(StatusCode::BAD_GATEWAY, "divergence (fail-fast)");
                *resp.body_mut() = full(body);
                resp.headers_mut()
                    .insert("x-replaykit-divergence", header_value("fail-fast"));
                resp
            }
            DivergencePolicy::PassthroughLive => {
                {
                    let mut st = self.state.lock().unwrap();
                    st.divergences.push(div);
                }
                self.record_outcome(&endpoint, None, None, false, true);
                self.write_report();
                self.forward_live(captured).await
            }
            DivergencePolicy::ReturnClosest => {
                let closest = div.closest_step;
                {
                    let mut st = self.state.lock().unwrap();
                    st.divergences.push(div);
                }
                self.record_outcome(&endpoint, closest, Some("closest"), false, true);
                self.write_report();
                match closest.and_then(|s| self.reader.interactions().iter().find(|i| i.step == s))
                {
                    Some(interaction) => {
                        let mut resp = self.build_response(interaction, "closest", false);
                        resp.headers_mut()
                            .insert("x-replaykit-divergence", header_value("return-closest"));
                        resp
                    }
                    None => error_response(
                        StatusCode::BAD_GATEWAY,
                        "divergence with no closest recording to return",
                    ),
                }
            }
        }
    }

    fn record_outcome(
        &self,
        endpoint: &str,
        matched_step: Option<usize>,
        tier: Option<&str>,
        in_order: bool,
        diverged: bool,
    ) {
        let mut st = self.state.lock().unwrap();
        let step = st.outcomes.len();
        st.outcomes.push(StepOutcome {
            step,
            endpoint: endpoint.to_string(),
            matched_step,
            tier: tier.map(|s| s.to_string()),
            in_order,
            diverged,
        });
    }

    /// Build an HTTP response from a recorded interaction, streaming SSE bodies.
    fn build_response(&self, interaction: &Interaction, tier: &str, in_order: bool) -> Resp {
        let status = StatusCode::from_u16(interaction.response.status)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);

        if interaction.response.stream {
            let (tx, rx) = futures::channel::mpsc::unbounded::<Result<Frame<Bytes>, BoxError>>();
            let refs = interaction.response.body.clone();
            let store_dir = self.reader.root().join(crate::cassette::BLOBS_DIR);
            let preserve = self.preserve_timing;
            tokio::spawn(async move {
                // Reopen the store in the task to avoid borrowing self.
                let store = match crate::cassette::store::BlobStore::open_readonly(&store_dir) {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.unbounded_send(Err(format!("blob store: {e}").into()));
                        return;
                    }
                };
                for r in refs {
                    if preserve {
                        if let Some(ms) = r.delay_ms {
                            let capped = ms.min(2000);
                            if capped > 0 {
                                tokio::time::sleep(std::time::Duration::from_millis(capped)).await;
                            }
                        }
                    }
                    match store.get_chunk(&r.hash) {
                        Ok(data) => {
                            if tx
                                .unbounded_send(Ok(Frame::data(Bytes::from(data))))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Err(e) => {
                            let _ = tx.unbounded_send(Err(
                                format!("missing blob {}: {e}", r.hash).into()
                            ));
                            break;
                        }
                    }
                }
            });
            let body = StreamBody::new(rx).boxed();
            let mut out = Response::new(body);
            *out.status_mut() = status;
            self.apply_recorded_headers(interaction, out.headers_mut(), tier, in_order);
            out
        } else {
            let body = match self.reader.response_body(interaction) {
                Ok(b) => b,
                Err(e) => {
                    return error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        &format!("reassembling recorded body: {e}"),
                    )
                }
            };
            let mut out = Response::new(full(body));
            *out.status_mut() = status;
            self.apply_recorded_headers(interaction, out.headers_mut(), tier, in_order);
            out
        }
    }

    fn apply_recorded_headers(
        &self,
        interaction: &Interaction,
        out: &mut HeaderMap,
        tier: &str,
        in_order: bool,
    ) {
        for h in &interaction.response.headers {
            if HOP_BY_HOP.contains(&h.name.to_lowercase().as_str()) {
                continue;
            }
            if let (Ok(n), Ok(v)) = (
                HeaderName::from_bytes(h.name.as_bytes()),
                HeaderValue::from_str(&h.value),
            ) {
                out.append(n, v);
            }
        }
        out.insert("x-replaykit-mode", HeaderValue::from_static("replay"));
        out.insert(
            "x-replaykit-step",
            header_value(&interaction.step.to_string()),
        );
        out.insert("x-replaykit-tier", header_value(tier));
        if !in_order {
            out.insert(
                "x-replaykit-order",
                HeaderValue::from_static("out-of-order"),
            );
        }
    }

    /// Forward a request to the live upstream (passthrough policy only) and
    /// return the response without recording it.
    async fn forward_live(&self, captured: CapturedRequest) -> Resp {
        let client_tls =
            match &self.client_tls {
                Some(c) => c.clone(),
                None => return error_response(
                    StatusCode::NOT_IMPLEMENTED,
                    "passthrough requested but live upstream is unavailable in this configuration",
                ),
            };
        let _ = &self.default_upstream; // target is carried on captured already
        let upstream_req = match super::record::build_upstream_request_pub(&captured) {
            Ok(r) => r,
            Err(e) => return error_response(StatusCode::BAD_REQUEST, &format!("{e}")),
        };
        let io = match connect_upstream(&captured.target, &client_tls).await {
            Ok(io) => io,
            Err(e) => return error_response(StatusCode::BAD_GATEWAY, &format!("connect: {e}")),
        };
        let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(io)).await
        {
            Ok(pair) => pair,
            Err(e) => return error_response(StatusCode::BAD_GATEWAY, &format!("handshake: {e}")),
        };
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                debug!("passthrough upstream closed: {e}");
            }
        });
        match sender.send_request(upstream_req).await {
            Ok(resp) => {
                let status = resp.status();
                let headers = resp.headers().clone();
                let body = match resp.into_body().collect().await {
                    Ok(b) => b.to_bytes(),
                    Err(e) => {
                        return error_response(StatusCode::BAD_GATEWAY, &format!("body: {e}"))
                    }
                };
                let mut out = Response::new(full(body));
                *out.status_mut() = status;
                copy_response_headers(&headers, out.headers_mut());
                out.headers_mut()
                    .insert("x-replaykit-divergence", header_value("passthrough-live"));
                out
            }
            Err(e) => error_response(StatusCode::BAD_GATEWAY, &format!("upstream: {e}")),
        }
    }
}

/// Pretty-print a recorded interaction's timestamp for logs.
#[allow(dead_code)]
fn fmt_ts() -> String {
    util::now_rfc3339()
}
