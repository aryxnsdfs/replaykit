//! Auto (daemon) mode: a persistent hybrid of replay and record.
//!
//! For each request the engine first tries to **match** it against the
//! interactions already in the cassette. On a hit it serves the recorded
//! response offline (exactly like replay). On a miss it **forwards the request
//! to the live upstream and records it** on the fly (exactly like record), so
//! the cassette grows as the agent explores new paths.
//!
//! This is what makes a background daemon feel invisible: you just run your
//! agents normally — known calls are instant and offline, new calls pass
//! through and are captured for next time.
//!
//! Matching here is **stateless** (order-independent and repeatable): the same
//! request always resolves to the same recorded interaction, no cursor. That is
//! the right model for a long-lived daemon serving many independent runs.
//!
//! Known limitation: interactions recorded *during the current daemon session*
//! are not visible to the matcher until the daemon is restarted (the read-side
//! snapshot is taken at startup). A call seen for the first time is therefore
//! recorded once; restart the daemon to replay it offline. Record-then-replay
//! across restarts — the common workflow — is fully supported.

use std::sync::Arc;

use tracing::{debug, warn};

use super::record::RecordEngine;
use super::replay::ReplayEngine;
use super::{error_response, CapturedRequest, Resp};
use crate::cassette::CassetteReader;
use crate::matcher::{self, MatchConfig, RequestView, Tier};

pub struct AutoEngine {
    reader: Arc<CassetteReader>,
    replay: Arc<ReplayEngine>,
    record: Arc<RecordEngine>,
    match_config: MatchConfig,
}

impl AutoEngine {
    pub fn new(
        reader: Arc<CassetteReader>,
        replay: Arc<ReplayEngine>,
        record: Arc<RecordEngine>,
        match_config: MatchConfig,
    ) -> Self {
        AutoEngine {
            reader,
            replay,
            record,
            match_config,
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
        let keys = matcher::compute_keys(&view, &self.match_config);

        // Stateless best match over every recorded interaction.
        let mut best: Option<(usize, Tier, f64)> = None;
        for (i, it) in self.reader.interactions().iter().enumerate() {
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

        if let Some((i, tier, _)) = best {
            debug!("auto: hit ({}) for {}", tier.label(), keys.endpoint);
            return self.replay.serve_recorded(i, tier.label());
        }

        // Miss: forward live + record so the cassette grows.
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
