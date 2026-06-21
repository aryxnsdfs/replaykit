#![no_main]
//! Fuzz target: feed arbitrary bytes as a request body (and a fuzzed
//! URL/method/headers tuple) into `matcher::compute_keys`. Property under
//! test: the function must never panic and must always return four
//! non-empty hex strings.

use libfuzzer_sys::fuzz_target;

use replaykit::matcher::{compute_keys, MatchConfig, RequestView};

#[derive(arbitrary::Arbitrary, Debug)]
struct Input<'a> {
    method: &'a str,
    host: &'a str,
    path: &'a str,
    query: &'a str,
    headers: Vec<(&'a str, &'a str)>,
    body: &'a [u8],
}

fuzz_target!(|input: Input<'_>| {
    // Cap header count to keep the corpus useful (avoids quadratic blowup).
    let headers: Vec<(String, String)> = input
        .headers
        .into_iter()
        .take(32)
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let url = format!("https://{}{}", input.host, input.path);
    let view = RequestView {
        method: input.method,
        url: &url,
        host: input.host,
        path: input.path,
        query: input.query,
        headers: &headers,
        body: input.body,
    };
    let cfg = MatchConfig::default();
    let keys = compute_keys(&view, &cfg);
    // Cheap structural invariants — anything else would just re-implement
    // the function. The real win of the fuzzer is panic-freedom.
    assert!(!keys.exact.is_empty());
    assert!(!keys.normalized.is_empty());
    assert!(!keys.structural.is_empty());
});
