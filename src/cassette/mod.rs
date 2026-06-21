//! Cassette read/write: the append-only recorder and the reader used by
//! replay, inspect and the dashboard.

pub mod chunker;
pub mod manifest;
pub mod store;

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{bail, Context, Result};

pub use manifest::{
    ChunkRef, Header, Interaction, Manifest, MatchKeys, RequestRecord, ResponseRecord, BLOBS_DIR,
    CASSETTE_FORMAT_VERSION, INTERACTIONS_FILE, MANIFEST_FILE,
};
use store::BlobStore;

/// Append-only writer used during a recording. Interactions are flushed to
/// `interactions.jsonl` the instant they complete, so memory stays flat and a
/// crash leaves a readable cassette. Run-level aggregates are kept as small
/// counters and written to `manifest.json` on [`finalize`](CassetteWriter::finalize).
pub struct CassetteWriter {
    root: PathBuf,
    store: BlobStore,
    inner: Mutex<WriterInner>,
}

struct WriterInner {
    log: File,
    next_step: usize,
    created_utc: String,
    run_id: String,
    providers: BTreeSet<String>,
    interaction_count: usize,
    total_logical_bytes: u64,
    default_upstream: Option<String>,
}

impl CassetteWriter {
    /// Create (or reopen) a cassette at `root` for recording. `default_upstream`
    /// is the reverse-proxy base URL (if any), persisted so replay can run
    /// offline without re-specifying it.
    pub fn create(
        root: impl AsRef<Path>,
        run_id: String,
        created_utc: String,
        default_upstream: Option<String>,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .with_context(|| format!("creating run dir {}", root.display()))?;
        let store = BlobStore::open(root.join(BLOBS_DIR))?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join(INTERACTIONS_FILE))
            .with_context(|| format!("opening {INTERACTIONS_FILE}"))?;
        Ok(CassetteWriter {
            root,
            store,
            inner: Mutex::new(WriterInner {
                log,
                next_step: 0,
                created_utc,
                run_id,
                providers: BTreeSet::new(),
                interaction_count: 0,
                total_logical_bytes: 0,
                default_upstream,
            }),
        })
    }

    /// Borrow the underlying blob store (proxy handlers stream bodies through
    /// it before assembling the [`Interaction`]).
    pub fn store(&self) -> &BlobStore {
        &self.store
    }

    /// Seed the step counter and interaction count so a reopened cassette
    /// (e.g. the daemon appending to an existing run) continues numbering after
    /// the interactions already on disk instead of colliding from zero.
    pub fn seed_from_existing(&self, existing: usize) {
        let mut inner = self.inner.lock().unwrap();
        if existing > inner.next_step {
            inner.next_step = existing;
        }
        if existing > inner.interaction_count {
            inner.interaction_count = existing;
        }
    }

    /// Reserve the next step index. Steps are handed out in request order so the
    /// recording stays ordered even under concurrent connections.
    pub fn next_step(&self) -> usize {
        let mut inner = self.inner.lock().unwrap();
        let s = inner.next_step;
        inner.next_step += 1;
        s
    }

    /// Append a completed interaction to the log and update aggregates.
    pub fn append(&self, interaction: &Interaction) -> Result<()> {
        let line = serde_json::to_string(interaction).context("serialising interaction")?;
        let mut inner = self.inner.lock().unwrap();
        inner.log.write_all(line.as_bytes())?;
        inner.log.write_all(b"\n")?;
        inner.log.flush()?;
        inner.providers.insert(interaction.request.host.clone());
        inner.interaction_count += 1;
        inner.total_logical_bytes += interaction.request.body_len + interaction.response.body_len;
        Ok(())
    }

    /// Write `manifest.json`. Safe to call repeatedly (e.g. on shutdown signal).
    pub fn finalize(&self) -> Result<Manifest> {
        let inner = self.inner.lock().unwrap();
        let mut manifest = Manifest::new(inner.run_id.clone(), inner.created_utc.clone());
        manifest.interaction_count = inner.interaction_count;
        manifest.total_logical_bytes = inner.total_logical_bytes;
        manifest.total_blob_bytes = self.store.total_on_disk().unwrap_or(0);
        manifest.providers = inner.providers.iter().cloned().collect();
        manifest.default_upstream = inner.default_upstream.clone();
        let json = serde_json::to_string_pretty(&manifest)?;
        let path = self.root.join(MANIFEST_FILE);
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json)?;
        fs::rename(&tmp, &path)?;
        Ok(manifest)
    }

    #[allow(dead_code)]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

/// Reader over a finished (or in-progress) cassette. Loads the small interaction
/// log into memory for matching/inspection; bodies stay on disk and are pulled
/// lazily through the blob store.
pub struct CassetteReader {
    root: PathBuf,
    manifest: Manifest,
    interactions: Vec<Interaction>,
    store: BlobStore,
}

