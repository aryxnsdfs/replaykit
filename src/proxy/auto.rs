//! Auto (daemon) mode: a persistent hybrid of replay and record.
//!
//! For each request the engine first tries to **match** it against the
//! interactions it knows about. On a hit it serves the recorded response
//! offline (exactly like replay). On a miss it **forwards the request to the
//! live upstream and records it** on the fly (exactly like record), so the
//! cassette grows as the agent explores new paths.
//!
//! This is what makes a background daemon feel invisible: you just run your
//! agents normally — known calls are instant and offline, new calls pass
//! through and are captured for next time.
//!
//! Matching is **stateless** (order-independent and repeatable): the same
//! request always resolves to the same recorded interaction, no cursor — the
//! right model for a long-lived daemon serving many independent runs.
//!
//! The known-interaction set is kept **live**: before each request the engine
//! cheaply checks whether the cassette's append-only log has grown and, if so,
//! reloads it. So a call recorded a moment ago is replayed offline on its next
//! occurrence — no daemon restart required. (Streamed responses are appended by
//! a background task a beat after they finish, so they become replayable on the
//! request after that; non-streamed responses are available immediately.)

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tracing::{debug, warn};

use super::record::RecordEngine;
use super::replay::ReplayEngine;
use super::{error_response, CapturedRequest, Resp};
use crate::cassette::{self, CassetteReader, Interaction, INTERACTIONS_FILE};
use crate::matcher::{self, MatchConfig, RequestView, Tier};

struct IndexState {
    interactions: Vec<Interaction>,
    /// Size (bytes) of the interaction log already loaded, used to detect growth.
    file_len: u64,
}

pub struct AutoEngine {
    replay: Arc<ReplayEngine>,
    record: Arc<RecordEngine>,
    match_config: MatchConfig,
    jsonl_path: PathBuf,
    index: Mutex<IndexState>,
}

impl AutoEngine {
    pub fn new(
        reader: Arc<CassetteReader>,
        replay: Arc<ReplayEngine>,
        record: Arc<RecordEngine>,
        match_config: MatchConfig,
    ) -> Self {
        let jsonl_path = reader.root().join(INTERACTIONS_FILE);
        let file_len = std::fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0);
        let index = Mutex::new(IndexState {
            interactions: reader.interactions().to_vec(),
            file_len,
        });
        AutoEngine {
            replay,
            record,
            match_config,
            jsonl_path,
            index,
        }
    }

    /// Reload the interaction log if it has grown since we last read it, so
    /// interactions recorded during this daemon session become matchable.
    fn refresh_if_grown(&self) {
        let cur_len = std::fs::metadata(&self.jsonl_path).map(|m| m.len()).unwrap_or(0);
        let mut st = self.index.lock().unwrap();
        if cur_len > st.file_len {
            match cassette::read_interactions(&self.jsonl_path) {
                Ok(mut v) => {
                    v.sort_by_key(|i| i.step);
                    st.interactions = v;
                    st.file_len = cur_len;
                }
                Err(e) => debug!("auto: reloading interactions failed: {e}"),
            }
        }
    }

    pub async fn handle(&self, captured: CapturedRequest) -> Resp {
        self.refresh_if_grown();

        let view = RequestView {
            method: &captured.method,
            url: &captured.url,
            host: &captured.target.host,
            path: &captured.path,
            query: &captured.query,
            headers: &captured.headers,
            body: &captured.body,
        };
        let keys = matcher::compute_keys(&view, &self.match_config);

        // Stateless best match over every known interaction. Clone the winner so
        // the index lock is released before we serve (serving touches disk).
        let hit = {
            let st = self.index.lock().unwrap();
            let mut best: Option<(usize, Tier, f64)> = None;
            for (i, it) in st.interactions.iter().enumerate() {
                if let Some(m) = matcher::compare(&keys, &it.keys, &self.match_config) {
                    let better = match best {
                        None => true,
                        Some((_, t, s)) => (m.tier, m.score) > (t, s),
                    };
                    if better {
                        best = Some((i, m.tier, m.score));
                    }
                }
            }
            best.map(|(i, t, _)| (st.interactions[i].clone(), t))
        };

        if let Some((interaction, tier)) = hit {
            debug!("auto: hit ({}) for {}", tier.label(), keys.endpoint);
            return self.replay.serve_interaction(&interaction, tier.label());
        }

        // Miss: forward live + record so the cassette grows. The next occurrence
        // of this call will be served from disk (refresh_if_grown picks it up).
        debug!(
            "auto: miss for {} — forwarding live + recording",
            keys.endpoint
        );
        match self.record.handle(captured).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("auto: live forward failed: {e}");
                error_response(
                    hyper::StatusCode::BAD_GATEWAY,
                    &format!("upstream error while recording new interaction: {e}"),
                )
            }
        }
    }
}
