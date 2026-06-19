//! On-disk cassette data model.
//!
//! A cassette is a directory the user chooses with `--out`:
//!
//! ```text
//! <run-dir>/
//!   manifest.json        versioned run header (meta, counts, providers)
//!   interactions.jsonl    append-only, one Interaction per line, ordered by step
//!   blobs/<hash>.zst      content-addressed, zstd-compressed unique chunks
//! ```
//!
//! `interactions.jsonl` is the crash-safe source of truth written during a
//! recording: each line is flushed as soon as the interaction completes, so a
//! 1000-step run never has to be buffered in RAM and a `kill -9` mid-run still
//! leaves a readable cassette. `manifest.json` is the small header written at
//! finalize (and reconstructable from the log if the process died first).

use serde::{Deserialize, Serialize};

/// Bumped whenever the on-disk layout changes in a backward-incompatible way.
pub const CASSETTE_FORMAT_VERSION: u32 = 1;

pub const MANIFEST_FILE: &str = "manifest.json";
pub const INTERACTIONS_FILE: &str = "interactions.jsonl";
pub const BLOBS_DIR: &str = "blobs";

/// Top-level header for a cassette. Kept deliberately small — the heavy data
/// lives in `interactions.jsonl` and `blobs/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    /// On-disk format version (see [`CASSETTE_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Version of the `replaykit` binary that produced the cassette.
    pub tool_version: String,
    /// Opaque identifier for the run (the directory name by default).
    pub run_id: String,
    /// RFC3339 timestamp of when recording started.
    pub created_utc: String,
    /// Number of recorded interactions.
    pub interaction_count: usize,
    /// Sum of the on-disk (compressed) size of every unique blob, in bytes.
    pub total_blob_bytes: u64,
    /// Sum of the logical (uncompressed) body bytes across all interactions.
    /// The ratio against `total_blob_bytes` is the dedup+compression win.
    pub total_logical_bytes: u64,
    /// Distinct upstream hosts hit during the run, e.g. `api.openai.com`.
    pub providers: Vec<String>,
    /// The reverse-proxy upstream base URL configured at record time, if any
    /// (e.g. `http://localhost:9000`). Lets `replay` reconstruct origin-form
    /// request identity offline without re-specifying `--upstream`.
    #[serde(default)]
    pub default_upstream: Option<String>,
}

impl Manifest {
    pub fn new(run_id: String, created_utc: String) -> Self {
        Manifest {
            format_version: CASSETTE_FORMAT_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").to_string(),
            run_id,
            created_utc,
            interaction_count: 0,
            total_blob_bytes: 0,
            total_logical_bytes: 0,
            providers: Vec::new(),
            default_upstream: None,
        }
    }
}

/// A single recorded request/response pair, plus the metadata the matcher needs
/// to find it again on replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    /// Zero-based position in the recording, in the order requests were made.
    pub step: usize,
    /// RFC3339 timestamp of when the request was issued.
    pub timestamp: String,
    /// Wall-clock milliseconds the upstream took to send the full response.
    pub duration_ms: u64,
    pub request: RequestRecord,
    pub response: ResponseRecord,
    /// Precomputed keys so the matcher never has to re-derive them at replay.
    pub keys: MatchKeys,
}

/// The set of fingerprints used by the tiered matcher. Computing these once at
/// record time keeps replay cheap and keeps the matching logic auditable from
/// the cassette itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchKeys {
    /// `METHOD host/path` — the coarse endpoint identity.
    pub endpoint: String,
    /// blake3 over the fully canonical request (method, url, sorted headers,
    /// body). Tier A (exact).
    pub exact: String,
    /// blake3 over the canonical request with volatile fields stripped
    /// (auth, dynamic headers, timestamps, request ids). Tier B (normalized).
    pub normalized: String,
    /// blake3 over the structural shape only: endpoint + sorted JSON key paths
    /// of the body (+ tool name & arg keys when present). Tier C (structural).
    pub structural: String,
    /// Best-effort extracted prompt/free text, retained for tier D
    /// (embedding similarity) and for human-readable diffs. May be empty.
    pub prompt_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestRecord {
    pub method: String,
    /// Full reconstructed URL, e.g. `https://api.openai.com/v1/chat/completions`.
    pub url: String,
    pub host: String,
    pub path: String,
    pub query: String,
    pub headers: Vec<Header>,
    /// Ordered chunk references that reassemble the request body.
    pub body: Vec<ChunkRef>,
    /// Logical (uncompressed) length of the request body in bytes.
    pub body_len: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub status: u16,
    pub headers: Vec<Header>,
    /// Ordered chunk references that reassemble the response body. For a
    /// streamed (SSE) response this preserves the exact chunk boundaries and
    /// inter-chunk timing the upstream produced.
    pub body: Vec<ChunkRef>,
    pub body_len: u64,
    /// True if the response was streamed (e.g. `text/event-stream`).
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub name: String,
    pub value: String,
}

impl Header {
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Header {
            name: name.into(),
            value: value.into(),
        }
    }
}

/// A reference to one content-addressed chunk in `blobs/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    /// blake3 hex of the *uncompressed* chunk content.
    pub hash: String,
    /// Uncompressed length in bytes.
    pub len: u64,
    /// For streamed bodies: milliseconds elapsed since the previous chunk
    /// arrived (the first chunk's delay is measured from request start).
    /// `None` for whole (non-streamed) bodies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}
