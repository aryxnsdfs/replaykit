//! Divergence detection: deciding, on replay, whether the agent stayed on the
//! recorded script — and producing a useful report when it didn't.
//!
//! The cardinal rule is **never silently return a wrong entry**. If a replayed
//! request matches nothing (or only matches out of order), that is surfaced
//! loudly with the step number and a diff against the closest recording.

use serde::{Deserialize, Serialize};

use crate::cassette::Interaction;
use crate::matcher::{self, MatchConfig, RequestView, Tier};

/// What to do when replay diverges from the recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DivergencePolicy {
    /// Stop immediately and return an error to the agent (default — surfaces
    /// the bug at the exact step it happens).
    FailFast,
    /// Warn, then forward the request to the live upstream and record nothing.
    PassthroughLive,
    /// Warn, then return the closest recorded response anyway (best-effort
    /// "keep going"; clearly flagged as approximate).
    ReturnClosest,
}

impl DivergencePolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fail-fast" => Some(Self::FailFast),
            "warn-and-passthrough-to-live" | "passthrough" => Some(Self::PassthroughLive),
            "warn-and-return-closest" | "closest" => Some(Self::ReturnClosest),
            _ => None,
        }
    }
}

/// Why a divergence happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DivergenceKind {
    /// No recorded interaction matched the request at all.
    NoMatch,
    /// A matching interaction exists, but not at the expected position — the
    /// agent reached it via a different path.
    OutOfOrder {
        expected_step: usize,
        found_step: usize,
    },
}

impl DivergenceKind {
    /// Stable short label used by metrics grouping and dashboard UI.
    pub fn reason(&self) -> &'static str {
        match self {
            DivergenceKind::NoMatch => "no_match",
            DivergenceKind::OutOfOrder { .. } => "out_of_order",
        }
    }
}

/// A reported divergence, suitable for CLI, dashboard and JSON output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Divergence {
    /// The replay step (request ordinal) at which divergence occurred.
    pub step: usize,
    pub kind: DivergenceKind,
    /// Step of the closest recorded interaction, if any (for the diff).
    pub closest_step: Option<usize>,
    /// Endpoint of the actual request, e.g. `POST api.openai.com/v1/chat`.
    pub actual_endpoint: String,
    /// Unified diff between the closest recorded request and the actual request.
    pub diff: String,
    /// One-line human summary.
    pub summary: String,
}

/// Outcome of resolving one replayed request against the cassette.
#[derive(Debug)]
pub enum Decision {
    /// Serve `interaction_index` (matched at `tier`); `in_order` is false when
    /// it was reached out of order (a soft divergence already recorded).
    Serve {
        interaction_index: usize,
        tier: Tier,
        in_order: bool,
    },
    /// No usable match; carries the divergence report.
    Diverged(Divergence),
}

/// Stateful replay cursor over a cassette. Tracks which interactions have been
/// consumed so repeated identical requests resolve to successive recordings and
/// out-of-order access is detectable. The interaction slice is passed in on each
/// call so the cursor can be owned alongside the (Arc-shared) cassette reader.
pub struct ReplayCursor {
    consumed: Vec<bool>,
    cfg: MatchConfig,
    step: usize,
}

impl ReplayCursor {
    pub fn new(len: usize, cfg: MatchConfig) -> Self {
        ReplayCursor {
            consumed: vec![false; len],
            cfg,
            step: 0,
        }
    }

    pub fn current_step(&self) -> usize {
        self.step
    }

    /// Index of the next unconsumed interaction (the "expected" one).
    fn expected_index(&self) -> Option<usize> {
        self.consumed.iter().position(|c| !c)
    }

