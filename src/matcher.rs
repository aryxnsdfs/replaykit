//! Tiered, semantic request matching.
//!
//! On replay the agent's requests are never byte-identical to what was
//! recorded — timestamps move, UUIDs and request-ids change, auth tokens
//! rotate, and the prompt grows every turn. So we fingerprint each request at
//! several levels of strictness and, at replay time, take the highest-
//! confidence tier that clears the configured floor.
//!
//! | Tier | Name        | What it ignores                                   |
//! |------|-------------|---------------------------------------------------|
//! | A    | exact       | nothing (hash of the canonical request)           |
//! | B    | normalized  | volatile headers + volatile JSON fields, key order|
//! | C    | structural  | all scalar *values* — same endpoint + body shape  |
//! | D    | similarity  | (optional, off by default) prompt-text similarity |

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The five tiers, ordered by confidence (highest first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tier {
    /// Optional fuzzy prompt-similarity match (lowest confidence).
    Similarity = 1,
    /// Same endpoint and body shape; scalar values may differ.
    Structural = 2,
    /// Volatile fields stripped, JSON key order canonicalised.
    Normalized = 3,
    /// Byte-for-byte identical canonical request.
    Exact = 4,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Exact => "exact",
            Tier::Normalized => "normalized",
            Tier::Structural => "structural",
            Tier::Similarity => "similarity",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_lowercase().as_str() {
            "exact" => Tier::Exact,
            "normalized" => Tier::Normalized,
            "structural" => Tier::Structural,
            "similarity" => Tier::Similarity,
            _ => return None,
        })
    }
}

/// Tunable matching policy. Defaults are chosen to "just work" for the major
/// LLM and tool APIs while staying configurable for anything unusual.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchConfig {
    /// Header names (lower-cased) stripped before the normalized tier.
    pub volatile_headers: Vec<String>,
    /// JSON field names (lower-cased, matched anywhere in the body) dropped
    /// before the normalized tier.
    pub volatile_json_fields: Vec<String>,
    /// Lowest tier accepted as a match. Anything below this is a divergence.
    pub min_tier: Tier,
    /// Enable the optional similarity tier (token-overlap on prompt text).
    pub enable_similarity: bool,
    /// Similarity threshold in `[0,1]` for the similarity tier.
    pub similarity_threshold: f64,
}

impl Default for MatchConfig {
    fn default() -> Self {
        MatchConfig {
            volatile_headers: [
                "authorization",
                "x-api-key",
                "api-key",
                "cookie",
                "set-cookie",
                "date",
                "user-agent",
                "x-request-id",
                "x-amzn-requestid",
                "x-amz-request-id",
                "cf-ray",
                "openai-organization",
                "openai-processing-ms",
                "x-stainless-arch",
                "x-stainless-os",
                "x-stainless-lang",
                "x-stainless-runtime",
                "x-stainless-runtime-version",
                "x-stainless-package-version",
                "x-stainless-retry-count",
                "x-stainless-async",
                "traceparent",
                "tracestate",
                "content-length",
                "host",
                "accept-encoding",
                "connection",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            volatile_json_fields: [
                "request_id",
                "id",
                "created",
                "created_at",
                "timestamp",
                "nonce",
                "trace_id",
                "session_id",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            min_tier: Tier::Structural,
            enable_similarity: false,
            similarity_threshold: 0.85,
        }
    }
}

/// A request as seen at the egress boundary, the input to key computation.
#[derive(Debug, Clone)]
pub struct RequestView<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(String, String)],
    pub body: &'a [u8],
}

use crate::cassette::MatchKeys;

