//! T-BENCH-ME CLIENT-side bench primitives (post-Merkle-drop model, design
//! RATIFIED 2026-05-29). The public engine's `--bench-me` does ONLY: download
//! the corpus → run the real product dedupe → ANSWER the server's challenge by
//! hashing the downloaded bytes at server-issued offsets → submit. There is NO
//! corpus generation, NO seed, NO Merkle tree on the client — the server holds
//! the private seed + groundtruth and verifies directly.
//!
//! This module is what SURVIVES the generator strip. It depends only on
//! BLAKE3 + std fs; no ChaCha20, no tree-build, no plan/manifest.
#![cfg(feature = "telemetry")]

use base64::Engine;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

fn b64(h: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(h)
}

/// A server-issued challenge position (delivered by `POST /bench/start`): read
/// `length` bytes starting at `byte_offset` from `f{path_index:010}.bin` and
/// hash them. Offsets are PER-FILE (path_index + byte offset within that file),
/// not a global corpus offset — the client has the files on disk.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengePosition {
    pub path_index: u64,
    pub byte_offset: u64,
    pub length: u64,
}

/// One challenge answer: the position echoed back + the BLAKE3 hash of the
/// bytes there (std-base64). `hash` uses the ALREADY-LOCKED leaf preimage
/// (tag `0x00`) — the exact form web's verifier reproduces from the
/// regenerated corpus, so no new hash golden is needed; only `result_digest`
/// is new.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengeAnswer {
    pub path_index: u64,
    pub byte_offset: u64,
    pub length: u64,
    pub hash: String,
}

/// The challenge hash over a chunk: `BLAKE3(0x00 ‖ u32le(path_len) ‖ path_utf8
/// ‖ u64le(offset) ‖ u64le(len) ‖ bytes)`. Identical to the (now-retired)
/// Merkle leaf preimage — path-bound so an answer for one position can't be
/// replayed for another, and byte-locked with web's #160 verifier already.
pub fn challenge_hash(path: &str, offset: u64, len: u64, bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[0x00]);
    h.update(&(path.len() as u32).to_le_bytes());
    h.update(path.as_bytes());
    h.update(&offset.to_le_bytes());
    h.update(&len.to_le_bytes());
    h.update(bytes);
    *h.finalize().as_bytes()
}

/// Answer the server's challenge from the DOWNLOADED corpus on disk. Seeks to
/// each position and reads its bytes (real IO — the work + the I/O signal),
/// hashes them, and returns the answers plus `bytes_read` (total bytes pulled
/// from disk; reported as `run_shape.bytes_scanned` so a no-IO forge → 0 →
/// fails the server's `bytes_scanned == manifest.total_bytes` cross-check).
pub fn answer_challenge_from_dir(
    dir: &Path,
    positions: &[ChallengePosition],
) -> std::io::Result<(Vec<ChallengeAnswer>, u64)> {
    let mut answers = Vec::with_capacity(positions.len());
    let mut bytes_read = 0u64;
    for p in positions {
        let name = format!("f{:010}.bin", p.path_index);
        let mut f = std::fs::File::open(dir.join(&name))?;
        f.seek(SeekFrom::Start(p.byte_offset))?;
        let mut buf = vec![0u8; p.length as usize];
        f.read_exact(&mut buf)?;
        bytes_read += p.length;
        answers.push(ChallengeAnswer {
            path_index: p.path_index,
            byte_offset: p.byte_offset,
            length: p.length,
            hash: b64(&challenge_hash(&name, p.byte_offset, p.length, &buf)),
        });
    }
    Ok((answers, bytes_read))
}