impl CassetteReader {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            bail!("cassette directory not found: {}", root.display());
        }
        let mut interactions = read_interactions(&root.join(INTERACTIONS_FILE))?;
        // Interactions complete (and are appended) in response-completion order,
        // which can differ from request order under concurrency. Sort by the
        // recorded step so replay/inspect always see request order.
        interactions.sort_by_key(|i| i.step);

        // Counts/providers/sizes are always recomputed from the authoritative
        // append-only log (so a manifest written before the run finished is never
        // wrong); `default_upstream` and run metadata are overlaid from
        // manifest.json when present.
        let mut manifest = reconstruct_manifest(&root, &interactions);
        if let Ok(s) = fs::read_to_string(root.join(MANIFEST_FILE)) {
            let m: Manifest = serde_json::from_str(&s).context("parsing manifest.json")?;
            if m.format_version != CASSETTE_FORMAT_VERSION {
                bail!(
                    "cassette format v{} is not supported by this build (expected v{})",
                    m.format_version,
                    CASSETTE_FORMAT_VERSION
                );
            }
            manifest.tool_version = m.tool_version;
            manifest.run_id = m.run_id;
            if !m.created_utc.is_empty() {
                manifest.created_utc = m.created_utc;
            }
            manifest.default_upstream = m.default_upstream;
        }
        let store = BlobStore::open_readonly(root.join(BLOBS_DIR))?;
        Ok(CassetteReader {
            root,
            manifest,
            interactions,
            store,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    pub fn interactions(&self) -> &[Interaction] {
        &self.interactions
    }

    #[allow(dead_code)]
    pub fn store(&self) -> &BlobStore {
        &self.store
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Convenience: reassemble a request body.
    pub fn request_body(&self, i: &Interaction) -> Result<Vec<u8>> {
        self.store.get_body(&i.request.body)
    }

    /// Convenience: reassemble a response body.
    pub fn response_body(&self, i: &Interaction) -> Result<Vec<u8>> {
        self.store.get_body(&i.response.body)
    }
}

/// Read every interaction from an append-only log. Public so the daemon's auto
/// engine can re-read the log to pick up interactions recorded mid-session.
pub fn read_interactions(path: &Path) -> Result<Vec<Interaction>> {
    let file = File::open(path)
        .with_context(|| format!("opening {} — is this a replaykit cassette?", path.display()))?;
    let reader = BufReader::new(file);
    let mut out = Vec::new();
    for (lineno, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let interaction: Interaction = serde_json::from_str(&line)
            .with_context(|| format!("parsing interaction on line {}", lineno + 1))?;
        out.push(interaction);
    }
    Ok(out)
}

fn reconstruct_manifest(root: &Path, interactions: &[Interaction]) -> Manifest {
    let run_id = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown")
        .to_string();
    let created = interactions
        .first()
        .map(|i| i.timestamp.clone())
        .unwrap_or_default();
    let mut m = Manifest::new(run_id, created);
    let mut providers = BTreeSet::new();
    for i in interactions {
        providers.insert(i.request.host.clone());
        m.total_logical_bytes += i.request.body_len + i.response.body_len;
    }
    m.interaction_count = interactions.len();
    m.providers = providers.into_iter().collect();
    m.total_blob_bytes = dir_size(&root.join(BLOBS_DIR)).unwrap_or(0);
    m
}

fn dir_size(dir: &Path) -> Result<u64> {
    let mut total = 0;
    if dir.is_dir() {
        for entry in fs::read_dir(dir)? {
            total += entry?.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matcher::{compute_keys, MatchConfig, RequestView};

    /// Acceptance: a 1000-interaction synthetic run with heavily overlapping
    /// prompts stores in a few MB (proving dedup), and reads back faithfully.
    #[test]
    fn thousand_interactions_dedup_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let writer = CassetteWriter::create(
            dir.path(),
            "scale".into(),
            "2026-01-01T00:00:00Z".into(),
            None,
        )
        .unwrap();
        let cfg = MatchConfig::default();
        let headers = vec![("content-type".to_string(), "application/json".to_string())];

        let mut history = String::new();
        let mut last_body = Vec::new();
        for step in 0..1000usize {
            // Each turn resends the full, growing conversation history.
            history.push_str(&format!(
                "{{\"role\":\"user\",\"content\":\"turn {step}: the agent resends the whole history every single time, which is exactly the repetition replaykit must dedup\"}},"
            ));
            let body = format!("{{\"model\":\"gpt-4o\",\"messages\":[{history}]}}").into_bytes();
            let resp = format!("{{\"id\":\"chatcmpl-{step}\",\"choices\":[{{\"index\":0}}]}}")
                .into_bytes();

            let req_refs = writer.store().put_body(&body).unwrap();
            let resp_refs = writer.store().put_body(&resp).unwrap();
            let view = RequestView {
                method: "POST",
                url: "http://localhost:9000/v1/chat/completions",
                host: "localhost",
                path: "/v1/chat/completions",
                query: "",
                headers: &headers,
                body: &body,
            };
            let keys = compute_keys(&view, &cfg);
            let interaction = Interaction {
                step,
                timestamp: "2026-01-01T00:00:00Z".into(),
                duration_ms: 1,
                request: RequestRecord {
                    method: "POST".into(),
                    url: "http://localhost:9000/v1/chat/completions".into(),
                    host: "localhost".into(),
                    path: "/v1/chat/completions".into(),
                    query: "".into(),
                    headers: vec![Header::new("content-type", "application/json")],
                    body: req_refs,
                    body_len: body.len() as u64,
                },
                response: ResponseRecord {
                    status: 200,
                    headers: vec![],
                    body: resp_refs,
                    body_len: resp.len() as u64,
                    stream: false,
                },
                keys,
            };
            writer.append(&interaction).unwrap();
            if step == 999 {
                last_body = body;
            }
        }
        let manifest = writer.finalize().unwrap();
        assert_eq!(manifest.interaction_count, 1000);

        // Dedup must collapse a large logical size into a few MB on disk.
        assert!(
            manifest.total_logical_bytes > 50_000_000,
            "expected a large logical size, got {}",
            manifest.total_logical_bytes
        );
        assert!(
            manifest.total_blob_bytes < 5_000_000,
            "1000 overlapping turns should compress to a few MB, got {}",
            manifest.total_blob_bytes
        );

        // And the cassette reads back faithfully.
        let reader = CassetteReader::open(dir.path()).unwrap();
        assert_eq!(reader.interactions().len(), 1000);
        let last = &reader.interactions()[999];
        assert_eq!(reader.request_body(last).unwrap(), last_body);
    }
}
