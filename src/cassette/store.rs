//! Content-addressed, zstd-compressed blob store (`blobs/<hash>.zst`).
//!
//! Each unique chunk is stored exactly once, keyed by the blake3 hex of its
//! *uncompressed* bytes. Writing the same chunk twice is a no-op, which is what
//! gives the cassette its dedup: a thousand turns that resend the same history
//! all reference the same blobs.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};

use super::manifest::ChunkRef;

const ZSTD_LEVEL: i32 = 7;

/// Owns the `blobs/` directory. Cheap to clone-by-reference via `&self`; all
/// mutable state (the seen-set and byte counter) is behind a mutex so the store
/// can be shared across proxy tasks.
pub struct BlobStore {
    dir: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Hashes already on disk, so we never recompress or rewrite a known chunk.
    /// Holds only 64-char hex strings — flat memory even for huge runs.
    seen: HashSet<String>,
    /// Running total of compressed bytes actually written by this process.
    written_bytes: u64,
}

impl BlobStore {
    /// Open (creating if needed) the blob store rooted at `dir`, pre-populating
    /// the seen-set from any blobs already present (so reopening a cassette
    /// resumes deduping correctly).
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).with_context(|| format!("creating blob dir {}", dir.display()))?;
        let mut seen = HashSet::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            if let Some(name) = entry.file_name().to_str() {
                if let Some(hash) = name.strip_suffix(".zst") {
                    seen.insert(hash.to_string());
                }
            }
        }
        Ok(BlobStore {
            dir,
            inner: Mutex::new(Inner {
                seen,
                written_bytes: 0,
            }),
        })
    }

    /// Open read-only (no writes will happen). Same as [`open`] but documents
    /// intent for inspect/replay/dashboard callers.
    pub fn open_readonly(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open(dir)
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        self.dir.join(format!("{hash}.zst"))
    }

    /// Store a single chunk and return its reference. Idempotent: a chunk whose
    /// hash is already present is not rewritten.
    pub fn put_chunk(&self, data: &[u8]) -> Result<ChunkRef> {
        let hash = blake3::hash(data).to_hex().to_string();
        let mut inner = self.inner.lock().unwrap();
        if !inner.seen.contains(&hash) {
            let compressed = zstd::encode_all(data, ZSTD_LEVEL).context("zstd compress chunk")?;
            let path = self.path_for(&hash);
            // Write to a temp file then rename for atomicity (crash-safety).
            let tmp = path.with_extension("zst.tmp");
            {
                let mut f = fs::File::create(&tmp)
                    .with_context(|| format!("creating {}", tmp.display()))?;
                f.write_all(&compressed)?;
                f.flush()?;
            }
            fs::rename(&tmp, &path).with_context(|| format!("finalising {}", path.display()))?;
            inner.written_bytes += compressed.len() as u64;
            inner.seen.insert(hash.clone());
        }
        Ok(ChunkRef {
            hash,
            len: data.len() as u64,
            delay_ms: None,
        })
    }

    /// Split a whole (non-streamed) body into content-defined chunks and store
    /// each, returning the ordered refs that reassemble it.
    pub fn put_body(&self, data: &[u8]) -> Result<Vec<ChunkRef>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }
        let mut refs = Vec::new();
        for range in super::chunker::chunk(data) {
            refs.push(self.put_chunk(&data[range])?);
        }
        Ok(refs)
    }

    /// Read and decompress a single chunk by hash.
    pub fn get_chunk(&self, hash: &str) -> Result<Vec<u8>> {
        let path = self.path_for(hash);
        let compressed =
            fs::read(&path).with_context(|| format!("reading blob {}", path.display()))?;
        let data = zstd::decode_all(&compressed[..])
            .with_context(|| format!("decompressing blob {hash}"))?;
        Ok(data)
    }

    /// Reassemble a full body from its ordered chunk refs.
    pub fn get_body(&self, refs: &[ChunkRef]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for r in refs {
            out.extend_from_slice(&self.get_chunk(&r.hash)?);
        }
        Ok(out)
    }

    /// Total compressed bytes written by this process so far.
    #[allow(dead_code)]
    pub fn written_bytes(&self) -> u64 {
        self.inner.lock().unwrap().written_bytes
    }

    /// Total on-disk size of every blob currently in the store.
    pub fn total_on_disk(&self) -> Result<u64> {
        let mut total = 0;
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".zst"))
            {
                total += entry.metadata()?.len();
            }
        }
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_body() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path()).unwrap();
        let body = b"the quick brown fox jumps over the lazy dog".repeat(1000);
        let refs = store.put_body(&body).unwrap();
        let got = store.get_body(&refs).unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn dedup_identical_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path()).unwrap();
        let body = vec![7u8; 100_000];
        let _ = store.put_body(&body).unwrap();
        let after_first = store.total_on_disk().unwrap();
        // Storing the same body again must not grow the store.
        let _ = store.put_body(&body).unwrap();
        let after_second = store.total_on_disk().unwrap();
        assert_eq!(after_first, after_second);
    }

    #[test]
    fn overlapping_prompts_dedup() {
        // Simulate 50 agent turns, each resending the growing history. The store
        // must stay far smaller than the sum of logical bytes.
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::open(dir.path()).unwrap();
        let mut history = String::new();
        let mut logical = 0u64;
        for turn in 0..50 {
            history.push_str(&format!(
                "{{\"role\":\"user\",\"content\":\"turn {turn} message with a fair bit of repeated filler text to make chunks\"}},"
            ));
            let body = format!("{{\"messages\":[{history}]}}");
            logical += body.len() as u64;
            store.put_body(body.as_bytes()).unwrap();
        }
        let on_disk = store.total_on_disk().unwrap();
        assert!(on_disk * 4 < logical, "on_disk={on_disk} logical={logical}");
    }
}