/// Canonical `result_digest` over the client's found dupsets — a compact
/// commitment to the dedupe RESULT the server compares against its private
/// groundtruth. `dupsets` MUST already be canonical (sorted path_index lists,
/// groups sorted — see `bench_corpus::client_found_dupsets`). FROZEN preimage:
/// `BLAKE3(0x05 ‖ u64le(group_count) ‖ for each group: u64le(group_len) ‖
/// u64le(path_index)*)`. std-base64.
pub fn result_digest(dupsets: &[Vec<u64>]) -> String {
    let mut h = blake3::Hasher::new();
    h.update(&[0x05]);
    h.update(&(dupsets.len() as u64).to_le_bytes());
    for set in dupsets {
        h.update(&(set.len() as u64).to_le_bytes());
        for pi in set {
            h.update(&pi.to_le_bytes());
        }
    }
    b64(h.finalize().as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_hash_is_path_bound_and_stable() {
        let base = challenge_hash("f0000000001.bin", 0, 4, b"abcd");
        assert_eq!(base, challenge_hash("f0000000001.bin", 0, 4, b"abcd"), "stable");
        assert_ne!(base, challenge_hash("f0000000002.bin", 0, 4, b"abcd"), "path binds");
        assert_ne!(base, challenge_hash("f0000000001.bin", 1, 4, b"abcd"), "offset binds");
        assert_ne!(base, challenge_hash("f0000000001.bin", 0, 4, b"abce"), "bytes bind");
    }

    #[test]
    fn answer_challenge_reads_disk_and_hashes() {
        // write a tiny on-disk corpus, then answer positions against it.
        let dir = std::env::temp_dir().join(format!("sd-bench-client-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f0000000000.bin"), b"0123456789ABCDEF").unwrap();
        std::fs::write(dir.join("f0000000001.bin"), b"the-quick-brown-fox").unwrap();

        let positions = vec![
            ChallengePosition { path_index: 0, byte_offset: 4, length: 6 }, // "456789"
            ChallengePosition { path_index: 1, byte_offset: 0, length: 9 }, // "the-quick"
        ];
        let (answers, bytes_read) = answer_challenge_from_dir(&dir, &positions).unwrap();
        assert_eq!(bytes_read, 15, "6 + 9 bytes read from disk");
        assert_eq!(answers.len(), 2);
        // hash matches an independent compute over the SAME (path, off, len, bytes).
        assert_eq!(answers[0].hash, b64(&challenge_hash("f0000000000.bin", 4, 6, b"456789")));
        assert_eq!(answers[1].hash, b64(&challenge_hash("f0000000001.bin", 0, 9, b"the-quick")));
        // echoed position is intact.
        assert_eq!(answers[0].path_index, 0);
        assert_eq!(answers[1].byte_offset, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn result_digest_canonical_and_order_sensitive() {
        let a = result_digest(&[vec![0, 6], vec![2, 8, 9]]);
        assert_eq!(a, result_digest(&[vec![0, 6], vec![2, 8, 9]]), "deterministic");
        assert_ne!(a, result_digest(&[vec![0, 7], vec![2, 8, 9]]), "membership binds");
        assert_ne!(a, result_digest(&[vec![2, 8, 9], vec![0, 6]]), "group order binds (caller canonicalizes)");
        assert_ne!(a, result_digest(&[vec![0, 6]]), "group count binds");
        assert_eq!(result_digest(&[]).len(), 44, "empty result still a 44-char b64 digest");
    }

    /// Emits the challenge-response + result_digest GOLDEN VECTOR for web to
    /// byte-match (run: cargo test --features telemetry -- --nocapture
    /// print_client_golden_vector). Asserts internal determinism; the printed
    /// values go to web-superdeduper for cross-impl lock.
    #[test]
    fn print_client_golden_vector() {
        eprintln!("--- T-BENCH-ME CLIENT GOLDEN VECTOR (challenge-response + result_digest) ---");
        // fixed tiny inputs (independent of any corpus generator).
        let samples: [(&str, u64, u64, &[u8]); 3] = [
            ("f0000000000.bin", 0, 8, b"BENCHME0"),
            ("f0000000007.bin", 1048576, 4, b"edge"),
            ("f0000000042.bin", 100, 16, b"sixteen-bytes!!!"),
        ];
        eprintln!("challenge_hash = BLAKE3(0x00||u32le(path_len)||path||u64le(off)||u64le(len)||bytes), std-base64:");
        for (path, off, len, bytes) in samples {
            eprintln!("  path={path} off={off} len={len} bytes={:?} -> {}", std::str::from_utf8(bytes).unwrap(), b64(&challenge_hash(path, off, len, bytes)));
        }
        let dupsets = vec![vec![0u64, 84000], vec![1, 84001], vec![2, 84002, 84003]];
        eprintln!("result_digest preimage = 0x05||u64le(group_count)||(u64le(len)||u64le(pi)*)*; canonical dupsets {dupsets:?}");
        eprintln!("  result_digest -> {}", result_digest(&dupsets));
        // determinism self-check.
        assert_eq!(result_digest(&dupsets), result_digest(&dupsets));
        assert_eq!(challenge_hash("f0000000000.bin", 0, 8, b"BENCHME0"), challenge_hash("f0000000000.bin", 0, 8, b"BENCHME0"));
    }
}
