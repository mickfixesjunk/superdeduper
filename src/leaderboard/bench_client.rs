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

/// A server-issued challenge descriptor (delivered by `POST /bench/start` in
/// its `challenges` array — the client CANNOT derive these, the BC needs the
/// private seed). Read `byte_length` bytes at `byte_offset` from
/// `f{path_index:010}.bin` and hash them. PER-FILE (the client has the files).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengePosition {
    pub path_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// One challenge answer (web's DEPLOYED wire): the descriptor echoed back + the
/// tag-0x02 `challenge_hash` of the bytes there (std-base64). The server
/// regenerates the same range from the private seed and direct-compares.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChallengeAnswer {
    pub path_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub challenge_hash: String,
}

/// The challenge hash over a chunk (FROZEN, research golden): `BLAKE3(0x02 ‖
/// u32le(path_len) ‖ path_utf8 ‖ u64le(offset) ‖ u64le(len) ‖ bytes)`. Tag
/// `0x02` is the CHALLENGE domain (distinct from the retired Merkle leaf `0x00`
/// / node `0x01`). Path-bound so an answer for one position can't be replayed
/// for another; the server reproduces it from the private seed.
pub fn challenge_hash(path: &str, offset: u64, len: u64, bytes: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[0x02]);
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
        let mut buf = vec![0u8; p.byte_length as usize];
        f.read_exact(&mut buf)?;
        bytes_read += p.byte_length;
        answers.push(ChallengeAnswer {
            path_index: p.path_index,
            byte_offset: p.byte_offset,
            byte_length: p.byte_length,
            challenge_hash: b64(&challenge_hash(&name, p.byte_offset, p.byte_length, &buf)),
        });
    }
    Ok((answers, bytes_read))
}

/// FROZEN `result_digest` (research golden) over the client's found dupsets — a
/// compact commitment to the dedupe RESULT the server compares against its
/// private groundtruth. Canonical: clusters sorted by min(path_index), members
/// ascending. Preimage: `BLAKE3( u32le(17) ‖ "tcorpus-result-v1" ‖
/// u64le(cluster_count) ‖ per cluster: u64le(len) ‖ u64le(path_index)* )`.
/// std-base64. (`client_found_dupsets` already yields canonical order, but we
/// re-canonicalize here so the digest is correct regardless of input order.)
pub const RESULT_DIGEST_DOMAIN: &[u8] = b"tcorpus-result-v1";

pub fn result_digest_bytes(dupsets: &[Vec<u64>]) -> [u8; 32] {
    let mut clusters: Vec<Vec<u64>> = dupsets
        .iter()
        .map(|c| {
            let mut v = c.clone();
            v.sort_unstable();
            v
        })
        .collect();
    clusters.sort_by_key(|c| c.first().copied().unwrap_or(0));
    let mut h = blake3::Hasher::new();
    h.update(&(RESULT_DIGEST_DOMAIN.len() as u32).to_le_bytes());
    h.update(RESULT_DIGEST_DOMAIN);
    h.update(&(clusters.len() as u64).to_le_bytes());
    for c in &clusters {
        h.update(&(c.len() as u64).to_le_bytes());
        for &pi in c {
            h.update(&pi.to_le_bytes());
        }
    }
    *h.finalize().as_bytes()
}

pub fn result_digest(dupsets: &[Vec<u64>]) -> String {
    b64(&result_digest_bytes(dupsets))
}

