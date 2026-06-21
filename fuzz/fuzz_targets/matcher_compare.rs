#![no_main]
//! Fuzz target: build two requests from arbitrary bytes, compute keys for
//! both, and compare. Property: comparison must never panic; if compare
//! returns a tier the score must be in `[0,1]`; equal inputs must always
//! match at the Exact tier.

use libfuzzer_sys::fuzz_target;

use replaykit::matcher::{compare, compute_keys, MatchConfig, RequestView, Tier};

#[derive(arbitrary::Arbitrary, Debug)]
struct Pair<'a> {
    host: &'a str,
    path: &'a str,
    body_a: &'a [u8],
    body_b: &'a [u8],
    enable_similarity: bool,
}

fuzz_target!(|input: Pair<'_>| {
    let headers: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
    let url = format!("https://{}{}", input.host, input.path);
    let mk = |body: &[u8]| {
        let view = RequestView {
            method: "POST",
            url: &url,
            host: input.host,
            path: input.path,
            query: "",
            headers: &headers,
            body,
        };
        compute_keys(&view, &MatchConfig::default())
    };

    let cfg = MatchConfig {
        enable_similarity: input.enable_similarity,
        min_tier: Tier::Similarity,
        similarity_threshold: 0.0,
        ..MatchConfig::default()
    };

    let a = mk(input.body_a);
    let b = mk(input.body_b);

    if let Some(m) = compare(&a, &b, &cfg) {
        assert!((0.0..=1.0).contains(&m.score));
    }

    // Reflexivity: a request always matches itself at the Exact tier.
    let m = compare(&a, &a, &cfg).expect("self-match must succeed");
    assert_eq!(m.tier, Tier::Exact);
});