/// Compute every fingerprint for a request under `cfg`.
pub fn compute_keys(req: &RequestView, cfg: &MatchConfig) -> MatchKeys {
    let endpoint = format!("{} {}{}", req.method, req.host, req.path);
    let body_json: Option<Value> = serde_json::from_slice(req.body).ok();

    let exact = {
        let mut h = blake3::Hasher::new();
        h.update(req.method.as_bytes());
        h.update(b"\n");
        h.update(req.url.as_bytes());
        h.update(b"\n");
        let mut headers: Vec<String> = req
            .headers
            .iter()
            .map(|(n, v)| format!("{}:{}", n.to_lowercase(), v))
            .collect();
        headers.sort();
        h.update(headers.join("\n").as_bytes());
        h.update(b"\n");
        h.update(req.body);
        h.finalize().to_hex().to_string()
    };

    let normalized = {
        let mut h = blake3::Hasher::new();
        h.update(req.method.as_bytes());
        h.update(b"\n");
        h.update(req.path.as_bytes());
        h.update(b"?");
        h.update(canonical_query(req.query).as_bytes());
        h.update(b"\n");
        let mut headers: Vec<String> = req
            .headers
            .iter()
            .filter(|(n, _)| !cfg.volatile_headers.contains(&n.to_lowercase()))
            .map(|(n, v)| format!("{}:{}", n.to_lowercase(), v))
            .collect();
        headers.sort();
        h.update(headers.join("\n").as_bytes());
        h.update(b"\n");
        match &body_json {
            Some(v) => {
                let mut v = v.clone();
                strip_volatile(&mut v, &cfg.volatile_json_fields);
                normalize_strings(&mut v);
                h.update(canonical_json(&v).as_bytes());
            }
            None => {
                h.update(req.body);
            }
        };
        h.finalize().to_hex().to_string()
    };

    let structural = {
        let mut h = blake3::Hasher::new();
        h.update(endpoint.as_bytes());
        h.update(b"\n");
        match &body_json {
            Some(v) => {
                h.update(shape(v).as_bytes());
                h.update(b"\n");
                let mut ids = Vec::new();
                collect_identity(v, &mut ids);
                ids.sort();
                ids.dedup();
                h.update(ids.join(",").as_bytes());
            }
            None => {
                h.update(b"<opaque-body>");
            }
        };
        h.finalize().to_hex().to_string()
    };

    let prompt_text = body_json.as_ref().map(extract_prompt).unwrap_or_default();

    MatchKeys {
        endpoint,
        exact,
        normalized,
        structural,
        prompt_text,
    }
}

/// Result of matching an incoming request against a candidate interaction.
#[derive(Debug, Clone)]
pub struct MatchResult {
    pub tier: Tier,
    /// `[0,1]` confidence — 1.0 for exact/normalized/structural, the similarity
    /// score for the similarity tier.
    pub score: f64,
}

/// Compare two key sets and return the strongest tier at which they match
/// (respecting `cfg.min_tier` and the similarity toggle). `None` = no match.
pub fn compare(a: &MatchKeys, b: &MatchKeys, cfg: &MatchConfig) -> Option<MatchResult> {
    let tier = if a.exact == b.exact {
        Some((Tier::Exact, 1.0))
    } else if a.normalized == b.normalized {
        Some((Tier::Normalized, 1.0))
    } else if a.structural == b.structural {
        Some((Tier::Structural, 1.0))
    } else if cfg.enable_similarity {
        let s = token_similarity(&a.prompt_text, &b.prompt_text);
        if s >= cfg.similarity_threshold {
            Some((Tier::Similarity, s))
        } else {
            None
        }
    } else {
        None
    };
    tier.and_then(|(t, score)| {
        if t >= cfg.min_tier {
            Some(MatchResult { tier: t, score })
        } else {
            None
        }
    })
}

// ----- JSON canonicalisation helpers -------------------------------------

/// Serialise a `Value` with deterministic key order. `serde_json::Map` is a
/// `BTreeMap` by default (no `preserve_order` feature), so keys come out sorted.
fn canonical_json(v: &Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<&str> = query.split('&').collect();
    pairs.sort_unstable();
    pairs.join("&")
}

