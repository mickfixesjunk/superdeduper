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

/// #4(a) V2 challenge_hash — tag 0x03, server-issued 32-byte blob APPENDED
/// to the V1 preimage. Distinct tag means V1/V2 are cryptographically
/// non-interchangeable (no replay risk across protocol versions). Used when
/// /bench/start returns a top-level `server_challenge_blob`; absent =>
/// V1 fallback during the transition window (web's
/// `BENCH_SERVER_BLOB_REQUIRED` flag, default false until engine ships V2).
pub fn challenge_hash_v2(
    path: &str,
    offset: u64,
    len: u64,
    bytes: &[u8],
    server_blob: &[u8; 32],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[0x03]);
    h.update(&(path.len() as u32).to_le_bytes());
    h.update(path.as_bytes());
    h.update(&offset.to_le_bytes());
    h.update(&len.to_le_bytes());
    h.update(bytes);
    // APPENDED per web 584089d wire spec.
    h.update(server_blob);
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
    answer_challenge_from_dir_v(dir, positions, None)
}

/// #4(a) — answer the server's challenge with optional V2 mode. When
/// `server_blob` is `Some`, uses [`challenge_hash_v2`] (tag 0x03, blob
/// appended) for every position so the server's V2 verifier accepts.
/// `None` keeps V1 (tag 0x02) — pre-#4(a) servers + the transition
/// window's V1-fallback path.
pub fn answer_challenge_from_dir_v(
    dir: &Path,
    positions: &[ChallengePosition],
    server_blob: Option<&[u8; 32]>,
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
        let h = match server_blob {
            Some(b) => challenge_hash_v2(&name, p.byte_offset, p.byte_length, &buf, b),
            None => challenge_hash(&name, p.byte_offset, p.byte_length, &buf),
        };
        answers.push(ChallengeAnswer {
            path_index: p.path_index,
            byte_offset: p.byte_offset,
            byte_length: p.byte_length,
            challenge_hash: b64(&h),
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
/// #4(a) V2 result_digest domain tag. Web 584089d publishes this verbatim;
/// the prefix change pairs with the blob PREPEND so V1/V2 hashes cannot
/// collide.
pub const RESULT_DIGEST_DOMAIN_V2: &[u8] = b"tcorpus-result-v2";

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

/// #4(a) V2 result_digest — `tcorpus-result-v2` prefix, server-issued 32-byte
/// blob PREPENDED into the hash input (before the domain tag). Distinct
/// prefix + prepend position make V1/V2 cryptographically non-interchangeable.
pub fn result_digest_bytes_v2(dupsets: &[Vec<u64>], server_blob: &[u8; 32]) -> [u8; 32] {
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
    // V1 framing kept (u32le(domain_len) || domain || u64le(cluster_count) ||
    // ...), with the server_blob PREPENDED per web 584089d wire spec — but
    // here "prepended" means INSIDE the V1 preimage, after the domain tag,
    // before the cluster data. Golden-locked by `v2_matches_web_golden_vectors`.
    h.update(&(RESULT_DIGEST_DOMAIN_V2.len() as u32).to_le_bytes());
    h.update(RESULT_DIGEST_DOMAIN_V2);
    h.update(server_blob);
    h.update(&(clusters.len() as u64).to_le_bytes());
    for c in &clusters {
        h.update(&(c.len() as u64).to_le_bytes());
        for &pi in c {
            h.update(&pi.to_le_bytes());
        }
    }
    *h.finalize().as_bytes()
}

pub fn result_digest_v2(dupsets: &[Vec<u64>], server_blob: &[u8; 32]) -> String {
    b64(&result_digest_bytes_v2(dupsets, server_blob))
}

// ----------------------------------------------------------------------------
// A3 / v3-mutate (v0.3.0 HARD CUTOVER). Per-run content mutation defeats the
// "precompute-once forge" attack: an attacker who knows the corpus + dedupe
// algorithm can normally precompute the answer table once and replay it
// across runs. v3-mutate derives a per-run, per-file key from the server-
// issued K and the file's own raw content hash, then XORs a ChaCha20
// keystream over the file content before dedupe runs. The mutated content
// is what gets hashed for dedupe AND for the sampled challenge. K never
// repeats across runs, so the attacker must re-do the work each run.
//
// Wire shape:
//   /bench/start response  -> top-level `K` (32 bytes, base64-std)
//   challenge_hash_v3      -> tag 0x04, K appended (cryptographically
//                              distinct from V1 tag 0x02 + V2 tag 0x03)
//   result_digest_v3       -> domain "tcorpus-result-v3", K mixed before
//                              cluster data
//   bench_proof.k_echo     -> client echoes K so server can confirm the
//                              client used the K it was issued
//
// Dupsets in V3 are computed over MUTATED content (post-XOR). Two files
// identical raw → identical file_hash → identical per_file_key →
// identical keystream → identical mutated content. So dedupe still
// surfaces them as a cluster.
// ----------------------------------------------------------------------------

/// V3 result_digest domain. Same prefix-with-length-prefix framing as V1/V2;
/// the changed string + the K mix make V3 cryptographically non-interchangeable
/// with V1 or V2 even when the server happens to issue an all-zero K.
pub const RESULT_DIGEST_DOMAIN_V3: &[u8] = b"tcorpus-result-v3";

/// Derive the per-file mutation key for V3. Inputs: the server-issued
/// per-run key `K` (delivered by /bench/start) and the file's own raw
/// content hash. The HMAC binds the per-file key to BOTH the per-run K
/// (so it can't be replayed across runs) AND the file's actual content
/// (so an attacker who swaps the file's bytes mid-stream changes its
/// key derivation and breaks the mutation).
pub fn per_file_key_v3(k: &[u8; 32], file_hash: &[u8; 32]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac = <Hmac<Sha256>>::new_from_slice(k)
        .expect("HMAC accepts any key length");
    mac.update(file_hash);
    let out = mac.finalize().into_bytes();
    let mut key = [0u8; 32];
    key.copy_from_slice(&out);
    key
}

/// Fill `buf` with `len` bytes of the V3 mutation keystream starting at
/// absolute file offset `offset`. ChaCha20 keyed by `per_file_key` with
/// a zero nonce — the absolute offset is provided via `StreamCipherSeek`
/// so any (offset, len) window can be regenerated independently without
/// hashing forward (essential for the sparse-challenge path where the
/// server samples bytes deep into a file).
pub fn keystream_at_v3(per_file_key: &[u8; 32], offset: u64, buf: &mut [u8]) {
    use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
    let key = chacha20::Key::from(*per_file_key);
    let nonce = chacha20::Nonce::from([0u8; 12]);
    let mut cipher = chacha20::ChaCha20::new(&key, &nonce);
    cipher.seek(offset);
    for b in buf.iter_mut() {
        *b = 0;
    }
    cipher.apply_keystream(buf);
}

/// XOR the V3 mutation keystream over `raw`, returning the mutated bytes.
/// `offset` is the absolute file offset at which `raw` starts (so the
/// keystream is taken from the same position the dedupe pipeline will
/// emit for that file slice).
pub fn mutate_bytes_v3(per_file_key: &[u8; 32], offset: u64, raw: &[u8]) -> Vec<u8> {
    let mut ks = vec![0u8; raw.len()];
    keystream_at_v3(per_file_key, offset, &mut ks);
    raw.iter().zip(ks.iter()).map(|(a, b)| a ^ b).collect()
}

/// V3 challenge_hash: `BLAKE3(0x04 ‖ u32le(path_len) ‖ path ‖ u64le(offset)
/// ‖ u64le(len) ‖ mutated_bytes ‖ K)`. Tag 0x04 + K appended make this
/// cryptographically distinct from V1 (tag 0x02, no K) and V2 (tag 0x03,
/// server_blob appended). `mutated_bytes` MUST be the bytes after
/// keystream XOR — passing raw bytes will produce a hash the server
/// rejects (which is the point: the mutation IS the forge defence).
pub fn challenge_hash_v3(
    path: &str,
    offset: u64,
    len: u64,
    mutated_bytes: &[u8],
    k: &[u8; 32],
) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[0x04]);
    h.update(&(path.len() as u32).to_le_bytes());
    h.update(path.as_bytes());
    h.update(&offset.to_le_bytes());
    h.update(&len.to_le_bytes());
    h.update(mutated_bytes);
    h.update(k);
    *h.finalize().as_bytes()
}

/// V3 result_digest preimage: `BLAKE3( u32le(17) ‖ "tcorpus-result-v3" ‖ K ‖
/// u64le(cluster_count) ‖ per cluster: u64le(len) ‖ u64le(path_index)* )`.
/// Same canonical ordering rule as V1/V2 (clusters sorted by min member,
/// members ascending). dupsets are taken from a dedupe pass run OVER
/// MUTATED content — V3 mutation is deterministic per (K, file_hash) so
/// dupes survive the mutation as identical mutated content.
pub fn result_digest_bytes_v3(dupsets: &[Vec<u64>], k: &[u8; 32]) -> [u8; 32] {
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
    h.update(&(RESULT_DIGEST_DOMAIN_V3.len() as u32).to_le_bytes());
    h.update(RESULT_DIGEST_DOMAIN_V3);
    h.update(k);
    h.update(&(clusters.len() as u64).to_le_bytes());
    for c in &clusters {
        h.update(&(c.len() as u64).to_le_bytes());
        for &pi in c {
            h.update(&pi.to_le_bytes());
        }
    }
    *h.finalize().as_bytes()
}

pub fn result_digest_v3(dupsets: &[Vec<u64>], k: &[u8; 32]) -> String {
    b64(&result_digest_bytes_v3(dupsets, k))
}

/// Compute BLAKE3 over a file's entire raw content. Used to derive the
/// per-file mutation key (`per_file_key_v3(K, file_hash)`). The whole-
/// file read is the forge-defence cost: a precompute-once attacker
/// can't shortcut to a final answer without materialising every file
/// each run (since K changes per run, file_hash binds into the key,
/// and a fake stream of bytes would produce a different file_hash and
/// therefore a different per_file_key → different mutation → different
/// challenge_hash → server reject).
pub fn file_raw_hash(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(*h.finalize().as_bytes())
}

/// V3 answer the challenge from the downloaded corpus, applying the
/// per-run mutation. For each unique file referenced in `positions`,
/// reads the whole file once to compute `file_hash`, derives
/// `per_file_key = HMAC(K, file_hash)`, then for each (offset, len)
/// targeting that file: reads the raw bytes, XORs them with the
/// keystream at that offset, hashes the mutated result via
/// [`challenge_hash_v3`]. `bytes_read` counts the SAMPLED bytes only
/// (not the whole-file reads used to derive file_hash) so the server's
/// `bytes_scanned == manifest.total_bytes` cross-check keeps its
/// existing meaning (the sampled-bytes work the server expects).
pub fn answer_challenge_from_dir_v3(
    dir: &Path,
    positions: &[ChallengePosition],
    k: &[u8; 32],
) -> std::io::Result<(Vec<ChallengeAnswer>, u64)> {
    use std::collections::HashMap;
    let mut per_file_keys: HashMap<u64, [u8; 32]> = HashMap::new();
    let mut answers = Vec::with_capacity(positions.len());
    let mut bytes_read = 0u64;
    for p in positions {
        let name = format!("f{:010}.bin", p.path_index);
        let file_path = dir.join(&name);
        let pf_key = if let Some(k) = per_file_keys.get(&p.path_index) {
            *k
        } else {
            let file_hash = file_raw_hash(&file_path)?;
            let pf_key = per_file_key_v3(k, &file_hash);
            per_file_keys.insert(p.path_index, pf_key);
            pf_key
        };
        let mut f = std::fs::File::open(&file_path)?;
        f.seek(SeekFrom::Start(p.byte_offset))?;
        let mut raw = vec![0u8; p.byte_length as usize];
        f.read_exact(&mut raw)?;
        bytes_read += p.byte_length;
        let mutated = mutate_bytes_v3(&pf_key, p.byte_offset, &raw);
        let h = challenge_hash_v3(&name, p.byte_offset, p.byte_length, &mutated, k);
        answers.push(ChallengeAnswer {
            path_index: p.path_index,
            byte_offset: p.byte_offset,
            byte_length: p.byte_length,
            challenge_hash: b64(&h),
        });
    }
    Ok((answers, bytes_read))
}

/// V3 assembly of the canonical-bench submission block. `bench_proof`
/// carries `answers`, the V3 `result_digest`, and the `k_echo` (base64-
/// std encoded K) so the server can confirm the client used the K it
/// was issued on /bench/start.
#[allow(clippy::too_many_arguments)]
pub fn to_canonical_bench_v3(
    protocol_version: &str,
    corpus_version: &str,
    tier: &str,
    bench_run_id: &str,
    answers: &[ChallengeAnswer],
    found_dupsets: &[Vec<u64>],
    cold_enforced: bool,
    k: &[u8; 32],
) -> super::submission::CanonicalBench {
    let bench_proof = serde_json::json!({
        "answers": answers,
        "result_digest": result_digest_v3(found_dupsets, k),
        "k_echo": b64(k),
    });
    super::submission::CanonicalBench {
        protocol_version: protocol_version.to_string(),
        corpus_version: corpus_version.to_string(),
        tier: tier.to_string(),
        bench_run_id: bench_run_id.to_string(),
        bench_proof,
        cold_enforced,
    }
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
    cold_enforced: bool,
) -> super::submission::CanonicalBench {
    to_canonical_bench_v(
        protocol_version,
        corpus_version,
        tier,
        bench_run_id,
        answers,
        found_dupsets,
        cold_enforced,
        None,
    )
}

/// #4(a) — assembly with optional V2 server_challenge_blob. When
/// `server_blob` is Some, [`result_digest_v2`] (tcorpus-result-v2 prefix,
/// blob prepended) is used + `bench_proof.challenge_blob_echo` is included
/// as the base64-std blob so web's verifier can confirm the echo matches
/// what /bench/start issued. `None` ships V1 unchanged (transition-window
/// path; web's BENCH_SERVER_BLOB_REQUIRED is default false until engine
/// ships V2).
#[allow(clippy::too_many_arguments)]
pub fn to_canonical_bench_v(
    protocol_version: &str,
    corpus_version: &str,
    tier: &str,
    bench_run_id: &str,
    answers: &[ChallengeAnswer],
    found_dupsets: &[Vec<u64>],
    cold_enforced: bool,
    server_blob: Option<&[u8; 32]>,
) -> super::submission::CanonicalBench {
    let bench_proof = match server_blob {
        Some(blob) => serde_json::json!({
            "answers": answers,
            "result_digest": result_digest_v2(found_dupsets, blob),
            "challenge_blob_echo": b64(blob),
        }),
        None => serde_json::json!({
            "answers": answers,
            "result_digest": result_digest(found_dupsets),
        }),
    };
    super::submission::CanonicalBench {
        protocol_version: protocol_version.to_string(),
        corpus_version: corpus_version.to_string(),
        tier: tier.to_string(),
        bench_run_id: bench_run_id.to_string(),
        bench_proof,
        cold_enforced,
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

    /// #4(a) — byte-for-byte cross-stack golden vectors, pinned by web 503a37b
    /// (tests/bench-content.test.ts). Inputs are fully hard-coded so a tag /
    /// endianness / framing bug between this Rust impl and the TS server impl
    /// localizes immediately. Any mismatch here = engine and server will
    /// produce different digests on the real wire => bench rejection.
    #[test]
    fn v2_matches_web_golden_vectors() {
        use super::super::bench;
        // SEED = 0x00..0x1f, BLOB = 0x10..0x2f per web 503a37b.
        let seed: [u8; 32] = std::array::from_fn(|i| i as u8);
        let blob: [u8; 32] = std::array::from_fn(|i| (0x10 + i) as u8);
        let (k_content, _) = bench::corpus_keys(&seed);
        let mk_bytes = |content_id: u64, offset: u64, len: usize| -> Vec<u8> {
            let mut buf = vec![0u8; len];
            bench::content_bytes_at(&k_content, content_id, offset, &mut buf);
            buf
        };
        // challenge_hash_v2 vectors (path -> content_id from f%010d filename).
        let cases: [(&str, u64, u64, u64, &str); 4] = [
            ("f0000000000.bin", 0, 0, 16, "WzL+JY6LdG5oU+7zfhWmeHN6dr0jfxk/s975iCH3EbI="),
            ("f0000000005.bin", 5, 0, 1000, "mSj8nuThsvGHjXtFqPYYzBSNuy3sY8EEx0Bg2xMA0lk="),
            ("f0000000007.bin", 7, 0, 4096, "wxo6q2vc0onvDVgZszo6+J6YzTtBwYjMReeQsWhyf38="),
            ("f0000000007.bin", 7, 1048576, 64, "g/ozJHJcMFMZdtKGw+JJNVYi7jBUz9rkQnmZkOS90Xg="),
        ];
        for (path, cid, off, len, expected_b64) in cases {
            let bytes = mk_bytes(cid, off, len as usize);
            let got = b64(&challenge_hash_v2(path, off, len, &bytes, &blob));
            assert_eq!(
                got, expected_b64,
                "V2 challenge_hash golden mismatch for {path} off={off} len={len}"
            );
        }
        // result_digest_v2 vectors.
        let one_cluster = vec![vec![1u64, 4, 6]];
        assert_eq!(
            result_digest_v2(&one_cluster, &blob),
            "3xrx5Ue8ZJtJgLwHksGD0GbrSNwHmxTppyigC43o4dI=",
            "V2 result_digest golden mismatch for [[1,4,6]]"
        );
        let empty: Vec<Vec<u64>> = Vec::new();
        assert_eq!(
            result_digest_v2(&empty, &blob),
            "BOt3wuuNnC8VQ9VDltkNMlD6Nq5AG8Muy1TGAidautA=",
            "V2 result_digest golden mismatch for empty dupsets"
        );
    }

    /// #4(a) — V2 challenge_hash + V2 result_digest must be cryptographically
    /// distinct from V1 (distinct tag 0x03 vs 0x02 + distinct domain prefix +
    /// blob mix). Web's BENCH_SERVER_BLOB_REQUIRED transition flag relies on
    /// the two paths being non-interchangeable: a V1 client's old digest
    /// MUST NOT accidentally match a V2 server's expectation, even when the
    /// blob happens to be all-zeros.
    #[test]
    fn v2_is_cryptographically_distinct_from_v1() {
        // challenge_hash: V1 vs V2 (even with all-zero blob) must differ.
        let v1 = challenge_hash("f0000000001.bin", 0, 4, b"abcd");
        let zero_blob = [0u8; 32];
        let v2_zero = challenge_hash_v2("f0000000001.bin", 0, 4, b"abcd", &zero_blob);
        assert_ne!(
            v1, v2_zero,
            "V1 (tag 0x02) and V2-with-zero-blob (tag 0x03 + appended) must produce different digests"
        );
        // V2 binds to the blob: distinct blobs -> distinct hashes.
        let one_blob = [1u8; 32];
        let v2_one = challenge_hash_v2("f0000000001.bin", 0, 4, b"abcd", &one_blob);
        assert_ne!(v2_zero, v2_one, "V2 blob must bind into challenge_hash");

        // result_digest: V1 vs V2 differ for the same dupsets (even with zero blob).
        let dupsets = vec![vec![0u64, 7], vec![1, 8]];
        let r1 = result_digest_bytes(&dupsets);
        let r2_zero = result_digest_bytes_v2(&dupsets, &zero_blob);
        assert_ne!(r1, r2_zero, "V1 and V2 result_digest must produce different digests");
        // V2 result binds to the blob: distinct blobs -> distinct digests.
        let r2_one = result_digest_bytes_v2(&dupsets, &one_blob);
        assert_ne!(r2_zero, r2_one, "V2 blob must bind into result_digest");
    }

    /// #4(a) — to_canonical_bench_v with Some(blob) emits challenge_blob_echo
    /// in bench_proof and uses V2 result_digest; None omits the echo and
    /// uses V1.
    #[test]
    fn to_canonical_bench_v_emits_challenge_blob_echo_in_v2() {
        let blob = [0x55u8; 32];
        let bench_v2 = to_canonical_bench_v(
            "v3-mutate", "corpus-v2-quick", "quick", "run-X",
            &[],
            &[vec![1u64, 2]],
            true,
            Some(&blob),
        );
        // Echo field present and base64-equal to the blob (44 chars std-base64).
        let echo = bench_v2.bench_proof
            .get("challenge_blob_echo")
            .and_then(|v| v.as_str())
            .expect("challenge_blob_echo present in V2 bench_proof");
        assert_eq!(echo, b64(&blob));
        // V2 result_digest used (distinct from V1 over the same dupsets).
        let v2_digest = bench_v2.bench_proof.get("result_digest").and_then(|v| v.as_str()).unwrap();
        assert_eq!(v2_digest, result_digest_v2(&[vec![1u64, 2]], &blob));
        assert_ne!(v2_digest, result_digest(&[vec![1u64, 2]]));

        // V1 path: no echo field, V1 digest.
        let bench_v1 = to_canonical_bench_v(
            "tcorpus-1", "corpus-v2-quick", "quick", "run-X",
            &[],
            &[vec![1u64, 2]],
            true,
            None,
        );
        assert!(bench_v1.bench_proof.get("challenge_blob_echo").is_none(),
            "V1 must NOT carry challenge_blob_echo");
        assert_eq!(
            bench_v1.bench_proof.get("result_digest").and_then(|v| v.as_str()).unwrap(),
            result_digest(&[vec![1u64, 2]]),
        );
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
        let cb = to_canonical_bench("tbench-1", "corpus-v1-quick", "quick", "run-Z", &answers, &dupsets, true);
        assert_eq!(cb.bench_run_id, "run-Z");
        assert_eq!(cb.protocol_version, "tbench-1");
        assert!(cb.cold_enforced, "cold_enforced threads through");
        assert_eq!(cb.bench_proof.pointer("/answers").and_then(|a| a.as_array()).map(Vec::len), Some(2));
        assert_eq!(cb.bench_proof.pointer("/answers/1/path_index").and_then(|v| v.as_u64()), Some(9));
        assert_eq!(cb.bench_proof.pointer("/result_digest").and_then(|v| v.as_str()), Some(result_digest(&dupsets).as_str()));
        // round-trips into a submission payload as the canonical-bench block.
        let inputs = super::super::submission::SubmissionInputs {
            client_version: "t".into(),
            run_uuid: "u".into(),
            scan_id: None,
            bench: Some(cb),
            lane: None,
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

    // -----------------------------------------------------------------------
    // A3 / v3-mutate (v0.3.0). All tests below this line exercise V3 only.
    // -----------------------------------------------------------------------

    /// Deterministic V3 K for tests. 0x80..0x9f — distinct from any V1 seed
    /// (0x00..0x1f) or V2 blob (0x10..0x2f) used in this file so a slip
    /// between V1/V2/V3 vectors localises immediately.
    fn v3_test_k() -> [u8; 32] {
        std::array::from_fn(|i| (0x80 + i) as u8)
    }

    #[test]
    fn v3_per_file_key_binds_k_and_file_hash() {
        let k = v3_test_k();
        let h1: [u8; 32] = std::array::from_fn(|i| i as u8);
        let h2: [u8; 32] = std::array::from_fn(|i| (i as u8).wrapping_add(1));
        let k_other: [u8; 32] = std::array::from_fn(|i| (i as u8) ^ 0xff);
        let a = per_file_key_v3(&k, &h1);
        // determinism
        assert_eq!(a, per_file_key_v3(&k, &h1));
        // K binds: distinct K -> distinct per_file_key (same file_hash)
        assert_ne!(a, per_file_key_v3(&k_other, &h1));
        // file_hash binds: distinct file_hash -> distinct per_file_key (same K)
        assert_ne!(a, per_file_key_v3(&k, &h2));
    }

    #[test]
    fn v3_mutate_bytes_is_deterministic_and_involutive() {
        let k = v3_test_k();
        let file_hash: [u8; 32] = std::array::from_fn(|i| i as u8);
        let pf_key = per_file_key_v3(&k, &file_hash);
        let raw = b"the-quick-brown-fox-jumps-over-the-lazy-dog";
        let m1 = mutate_bytes_v3(&pf_key, 0, raw);
        let m2 = mutate_bytes_v3(&pf_key, 0, raw);
        assert_eq!(m1, m2, "mutation is deterministic");
        assert_ne!(m1.as_slice(), raw.as_slice(), "mutation actually XORs (not no-op)");
        // XOR is its own inverse: mutating the mutated content reproduces raw.
        let restored = mutate_bytes_v3(&pf_key, 0, &m1);
        assert_eq!(restored.as_slice(), raw.as_slice(), "XOR is involutive");
        // offset binds: same bytes at offset 0 vs offset 100 produce
        // different mutated output (different keystream region).
        let m_at_100 = mutate_bytes_v3(&pf_key, 100, raw);
        assert_ne!(m1, m_at_100, "absolute offset binds into the keystream");
    }

    #[test]
    fn v3_is_cryptographically_distinct_from_v1_and_v2() {
        // challenge_hash: V1 (tag 0x02) vs V2 (tag 0x03, zero blob) vs V3
        // (tag 0x04, zero K) must all differ even when the only changing
        // inputs are tag + appended bytes.
        let zero32 = [0u8; 32];
        let bytes = b"abcd";
        let v1 = challenge_hash("f0000000001.bin", 0, 4, bytes);
        let v2 = challenge_hash_v2("f0000000001.bin", 0, 4, bytes, &zero32);
        let v3 = challenge_hash_v3("f0000000001.bin", 0, 4, bytes, &zero32);
        assert_ne!(v1, v2, "V1 vs V2 distinct");
        assert_ne!(v1, v3, "V1 vs V3 distinct");
        assert_ne!(v2, v3, "V2 vs V3 distinct");
        // K binds into V3 challenge_hash: distinct K -> distinct hash.
        let one32 = [1u8; 32];
        let v3_one = challenge_hash_v3("f0000000001.bin", 0, 4, bytes, &one32);
        assert_ne!(v3, v3_one, "K binds into V3 challenge_hash");

        // result_digest: V1 vs V2-zero vs V3-zero must differ for the same
        // dupsets, and V3 must bind to K.
        let dupsets = vec![vec![0u64, 7], vec![1, 8]];
        let r1 = result_digest_bytes(&dupsets);
        let r2 = result_digest_bytes_v2(&dupsets, &zero32);
        let r3 = result_digest_bytes_v3(&dupsets, &zero32);
        assert_ne!(r1, r2);
        assert_ne!(r1, r3);
        assert_ne!(r2, r3);
        let r3_one = result_digest_bytes_v3(&dupsets, &one32);
        assert_ne!(r3, r3_one, "K binds into V3 result_digest");
    }

    #[test]
    fn v3_challenge_hash_is_position_and_k_bound_and_stable() {
        let k = v3_test_k();
        let bytes = b"abcd";
        let base = challenge_hash_v3("f0000000001.bin", 0, 4, bytes, &k);
        assert_eq!(base, challenge_hash_v3("f0000000001.bin", 0, 4, bytes, &k), "stable");
        assert_ne!(base, challenge_hash_v3("f0000000002.bin", 0, 4, bytes, &k), "path binds");
        assert_ne!(base, challenge_hash_v3("f0000000001.bin", 1, 4, bytes, &k), "offset binds");
        assert_ne!(base, challenge_hash_v3("f0000000001.bin", 0, 4, b"abce", &k), "bytes bind");
        let k_other: [u8; 32] = std::array::from_fn(|i| (i as u8) ^ 0xff);
        assert_ne!(base, challenge_hash_v3("f0000000001.bin", 0, 4, bytes, &k_other), "K binds");
    }

    #[test]
    fn v3_result_digest_canonical_and_k_bound() {
        let k = v3_test_k();
        let a = result_digest_v3(&[vec![0, 6], vec![2, 8, 9]], &k);
        assert_eq!(a, result_digest_v3(&[vec![0, 6], vec![2, 8, 9]], &k), "deterministic");
        // canonicalisation: group / member order doesn't matter.
        assert_eq!(a, result_digest_v3(&[vec![2, 8, 9], vec![0, 6]], &k), "group order canonicalised");
        assert_eq!(a, result_digest_v3(&[vec![6, 0], vec![9, 2, 8]], &k), "member order canonicalised");
        // membership binds.
        assert_ne!(a, result_digest_v3(&[vec![0, 7], vec![2, 8, 9]], &k), "membership binds");
        // K binds.
        let k_other: [u8; 32] = std::array::from_fn(|i| (i as u8) ^ 0xff);
        assert_ne!(a, result_digest_v3(&[vec![0, 6], vec![2, 8, 9]], &k_other), "K binds");
        // empty still a 44-char b64 digest.
        let e: Vec<Vec<u64>> = Vec::new();
        assert_eq!(result_digest_v3(&e, &k).len(), 44);
    }

    #[test]
    fn v3_answer_challenge_reads_mutates_and_hashes() {
        // tiny on-disk corpus: file content is fully under our control so we
        // can compute the expected mutated challenge_hash externally and
        // check the V3 read path matches byte-for-byte.
        let k = v3_test_k();
        let dir = std::env::temp_dir().join(format!("sd-bench-client-v3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let content_a: &[u8] = b"the-quick-brown-fox-jumps-over-the-lazy-dog!!!!";
        let content_b: &[u8] = b"another-totally-different-file-content!!!!!!!!!";
        std::fs::write(dir.join("f0000000000.bin"), content_a).unwrap();
        std::fs::write(dir.join("f0000000001.bin"), content_b).unwrap();

        let positions = vec![
            ChallengePosition { path_index: 0, byte_offset: 4, byte_length: 11 }, // "quick-brown"
            ChallengePosition { path_index: 1, byte_offset: 0, byte_length: 7 },  // "another"
            ChallengePosition { path_index: 0, byte_offset: 20, byte_length: 5 }, // "jumps" — same file as pos 0
        ];
        let (answers, bytes_read) = answer_challenge_from_dir_v3(&dir, &positions, &k).unwrap();
        assert_eq!(bytes_read, 11 + 7 + 5);
        assert_eq!(answers.len(), 3);

        // External recomputation: derive per_file_key from raw file_hash,
        // mutate the slice, hash. Must match what the V3 read path produced.
        for (i, pos) in positions.iter().enumerate() {
            let name = format!("f{:010}.bin", pos.path_index);
            let raw_file = std::fs::read(dir.join(&name)).unwrap();
            let file_hash: [u8; 32] = *blake3::hash(&raw_file).as_bytes();
            let pf_key = per_file_key_v3(&k, &file_hash);
            let slice = &raw_file[pos.byte_offset as usize ..
                                  (pos.byte_offset + pos.byte_length) as usize];
            let mutated = mutate_bytes_v3(&pf_key, pos.byte_offset, slice);
            let expected = b64(&challenge_hash_v3(&name, pos.byte_offset, pos.byte_length, &mutated, &k));
            assert_eq!(answers[i].challenge_hash, expected,
                "V3 read-path mismatch on position {i} ({pos:?})");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v3_to_canonical_bench_emits_k_echo_and_v3_digest() {
        let k = v3_test_k();
        let cb = to_canonical_bench_v3(
            "v3-mutate", "corpus-v3-quick", "quick", "run-V3",
            &[],
            &[vec![1u64, 2, 7]],
            true,
            &k,
        );
        // k_echo is base64-std of K (44 chars).
        let echo = cb.bench_proof.get("k_echo").and_then(|v| v.as_str())
            .expect("k_echo present in V3 bench_proof");
        assert_eq!(echo, b64(&k));
        // result_digest is V3 (distinct from V1 + V2 over same dupsets).
        let digest = cb.bench_proof.get("result_digest").and_then(|v| v.as_str()).unwrap();
        assert_eq!(digest, result_digest_v3(&[vec![1u64, 2, 7]], &k));
        assert_ne!(digest, result_digest(&[vec![1u64, 2, 7]]));
        // No V2 fields linger.
        assert!(cb.bench_proof.get("challenge_blob_echo").is_none(),
            "V3 bench_proof must not carry V2 challenge_blob_echo");
        assert_eq!(cb.protocol_version, "v3-mutate");
    }

    /// Emits the V3 GOLDEN VECTOR for web to byte-match (run:
    /// cargo test --features telemetry -- --nocapture print_v3_client_golden_vector).
    /// Inputs are fully hard-coded so a tag / endianness / framing bug between
    /// this Rust impl and web's TS impl localises immediately. Cross-stack
    /// lock procedure: web runs the V3 TS impl over identical inputs and
    /// asserts every line below matches. Asserts internal determinism here;
    /// the printed values go to web-superdeduper for cross-impl lock.
    #[test]
    fn print_v3_client_golden_vector() {
        let k: [u8; 32] = std::array::from_fn(|i| (0x80 + i) as u8);
        eprintln!("--- A3 / V3-MUTATE GOLDEN VECTOR (engine-side) ---");
        eprintln!("K = {} (b64-std of 0x80..0x9f)", b64(&k));

        // -- per_file_key_v3 --
        // file_hash = BLAKE3 of fixed raw content; derive per_file_key.
        let raw_a: &[u8] = b"alpha-content-for-v3-vector";
        let raw_b: &[u8] = b"beta-content-for-v3-vector";
        let fh_a: [u8; 32] = *blake3::hash(raw_a).as_bytes();
        let fh_b: [u8; 32] = *blake3::hash(raw_b).as_bytes();
        let pk_a = per_file_key_v3(&k, &fh_a);
        let pk_b = per_file_key_v3(&k, &fh_b);
        eprintln!("per_file_key_v3 = HMAC-SHA256(K, file_hash):");
        eprintln!("  file_hash(BLAKE3({:?})) -> per_file_key {}", "alpha-content-for-v3-vector", hex32(&pk_a));
        eprintln!("  file_hash(BLAKE3({:?})) -> per_file_key {}", "beta-content-for-v3-vector",  hex32(&pk_b));

        // -- mutate_bytes_v3 + challenge_hash_v3 --
        // mutated = raw XOR ChaCha20(pf_key, nonce=0, seek=offset).
        // challenge_hash_v3 = BLAKE3(0x04 || u32le(plen) || path || u64le(off) || u64le(len) || mutated || K).
        eprintln!("challenge_hash_v3 (tag 0x04 + K appended), std-base64:");
        let cases: [(&str, u64, &[u8]); 3] = [
            ("f0000000000.bin", 0, raw_a),
            ("f0000000007.bin", 11, raw_a),
            ("f0000000042.bin", 0, raw_b),
        ];
        for (path, off, raw) in cases {
            let fh: [u8; 32] = *blake3::hash(raw).as_bytes();
            let pf = per_file_key_v3(&k, &fh);
            let mutated = mutate_bytes_v3(&pf, off, raw);
            let h = challenge_hash_v3(path, off, raw.len() as u64, &mutated, &k);
            eprintln!("  path={path} off={off} len={} raw={:?} mutated={} -> {}",
                raw.len(),
                std::str::from_utf8(raw).unwrap(),
                hex32(&{
                    let mut arr = [0u8; 32];
                    for (i, b) in mutated.iter().take(32).enumerate() { arr[i] = *b; }
                    arr
                })[..2 * mutated.len().min(32)].to_string(),
                b64(&h));
        }

        // -- result_digest_v3 --
        // domain "tcorpus-result-v3" + K mixed before cluster data.
        let dupsets = vec![vec![0u64, 84000], vec![1, 84001], vec![2, 84002, 84003]];
        eprintln!("result_digest_v3 preimage = u32le(17)||\"tcorpus-result-v3\"||K||u64le(cluster_count)||(u64le(len)||u64le(pi)*)*");
        eprintln!("  canonical dupsets {dupsets:?}");
        eprintln!("  result_digest_v3 -> {}", result_digest_v3(&dupsets, &k));

        // Determinism self-check (printed vector must be stable).
        assert_eq!(result_digest_v3(&dupsets, &k), result_digest_v3(&dupsets, &k));
        let again = challenge_hash_v3("f0000000000.bin", 0, raw_a.len() as u64,
            &mutate_bytes_v3(&pk_a, 0, raw_a), &k);
        let twice = challenge_hash_v3("f0000000000.bin", 0, raw_a.len() as u64,
            &mutate_bytes_v3(&pk_a, 0, raw_a), &k);
        assert_eq!(again, twice);
    }
}
