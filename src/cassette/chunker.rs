//! Content-defined chunking (a small FastCDC-style gear hasher).
//!
//! Agent prompts are hugely repetitive: every turn resends the whole
//! conversation history. If we split each body into content-defined chunks and
//! store each unique chunk once, the shared history collapses to a handful of
//! blobs no matter how many turns reference it.
//!
//! Content-defined (vs fixed-size) boundaries matter because a new turn inserts
//! text into the *middle* of the JSON (before the closing brackets). Fixed-size
//! chunks would shift after the insertion point and stop deduping; gear-hash
//! boundaries re-synchronise within one chunk, so the unchanged tail still
//! dedups.

/// Target average chunk size. Small enough that near-duplicate bodies share
/// most chunks, large enough that per-chunk overhead stays negligible.
const MIN_SIZE: usize = 2 * 1024;
const AVG_SIZE: usize = 8 * 1024;
const MAX_SIZE: usize = 64 * 1024;

/// Mask with `log2(AVG_SIZE)` bits set; a boundary is cut when the rolling gear
/// hash has those low bits clear.
const MASK: u64 = (AVG_SIZE as u64) - 1;

/// Split `data` into content-defined chunks, returning the byte ranges. The
/// ranges are contiguous and cover the whole input, so
/// `chunks().map(|r| &data[r]).concat() == data`.
pub fn chunk(data: &[u8]) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    let n = data.len();
    if n == 0 {
        return ranges;
    }
    let mut start = 0usize;
    while start < n {
        let end = next_boundary(&data[start..n]).min(n - start) + start;
        ranges.push(start..end);
        start = end;
    }
    ranges
}

/// Find the end offset of the next chunk within `data` (a slice starting at a
/// chunk boundary). Returns at least `min(MIN_SIZE, data.len())` and at most
/// `MAX_SIZE`.
fn next_boundary(data: &[u8]) -> usize {
    let len = data.len();
    if len <= MIN_SIZE {
        return len;
    }
    let mut hash: u64 = 0;
    let scan_end = len.min(MAX_SIZE);
    let mut i = MIN_SIZE;
    // Pre-roll up to MIN_SIZE so the boundary decision has context, but never
    // cut before MIN_SIZE.
    for &b in &data[..MIN_SIZE] {
        hash = (hash << 1).wrapping_add(GEAR[b as usize]);
    }
    while i < scan_end {
        hash = (hash << 1).wrapping_add(GEAR[data[i] as usize]);
        if hash & MASK == 0 {
            return i + 1;
        }
        i += 1;
    }
    scan_end
}

/// A fixed table of pseudo-random 64-bit values, one per byte value. Generated
/// from a splitmix64 sequence at build time of this module (constant, so chunk
/// boundaries are stable across machines and versions).
static GEAR: [u64; 256] = build_gear();

const fn build_gear() -> [u64; 256] {
    let mut table = [0u64; 256];
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut i = 0;
    while i < 256 {
        // splitmix64
        state = state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        table[i] = z;
        i += 1;
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_whole_input() {
        let data: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
        let ranges = chunk(&data);
        assert!(!ranges.is_empty());
        // contiguous and complete
        let mut cursor = 0;
        for r in &ranges {
            assert_eq!(r.start, cursor);
            cursor = r.end;
        }
        assert_eq!(cursor, data.len());
    }

    #[test]
    fn empty_input_no_chunks() {
        assert!(chunk(&[]).is_empty());
    }

    #[test]
    fn respects_max_size() {
        let data = vec![0u8; 500_000]; // all-zero never triggers a hash boundary
        for r in chunk(&data) {
            assert!(r.end - r.start <= MAX_SIZE);
        }
    }

    #[test]
    fn insertion_preserves_tail_chunks() {
        // A realistic "next turn" edit: insert a block in the middle. Most tail
        // chunks should be byte-identical, proving dedup survives insertion.
        let mut a = Vec::new();
        for i in 0..4000u32 {
            a.extend_from_slice(
                format!("message line number {i} with some filler text\n").as_bytes(),
            );
        }
        let mut b = a.clone();
        let insert_at = a.len() / 2;
        b.splice(
            insert_at..insert_at,
            b"INSERTED A BRAND NEW MIDDLE BLOCK\n".iter().copied(),
        );

        let chunks_a: Vec<Vec<u8>> = chunk(&a).iter().map(|r| a[r.clone()].to_vec()).collect();
        let chunks_b: Vec<Vec<u8>> = chunk(&b).iter().map(|r| b[r.clone()].to_vec()).collect();

        let set_a: std::collections::HashSet<&Vec<u8>> = chunks_a.iter().collect();
        let shared = chunks_b.iter().filter(|c| set_a.contains(c)).count();
        // The unchanged head and tail should share most chunks.
        assert!(
            shared > chunks_b.len() / 2,
            "shared={shared} of {}",
            chunks_b.len()
        );
    }
}