/// Recursively remove any object field whose (lower-cased) name is volatile.
fn strip_volatile(v: &mut Value, volatile: &[String]) {
    match v {
        Value::Object(map) => {
            map.retain(|k, _| !volatile.contains(&k.to_lowercase()));
            for (_, child) in map.iter_mut() {
                strip_volatile(child, volatile);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                strip_volatile(child, volatile);
            }
        }
        _ => {}
    }
}

/// Normalize textual JSON scalar values before the normalized-tier hash. This
/// keeps replay robust to harmless prompt spelling/casing/spacing drift while
/// preserving JSON shape and non-text scalar values for stricter matching.
fn normalize_strings(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (_, child) in map.iter_mut() {
                normalize_strings(child);
            }
        }
        Value::Array(arr) => {
            for child in arr.iter_mut() {
                normalize_strings(child);
            }
        }
        Value::String(s) => {
            *s = normalize_text_scalar(s);
        }
        _ => {}
    }
}

fn normalize_text_scalar(s: &str) -> String {
    let folded = collapse_ascii_whitespace(&s.to_lowercase());
    mask_dynamic_literals(&folded)
}

fn collapse_ascii_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn mask_dynamic_literals(s: &str) -> String {
    s.split_whitespace()
        .map(mask_dynamic_token)
        .collect::<Vec<_>>()
        .join(" ")
}

fn mask_dynamic_token(token: &str) -> &str {
    let trimmed = token.trim_matches(|c: char| c.is_ascii_punctuation());
    if is_uuid(trimmed) {
        "{{UUID}}"
    } else if is_timestamp_like(trimmed) {
        "{{TIMESTAMP}}"
    } else if is_long_number(trimmed) {
        "{{NUMBER}}"
    } else {
        token
    }
}

fn is_long_number(s: &str) -> bool {
    s.len() >= 5 && s.chars().all(|c| c.is_ascii_digit())
}

fn is_timestamp_like(s: &str) -> bool {
    if s.len() >= 10
        && s.as_bytes().get(4) == Some(&b'-')
        && s.as_bytes().get(7) == Some(&b'-')
        && s.chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 4 | 7) || c.is_ascii_digit())
    {
        return true;
    }
    if s.len() >= 8
        && s.as_bytes().get(2) == Some(&b':')
        && s.as_bytes().get(5) == Some(&b':')
        && s.chars()
            .enumerate()
            .all(|(i, c)| matches!(i, 2 | 5) || c.is_ascii_digit())
    {
        return true;
    }
    false
}

fn is_uuid(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 5 {
        return false;
    }
    let lens = [8, 4, 4, 4, 12];
    parts
        .iter()
        .zip(lens)
        .all(|(part, len)| part.len() == len && part.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Reduce a value to its structural shape: keys preserved, every scalar
/// replaced by its type name, arrays represented by the shape of their first
/// element. Two requests with the same shape differ only in scalar values.
fn shape(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut parts: Vec<String> = map
                .iter()
                .map(|(k, val)| format!("{k}:{}", shape(val)))
                .collect();
            parts.sort();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(arr) => match arr.first() {
            Some(first) => format!("[{}]", shape(first)),
            None => "[]".to_string(),
        },
        Value::String(_) => "s".to_string(),
        Value::Number(_) => "n".to_string(),
        Value::Bool(_) => "b".to_string(),
        Value::Null => "z".to_string(),
    }
}

/// Collect identity-bearing scalar values (model name, tool/function names,
/// type discriminators) so the structural tier still distinguishes e.g. two
/// different tool calls with the same argument shape.
fn collect_identity(v: &Value, out: &mut Vec<String>) {
    const IDENTITY_KEYS: &[&str] = &["model", "tool", "name", "function", "type", "tool_name"];
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                if IDENTITY_KEYS.contains(&k.to_lowercase().as_str()) {
                    if let Value::String(s) = val {
                        out.push(format!("{}={}", k.to_lowercase(), s));
                    }
                }
                collect_identity(val, out);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_identity(child, out);
            }
        }
        _ => {}
    }
}