    /// Resolve one incoming request against `interactions`. Advances the step
    /// counter. When a match is found out of order, `Serve.in_order` is false
    /// and the caller should record an out-of-order divergence.
    pub fn resolve(
        &mut self,
        interactions: &[Interaction],
        req: &RequestView,
        req_body_for_diff: &str,
    ) -> Decision {
        let step = self.step;
        self.step += 1;
        let keys = matcher::compute_keys(req, &self.cfg);
        let expected = self.expected_index();

        // 1. Prefer the expected (in-order) interaction.
        if let Some(exp) = expected {
            if let Some(m) = matcher::compare(&keys, &interactions[exp].keys, &self.cfg) {
                self.consumed[exp] = true;
                return Decision::Serve {
                    interaction_index: exp,
                    tier: m.tier,
                    in_order: true,
                };
            }
        }

        // 2. Otherwise scan every unconsumed interaction for the best match.
        let mut best: Option<(usize, Tier, f64)> = None;
        for (i, used) in self.consumed.iter().enumerate() {
            if *used {
                continue;
            }
            if let Some(m) = matcher::compare(&keys, &interactions[i].keys, &self.cfg) {
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
            self.consumed[i] = true;
            return Decision::Serve {
                interaction_index: i,
                tier,
                in_order: false,
            };
        }

        // 3. No match at all -> hard divergence with a diff against the closest.
        let closest = self.closest_by_endpoint(interactions, &keys.endpoint);
        let diff = match closest {
            Some(ci) => {
                unified_request_diff(&recorded_request_text(&interactions[ci]), req_body_for_diff)
            }
            None => format!("(no recorded request shares endpoint {})", keys.endpoint),
        };
        let summary = match closest {
            Some(ci) => format!(
                "diverged at step {step}: request to `{}` matched no recording (closest is step {ci})",
                keys.endpoint
            ),
            None => format!("diverged at step {step}: request to `{}` matched no recording", keys.endpoint),
        };
        Decision::Diverged(Divergence {
            step,
            kind: DivergenceKind::NoMatch,
            closest_step: closest,
            actual_endpoint: keys.endpoint,
            diff,
            summary,
        })
    }

    /// Build an out-of-order divergence report for a served-but-misordered hit.
    #[allow(dead_code)]
    pub fn out_of_order_report(
        &self,
        step: usize,
        expected_step: usize,
        found_step: usize,
        endpoint: String,
    ) -> Divergence {
        Divergence {
            step,
            kind: DivergenceKind::OutOfOrder { expected_step, found_step },
            closest_step: Some(found_step),
            actual_endpoint: endpoint,
            diff: String::new(),
            summary: format!(
                "diverged at step {step}: expected recorded step {expected_step} but the agent's request matched step {found_step} (out of order)"
            ),
        }
    }

    fn closest_by_endpoint(&self, interactions: &[Interaction], endpoint: &str) -> Option<usize> {
        interactions
            .iter()
            .position(|i| {
                i.keys.endpoint == endpoint
                    && !self.consumed[i.step.min(self.consumed.len().saturating_sub(1))]
            })
            .or_else(|| {
                interactions
                    .iter()
                    .position(|i| i.keys.endpoint == endpoint)
            })
            .or_else(|| self.expected_index())
    }
}

fn recorded_request_text(i: &Interaction) -> String {
    format!(
        "{} {}\n{}",
        i.request.method,
        i.request.url,
        i.keys.prompt_text.trim()
    )
}

/// Produce a unified text diff between the recorded request and the actual one.
pub fn unified_request_diff(recorded: &str, actual: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(recorded, actual);
    let mut out = String::new();
    out.push_str("--- recorded\n+++ actual\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        out.push_str(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cassette::{ChunkRef, Header, RequestRecord, ResponseRecord};
    use crate::matcher::compute_keys;

    fn mk_interaction(step: usize, body: &[u8]) -> Interaction {
        let cfg = MatchConfig::default();
        let headers = vec![("content-type".to_string(), "application/json".to_string())];
        let view = RequestView {
            method: "POST",
            url: "https://api.openai.com/v1/chat",
            host: "api.openai.com",
            path: "/v1/chat",
            query: "",
            headers: &headers,
            body,
        };
        let keys = compute_keys(&view, &cfg);
        Interaction {
            step,
            timestamp: "2026-01-01T00:00:00Z".into(),
            duration_ms: 1,
            request: RequestRecord {
                method: "POST".into(),
                url: "https://api.openai.com/v1/chat".into(),
                host: "api.openai.com".into(),
                path: "/v1/chat".into(),
                query: "".into(),
                headers: vec![Header::new("content-type", "application/json")],
                body: vec![ChunkRef {
                    hash: "x".into(),
                    len: body.len() as u64,
                    delay_ms: None,
                }],
                body_len: body.len() as u64,
            },
            response: ResponseRecord {
                status: 200,
                headers: vec![],
                body: vec![],
                body_len: 0,
                stream: false,
            },
            keys,
        }
    }

    fn view<'a>(headers: &'a [(String, String)], body: &'a [u8]) -> RequestView<'a> {
        RequestView {
            method: "POST",
            url: "https://api.openai.com/v1/chat",
            host: "api.openai.com",
            path: "/v1/chat",
            query: "",
            headers,
            body,
        }
    }

    #[test]
    fn in_order_exact_replay() {
        let inters = vec![
            mk_interaction(
                0,
                br#"{"model":"gpt-4","messages":[{"role":"user","content":"a"}]}"#,
            ),
            mk_interaction(
                1,
                br#"{"model":"gpt-4","messages":[{"role":"user","content":"b"}]}"#,
            ),
        ];
        let mut cursor = ReplayCursor::new(inters.len(), MatchConfig::default());
        let h = vec![("content-type".to_string(), "application/json".to_string())];
        let d0 = cursor.resolve(
            &inters,
            &view(
                &h,
                br#"{"model":"gpt-4","messages":[{"role":"user","content":"a"}]}"#,
            ),
            "",
        );
        assert!(matches!(
            d0,
            Decision::Serve {
                interaction_index: 0,
                in_order: true,
                ..
            }
        ));
        let d1 = cursor.resolve(
            &inters,
            &view(
                &h,
                br#"{"model":"gpt-4","messages":[{"role":"user","content":"b"}]}"#,
            ),
            "",
        );
        assert!(matches!(
            d1,
            Decision::Serve {
                interaction_index: 1,
                in_order: true,
                ..
            }
        ));
    }

    #[test]
    fn divergence_when_no_match() {
        let inters = vec![mk_interaction(
            0,
            br#"{"model":"gpt-4","messages":[{"role":"user","content":"a"}]}"#,
        )];
        let mut cursor = ReplayCursor::new(inters.len(), MatchConfig::default());
        let h = vec![("content-type".to_string(), "application/json".to_string())];
        // different endpoint entirely
        let req = RequestView {
            method: "POST",
            url: "https://api.openai.com/v1/embeddings",
            host: "api.openai.com",
            path: "/v1/embeddings",
            query: "",
            headers: &h,
            body: br#"{"model":"text-embedding-3","input":"x"}"#,
        };
        let d = cursor.resolve(&inters, &req, "POST /v1/embeddings");
        match d {
            Decision::Diverged(div) => {
                assert_eq!(div.step, 0);
                assert_eq!(div.kind, DivergenceKind::NoMatch);
            }
            _ => panic!("expected divergence"),
        }
    }

    #[test]
    fn diff_is_nonempty_on_text_change() {
        let d = unified_request_diff("hello\nworld\n", "hello\nthere\n");
        assert!(d.contains("-world"));
        assert!(d.contains("+there"));
    }
}