/// Lowercase hex of a 32-byte digest (for cross-checking against research's
/// hex goldens, which print hex; the wire form is [`b64`]). Test-only.
#[cfg(test)]
fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Assemble the canonical-bench submission block (server-direct-verify model)
/// from a completed `--bench-me` run: the challenge `answers` + the dedupe
/// `found_dupsets`. NO Merkle, NO generator — this is the client-side replacement
/// for the retired `bench_corpus::build_canonical_bench`. The `--bench-me` flow
/// drops the returned block into a `SubmissionInputs` (scope=canonical-bench,
/// dedupe-only wall, bytes_scanned=bytes_read) alongside
/// `result_summary.client_found_dupsets = found_dupsets`.
pub fn to_canonical_bench(
    protocol_version: &str,
    corpus_version: &str,
    tier: &str,
    bench_run_id: &str,
    answers: &[ChallengeAnswer],
    found_dupsets: &[Vec<u64>],
) -> super::submission::CanonicalBench {
    super::submission::CanonicalBench {
        protocol_version: protocol_version.to_string(),
        corpus_version: corpus_version.to_string(),
        tier: tier.to_string(),
        bench_run_id: bench_run_id.to_string(),
        bench_proof: serde_json::json!({
            "answers": answers,
            "result_digest": result_digest(found_dupsets),
        }),
    }
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
            ChallengePosition { path_index: 0, byte_offset: 4, byte_length: 6 }, // "456789"
            ChallengePosition { path_index: 1, byte_offset: 0, byte_length: 9 }, // "the-quick"
        ];
        let (answers, bytes_read) = answer_challenge_from_dir(&dir, &positions).unwrap();
        assert_eq!(bytes_read, 15, "6 + 9 bytes read from disk");
        assert_eq!(answers.len(), 2);
        // hash matches an independent compute over the SAME (path, off, len, bytes).
        assert_eq!(answers[0].challenge_hash, b64(&challenge_hash("f0000000000.bin", 4, 6, b"456789")));
        assert_eq!(answers[1].challenge_hash, b64(&challenge_hash("f0000000001.bin", 0, 9, b"the-quick")));
        // descriptor echoed back per web's deployed wire.
        assert_eq!(answers[0].path_index, 0);
        assert_eq!((answers[1].byte_offset, answers[1].byte_length), (0, 9));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn result_digest_canonical_and_content_sensitive() {
        let a = result_digest(&[vec![0, 6], vec![2, 8, 9]]);
        assert_eq!(a, result_digest(&[vec![0, 6], vec![2, 8, 9]]), "deterministic");
        assert_ne!(a, result_digest(&[vec![0, 7], vec![2, 8, 9]]), "membership binds");
        // re-canonicalized internally: group order + member order do NOT matter.
        assert_eq!(a, result_digest(&[vec![2, 8, 9], vec![0, 6]]), "group order canonicalized");
        assert_eq!(a, result_digest(&[vec![6, 0], vec![9, 2, 8]]), "member order canonicalized");
        assert_ne!(a, result_digest(&[vec![0, 6]]), "group count binds");
        assert_eq!(result_digest(&[]).len(), 44, "empty result still a 44-char b64 digest");
    }

    #[test]
    fn matches_research_challenge_and_result_golden() {
        use super::super::bench;
        // result_digest golden: dupsets [[1,4,6]] → research's d8093f61… (hex).
        assert_eq!(
            hex32(&result_digest_bytes(&[vec![1, 4, 6]])),
            "d8093f61b09b2eef44fb186e8afd36e0f25ddce9d2e594cdccf26d0f387a686d",
            "result_digest must match research golden byte-for-byte"
        );
        // challenge_hash golden: f0 (content_id 0, 16B) from the golden seed
        // 000102…1f → research's T0ue6Dbv… (std-base64). Regenerates the corpus
        // content to confirm the challenge form matches end-to-end.
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        let (kc, _) = bench::corpus_keys(&seed);
        // All 4 of web's regression vectors (their verifier byte-matched these
        // against research's lock). content_id == path_index in the fixture.
        let chash = |cid: u64, path: &str, off: u64, len: u64| -> String {
            let mut data = vec![0u8; len as usize];
            bench::content_bytes_at(&kc, cid, off, &mut data);
            b64(&challenge_hash(path, off, len, &data))
        };
        assert_eq!(chash(0, "f0000000000.bin", 0, 16), "T0ue6DbvgHrGSD8Zs93DvT3G5i8o3eNUKSSbeyJVisY=", "f0/0/16");
        assert_eq!(chash(5, "f0000000005.bin", 0, 1000), "8cJYmV2EiWmEfhmfyGnoVn8NsGK7fK+ykxFnjEdJXvk=", "f5/0/1000");
        assert_eq!(chash(7, "f0000000007.bin", 0, 4096), "L6lAnTK8/mvlMtECBESmZcIiylEa/n1g7sAuutP7/RQ=", "f7/0/4096");
        assert_eq!(chash(7, "f0000000007.bin", 1_048_576, 64), "c+MrndAf17w6LwA6trqBC/3nZKdK6MYA5m4kGMqj4cA=", "f7 tail/1MiB/64");
    }

    #[test]
    fn to_canonical_bench_assembles_submission_block() {
        let answers = vec![
            ChallengeAnswer { path_index: 0, byte_offset: 0, byte_length: 8, challenge_hash: "AAAA".into() },
            ChallengeAnswer { path_index: 9, byte_offset: 64, byte_length: 4, challenge_hash: "BBBB".into() },
        ];
        let dupsets = vec![vec![1u64, 4, 6]];
        let cb = to_canonical_bench("tbench-1", "corpus-v1-quick", "quick", "run-Z", &answers, &dupsets);
        assert_eq!(cb.bench_run_id, "run-Z");
        assert_eq!(cb.protocol_version, "tbench-1");
        assert_eq!(cb.bench_proof.pointer("/answers").and_then(|a| a.as_array()).map(Vec::len), Some(2));
        assert_eq!(cb.bench_proof.pointer("/answers/1/path_index").and_then(|v| v.as_u64()), Some(9));
        assert_eq!(cb.bench_proof.pointer("/result_digest").and_then(|v| v.as_str()), Some(result_digest(&dupsets).as_str()));
        // round-trips into a submission payload as the canonical-bench block.
        let inputs = super::super::submission::SubmissionInputs {
            client_version: "t".into(),
            run_uuid: "u".into(),
            scan_id: None,
            bench: Some(cb),
            hardware: super::super::hardware::detect(),
            run_shape: super::super::submission::RunShape {
                wall_clock_seconds: 2.0,
                bytes_scanned: 1024,
                files_scanned: 3,
                hash_algorithm: "river5-aes-ni".into(),
                walker_variant: "hybrid".into(),
                scope: "canonical-bench".into(),
                features_used_bitmap: 0,
                corpus_kind: "canonical-bench".into(),
                cache_hit_ratio: None,
                easter_egg_hits: Vec::new(),
                zero_byte_group_max: None,
                max_hardlink_count_in_scan: None,
                name_collision_count: None,
                share_count_in_scope: None,
                dry_run: None,
                groups_reviewed_count: None,
            },
            result_summary: super::super::submission::ResultSummary {
                duplicate_groups: 1,
                duplicate_bytes_reclaimable: 100,
                largest_single_group_bytes: 0,
                actions_taken_summary: std::collections::BTreeMap::new(),
                placeholder_skip_count: None,
                placeholder_skip_bytes: None,
                client_found_dupsets: Some(dupsets),
            },
        };
        let p = super::super::submission::build_payload(&inputs, "id");
        assert_eq!(p.get("bench_run_id").and_then(|v| v.as_str()), Some("run-Z"));
        assert_eq!(p.pointer("/bench_proof/answers/1/path_index").and_then(|v| v.as_u64()), Some(9));
        assert!(p.pointer("/bench_proof/result_digest").is_some());
        assert!(p.pointer("/result_summary/client_found_dupsets").is_some());
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
        eprintln!("challenge_hash = BLAKE3(0x02||u32le(path_len)||path||u64le(off)||u64le(len)||bytes), std-base64:");
        for (path, off, len, bytes) in samples {
            eprintln!("  path={path} off={off} len={len} bytes={:?} -> {}", std::str::from_utf8(bytes).unwrap(), b64(&challenge_hash(path, off, len, bytes)));
        }
        let dupsets = vec![vec![0u64, 84000], vec![1, 84001], vec![2, 84002, 84003]];
        eprintln!("result_digest preimage = u32le(17)||\"tcorpus-result-v1\"||u64le(cluster_count)||(u64le(len)||u64le(pi)*)*; canonical dupsets {dupsets:?}");
        eprintln!("  result_digest -> {}", result_digest(&dupsets));
        // determinism self-check.
        assert_eq!(result_digest(&dupsets), result_digest(&dupsets));
        assert_eq!(challenge_hash("f0000000000.bin", 0, 8, b"BENCHME0"), challenge_hash("f0000000000.bin", 0, 8, b"BENCHME0"));
    }
}