/// Public, byte-input wrapper around [`extract_prompt`] for diff rendering.
pub fn extract_prompt_text(body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .map(|v| extract_prompt(&v))
        .unwrap_or_else(|_| String::from_utf8_lossy(body).to_string())
}

/// Best-effort extraction of human prompt text across the common API shapes
/// (OpenAI/Anthropic `messages[].content`, `prompt`, `input`, Google
/// `contents[].parts[].text`).
fn extract_prompt(v: &Value) -> String {
    let mut buf = String::new();
    fn walk(v: &Value, buf: &mut String) {
        match v {
            Value::Object(map) => {
                for key in ["prompt", "input", "text"] {
                    if let Some(Value::String(s)) = map.get(key) {
                        buf.push_str(s);
                        buf.push('\n');
                    }
                }
                if let Some(Value::String(s)) = map.get("content") {
                    buf.push_str(s);
                    buf.push('\n');
                }
                for key in ["messages", "contents", "parts", "content"] {
                    if let Some(child) = map.get(key) {
                        walk(child, buf);
                    }
                }
            }
            Value::Array(arr) => {
                for child in arr {
                    walk(child, buf);
                }
            }
            _ => {}
        }
    }
    walk(v, &mut buf);
    buf
}

/// Jaccard token overlap on whitespace-split tokens; cheap stand-in for the
/// optional embedding tier so similarity matching works with zero deps.
fn token_similarity(a: &str, b: &str) -> f64 {
    use std::collections::HashSet;
    let ta: HashSet<&str> = a.split_whitespace().collect();
    let tb: HashSet<&str> = b.split_whitespace().collect();
    if ta.is_empty() && tb.is_empty() {
        return 1.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view<'a>(
        method: &'a str,
        url: &'a str,
        host: &'a str,
        path: &'a str,
        headers: &'a [(String, String)],
        body: &'a [u8],
    ) -> RequestView<'a> {
        RequestView {
            method,
            url,
            host,
            path,
            query: "",
            headers,
            body,
        }
    }

    #[test]
    fn exact_matches_identical() {
        let cfg = MatchConfig::default();
        let h = vec![("content-type".into(), "application/json".into())];
        let body = br#"{"model":"gpt-4","messages":[{"role":"user","content":"hi"}]}"#;
        let a = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                body,
            ),
            &cfg,
        );
        let b = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                body,
            ),
            &cfg,
        );
        let m = compare(&a, &b, &cfg).unwrap();
        assert_eq!(m.tier, Tier::Exact);
    }

    #[test]
    fn normalized_ignores_auth_and_key_order() {
        let cfg = MatchConfig::default();
        let h1 = vec![
            ("Authorization".into(), "Bearer sk-AAA".into()),
            ("content-type".into(), "application/json".into()),
        ];
        let h2 = vec![
            ("Authorization".into(), "Bearer sk-ZZZ".into()),
            ("content-type".into(), "application/json".into()),
        ];
        // same fields, different JSON key order
        let b1 = br#"{"model":"gpt-4","stream":true}"#;
        let b2 = br#"{"stream":true,"model":"gpt-4"}"#;
        let a = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h1,
                b1,
            ),
            &cfg,
        );
        let b = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h2,
                b2,
            ),
            &cfg,
        );
        let m = compare(&a, &b, &cfg).unwrap();
        assert_eq!(m.tier, Tier::Normalized);
    }

    #[test]
    fn structural_ignores_scalar_values() {
        let cfg = MatchConfig::default();
        let h = vec![("content-type".into(), "application/json".into())];
        let b1 = br#"{"model":"gpt-4","messages":[{"role":"user","content":"hello"}]}"#;
        let b2 = br#"{"model":"gpt-4","messages":[{"role":"user","content":"a totally different prompt"}]}"#;
        let a = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b1,
            ),
            &cfg,
        );
        let b = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b2,
            ),
            &cfg,
        );
        let m = compare(&a, &b, &cfg).unwrap();
        assert_eq!(m.tier, Tier::Structural);
    }

    #[test]
    fn structural_distinguishes_different_models() {
        let cfg = MatchConfig::default();
        let h = vec![("content-type".into(), "application/json".into())];
        let b1 = br#"{"model":"gpt-4","messages":[{"role":"user","content":"x"}]}"#;
        let b2 = br#"{"model":"gpt-3.5","messages":[{"role":"user","content":"y"}]}"#;
        let a = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b1,
            ),
            &cfg,
        );
        let b = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b2,
            ),
            &cfg,
        );
        // identity (model) differs -> not a structural match
        assert!(compare(&a, &b, &cfg).is_none());
    }

    #[test]
    fn no_match_for_different_endpoint() {
        let cfg = MatchConfig::default();
        let h = vec![];
        let b = br#"{"model":"gpt-4"}"#;
        let a = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b,
            ),
            &cfg,
        );
        let c = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/embeddings",
                "api.openai.com",
                "/v1/embeddings",
                &h,
                b,
            ),
            &cfg,
        );
        assert!(compare(&a, &c, &cfg).is_none());
    }

    #[test]
    fn similarity_tier_when_enabled() {
        let cfg = MatchConfig {
            enable_similarity: true,
            similarity_threshold: 0.5,
            min_tier: Tier::Similarity,
            ..MatchConfig::default()
        };
        let h = vec![];
        // different shapes so only similarity can match
        let b1 = br#"{"prompt":"the quick brown fox jumps over the lazy dog today"}"#;
        let b2 = br#"{"prompt":"the quick brown fox jumps over the lazy dog now","extra":1}"#;
        let a = compute_keys(
            &view("POST", "https://api.x.com/c", "api.x.com", "/c", &h, b1),
            &cfg,
        );
        let b = compute_keys(
            &view("POST", "https://api.x.com/c", "api.x.com", "/c", &h, b2),
            &cfg,
        );
        let m = compare(&a, &b, &cfg).unwrap();
        assert_eq!(m.tier, Tier::Similarity);
        assert!(m.score > 0.5);
    }

    #[test]
    fn normalized_ignores_prompt_case_and_extra_spaces() {
        let cfg = MatchConfig::default();
        let h = vec![("content-type".into(), "application/json".into())];
        let b1 = br#"{"model":"qwen2.5:0.5b","prompt":"whats the size of paris","stream":true}"#;
        let b2 =
            br#"{"model":"qwen2.5:0.5b","prompt":" Whats   the SIZE of paris ","stream":true}"#;
        let a = compute_keys(
            &view(
                "POST",
                "http://localhost:11434/api/generate",
                "localhost",
                "/api/generate",
                &h,
                b1,
            ),
            &cfg,
        );
        let b = compute_keys(
            &view(
                "POST",
                "http://localhost:11434/api/generate",
                "localhost",
                "/api/generate",
                &h,
                b2,
            ),
            &cfg,
        );
        let m = compare(&a, &b, &cfg).unwrap();
        assert_eq!(m.tier, Tier::Normalized);
    }

    #[test]
    fn normalized_masks_common_dynamic_literals_in_prompts() {
        let cfg = MatchConfig::default();
        let h = vec![("content-type".into(), "application/json".into())];
        let b1 = br#"{"model":"gpt-4","messages":[{"role":"user","content":"Current time: 2026-06-22 order 94827"}]}"#;
        let b2 = br#"{"model":"gpt-4","messages":[{"role":"user","content":"current time: 2026-06-23 order 11122"}]}"#;
        let a = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b1,
            ),
            &cfg,
        );
        let b = compute_keys(
            &view(
                "POST",
                "https://api.openai.com/v1/chat",
                "api.openai.com",
                "/v1/chat",
                &h,
                b2,
            ),
            &cfg,
        );
        let m = compare(&a, &b, &cfg).unwrap();
        assert_eq!(m.tier, Tier::Normalized);
    }
}
