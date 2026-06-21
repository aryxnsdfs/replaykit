//! Replay metrics: aggregate counters derived from per-step outcomes and
//! divergences. Surfaced on `ReplayReport`, in the JSON report on shutdown, and
//! via the `/api/metrics` dashboard endpoint so users can debug a cassette
//! without scrolling every step.
//!
//! Counts are derived (not maintained alongside state) so the metrics view can
//! never drift from the underlying outcomes.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::divergence::Divergence;

/// Aggregate replay metrics.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricsSummary {
    /// Total steps the agent issued during replay.
    pub steps_total: usize,
    /// Steps that were served from the cassette (any tier, in-order or not).
    pub steps_served: usize,
    /// Steps that diverged hard (no match).
    pub steps_diverged: usize,
    /// Steps served out of order (a soft divergence).
    pub steps_out_of_order: usize,
    /// Per-tier hit counters keyed by tier label (`exact`/`normalized`/...).
    pub tier_hits: BTreeMap<String, usize>,
    /// Per-divergence-reason counters keyed by `DivergenceKind::reason()`.
    pub divergence_reasons: BTreeMap<String, usize>,
    /// Share of steps served by each tier in `[0,1]` (rounded to 4 dp).
    pub tier_hit_rate: BTreeMap<String, f64>,
}

/// Compute a [`MetricsSummary`] from the report's raw outcomes + divergences.
pub fn summarize(
    outcomes: &[crate::proxy::replay::StepOutcome],
    divergences: &[Divergence],
) -> MetricsSummary {
    let mut m = MetricsSummary {
        steps_total: outcomes.len(),
        ..Default::default()
    };

    for o in outcomes {
        if o.diverged {
            m.steps_diverged += 1;
        } else {
            m.steps_served += 1;
            if !o.in_order {
                m.steps_out_of_order += 1;
            }
            if let Some(t) = &o.tier {
                *m.tier_hits.entry(t.clone()).or_insert(0) += 1;
            }
        }
    }

    for d in divergences {
        let key = d.kind.reason().to_string();
        *m.divergence_reasons.entry(key).or_insert(0) += 1;
    }

    let denom = m.steps_served.max(1) as f64;
    for (k, v) in &m.tier_hits {
        let rate = (*v as f64 / denom * 10_000.0).round() / 10_000.0;
        m.tier_hit_rate.insert(k.clone(), rate);
    }

    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::divergence::DivergenceKind;
    use crate::proxy::replay::StepOutcome;

    fn outcome(tier: Option<&str>, in_order: bool, diverged: bool) -> StepOutcome {
        StepOutcome {
            step: 0,
            endpoint: "POST x/y".into(),
            matched_step: Some(0),
            tier: tier.map(|s| s.to_string()),
            in_order,
            diverged,
        }
    }

    fn div(kind: DivergenceKind) -> Divergence {
        Divergence {
            step: 0,
            kind,
            closest_step: None,
            actual_endpoint: "POST x/y".into(),
            diff: String::new(),
            summary: String::new(),
        }
    }

    #[test]
    fn counts_tiers_and_reasons() {
        let outcomes = vec![
            outcome(Some("exact"), true, false),
            outcome(Some("exact"), true, false),
            outcome(Some("normalized"), true, false),
            outcome(Some("structural"), false, false),
            outcome(None, false, true),
        ];
        let divs = vec![
            div(DivergenceKind::NoMatch),
            div(DivergenceKind::OutOfOrder {
                expected_step: 1,
                found_step: 3,
            }),
        ];
        let m = summarize(&outcomes, &divs);
        assert_eq!(m.steps_total, 5);
        assert_eq!(m.steps_served, 4);
        assert_eq!(m.steps_diverged, 1);
        assert_eq!(m.steps_out_of_order, 1);
        assert_eq!(m.tier_hits.get("exact"), Some(&2));
        assert_eq!(m.tier_hits.get("normalized"), Some(&1));
        assert_eq!(m.tier_hits.get("structural"), Some(&1));
        assert_eq!(m.divergence_reasons.get("no_match"), Some(&1));
        assert_eq!(m.divergence_reasons.get("out_of_order"), Some(&1));
        // 2/4 = 0.5 exactly
        assert_eq!(m.tier_hit_rate.get("exact"), Some(&0.5));
    }

    #[test]
    fn empty_is_zero() {
        let m = summarize(&[], &[]);
        assert_eq!(m.steps_total, 0);
        assert!(m.tier_hits.is_empty());
        assert!(m.divergence_reasons.is_empty());
    }
}
