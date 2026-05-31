//! V3.1 mutate-bench golden vectors — byte-exact cross-stack lock for
//! `rep_hash` and `result_digest_v3.1`. Authored by superdeduper-overflow
//! (2026-05-30) per the design-routed slice of Phase C V3.1 wire-format
//! work; spec at design-infosec.md (2026-05-30 22:51 PDT).
//!
//! Purpose: pin the cryptographic outputs (rep_hashes hex + result_digest
//! v3.1 hex) for ~20 deterministic test corpora so that engine, web verifier
//! (TS), and testrunner all agree byte-for-byte. Same role the locked V3
//! goldens play in src/leaderboard/bench.rs::tests, lifted up to V3.1.
//!
//! Design choices:
//! - SELF-CONTAINED REFERENCE IMPLEMENTATION in this file. Implements the
//!   V3.1 formulas directly from the spec text (repHashV31,
//!   computeResultDigestV31). Does NOT depend on any V3.1 primitives in
//!   src/leaderboard/ — that slice is owned by superdeduper's bench-loop
//!   work and lands in parallel. Once their bench-loop API ships, an
//!   additional test file will lock the engine impl against THESE
//!   goldens to close the loop.
//! - DATA-MODEL CHOICE: each vector specifies files as raw
//!   (path_index, content_id, size) tuples + the expected canonical
//!   partition. Skips the TS-side `params` abstraction (uniform / bimodal
//!   / class_cut) since the engine operates on (path, content_id, size)
//!   directly and the cryptographic outputs are determined solely by
//!   those + K + seed.
//! - LOCK PATTERN: each `expected_*_hex` starts as a tombstone string
//!   ("LOCK") for un-pinned vectors; the test prints the computed hex
//!   so the maintainer can paste it back as the golden, then re-runs to
//!   assert. Once locked, the tombstone is gone and the test asserts
//!   byte-exact every subsequent run.

#![cfg(feature = "telemetry")]

use blake3::Hasher;
use superdeduper::leaderboard::bench::{content_bytes_at, corpus_keys};
use superdeduper::leaderboard::bench_client::{mutate_bytes_v3, per_file_key_v3};

// ============================================================
// V3.1 REFERENCE PRIMITIVES (spec-derived, authoritative within
// this test file). When superdeduper's engine impl lands, the
// engine version will be cross-checked against these.
// ============================================================

/// V3.1 rep_hash domain tag. Distinct from 0x00 (Merkle leaf), 0x01
/// (Merkle node), 0x02/0x03/0x04 (V1/V2/V3 challenge_hash). Per spec
/// (design-infosec.md V3.1 section "SERVER-SIDE COMPUTE").
const REP_HASH_TAG_V31: u8 = 0x05;

/// V3.1 result_digest domain string. Same length-prefixed framing as
/// V1/V2/V3; the `.1` suffix makes it cryptographically distinct from
/// V3's `tcorpus-result-v3`.
const RESULT_DIGEST_DOMAIN_V31: &[u8] = b"tcorpus-result-v3.1";

/// V3.1 rep_hash per the spec:
///
/// ```text
/// repHashV31(per_file_key, rep_path_index, K, file_size, mutated_bytes):
///   BLAKE3(
///     u8(0x05) ||
///     u64le(rep_path_index) ||
///     u64le(file_size) ||
///     mutated_bytes ||
///     K
///   )
/// ```
///
/// Note: `per_file_key` appears in the spec signature for context
/// but is NOT in the digest body — the per-file-key contribution is
/// IMPLICIT in `mutated_bytes` (= canonical XOR keystream(per_file_key,
/// 0, size)). Keeping the parameter in the signature matches the spec
/// text verbatim so a future reviewer cross-checking against the spec
/// sees an identical call shape.
#[allow(clippy::too_many_arguments)]
fn rep_hash_v31(
    _per_file_key: &[u8; 32],
    rep_path_index: u64,
    k: &[u8; 32],
    file_size: u64,
    mutated_bytes: &[u8],
) -> [u8; 32] {
    assert_eq!(mutated_bytes.len() as u64, file_size, "mutated bytes must match file_size");
    let mut h = Hasher::new();
    h.update(&[REP_HASH_TAG_V31]);
    h.update(&rep_path_index.to_le_bytes());
    h.update(&file_size.to_le_bytes());
    h.update(mutated_bytes);
    h.update(k);
    *h.finalize().as_bytes()
}

/// V3.1 result_digest per the spec:
///
/// ```text
/// computeResultDigestV31(dupsets, rep_hashes_by_group, K):
///   groups = canonicalizeDupsets(dupsets)
///   wire = concat(
///     lenPrefixed("tcorpus-result-v3.1"),  // u32le(19) || domain
///     K,                                    // 32 bytes
///     u64le(groups.length),
///     for each (group, rep_hash) in canonical order:
///       u64le(32) || rep_hash || u64le(group.length) || u64le(member)* (asc)
///   )
///   BLAKE3(wire)
/// ```
///
/// `canonicalizeDupsets` sorts each group's members ascending, then
/// sorts groups by their minimum member — same canonical form V1/V2/V3
/// already use (matches `result_digest_bytes_v3` in bench_client.rs).
/// `reps_in_group_canonical_order[i]` MUST correspond to the i-th
/// canonical group.
fn result_digest_v31(
    dupsets: &[Vec<u64>],
    reps_in_group_canonical_order: &[[u8; 32]],
    k: &[u8; 32],
) -> [u8; 32] {
    let groups: Vec<Vec<u64>> = dupsets
        .iter()
        .map(|g| {
            let mut v = g.clone();
            v.sort_unstable();
            v
        })
        .collect();
    // Permutation that takes the input order to canonical order, so the
    // caller can pass reps_in_group_canonical_order indexed by the
    // ORIGINAL `dupsets` order and we sort both in lockstep.
    let mut perm: Vec<usize> = (0..groups.len()).collect();
    perm.sort_by_key(|&i| groups[i].first().copied().unwrap_or(0));
    let groups_sorted: Vec<&Vec<u64>> = perm.iter().map(|&i| &groups[i]).collect();
    let reps_sorted: Vec<&[u8; 32]> = perm.iter().map(|&i| &reps_in_group_canonical_order[i]).collect();

    assert_eq!(
        groups_sorted.len(),
        reps_sorted.len(),
        "rep count must match group count"
    );

    let mut h = Hasher::new();
    h.update(&(RESULT_DIGEST_DOMAIN_V31.len() as u32).to_le_bytes());
    h.update(RESULT_DIGEST_DOMAIN_V31);
    h.update(k);
    h.update(&(groups_sorted.len() as u64).to_le_bytes());
    for (group, rep) in groups_sorted.iter().zip(reps_sorted.iter()) {
        h.update(&32u64.to_le_bytes()); // rep_hash length (always 32 for BLAKE3)
        h.update(rep.as_slice());
        h.update(&(group.len() as u64).to_le_bytes());
        for &pi in group.iter() {
            h.update(&pi.to_le_bytes());
        }
    }
    *h.finalize().as_bytes()
}

fn hex32(b: &[u8; 32]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

// ============================================================
// Test corpora — engine-level data model.
//
// Each file is a (path_index, content_id, size) triple. Two files
// with the same content_id (and same size) are byte-identical →
// duplicate. `expected_dupsets` is the canonical partition; the
// reference impl computes rep_hashes from the partition + the
// file table.
// ============================================================

#[derive(Clone, Copy)]
struct FileSpec {
    path_index: u64,
    content_id: u64,
    size: u64,
}

struct V31Vector {
    name: &'static str,
    seed: [u8; 32],
    k: [u8; 32],
    files: &'static [FileSpec],
    /// Canonical partition: each inner slice is one dup group (or
    /// singleton). Groups MUST be in canonical order (sorted by
    /// min(group); members ascending). Singletons appear as
    /// 1-member groups per spec ("singletons being its own rep").
    expected_dupsets: &'static [&'static [u64]],
    /// One per dup group, canonical order. `None` (or "LOCK") means
    /// "not yet pinned — print + lock after first run".
    expected_rep_hashes_hex: &'static [&'static str],
    expected_result_digest_v31_hex: &'static str,
}

const SEED_ZEROS: [u8; 32] = [0u8; 32];
const SEED_ONES: [u8; 32] = [0xffu8; 32];
const K_0X11: [u8; 32] = [0x11u8; 32];
const K_ZEROS: [u8; 32] = [0u8; 32];
const K_ONES: [u8; 32] = [0xffu8; 32];

// VECTOR-1: 2-file uniform corpus, 1 dup group. The keystone — pins
// the spec's smallest end-to-end shape (one rep, two members, K_0x11,
// seed_zeros).
const VEC_01: V31Vector = V31Vector {
    name: "VECTOR-1 (2-file uniform, 1 group, size=64, K=0x11*32, seed=0)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 0, size: 64 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
    ],
    expected_result_digest_v31_hex:
        "3a35bceef8ab5af53b72b8b5fb21c0f54fce3f2d434197ff8b8c7466bde676de",
};

// VECTOR-2: empty corpus / 0 groups. Boundary case — spec defines
// behavior as result_digest over zero-group input = BLAKE3(prefix ||
// K || u64le(0)).
const VEC_02: V31Vector = V31Vector {
    name: "VECTOR-2 (empty corpus, 0 groups, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[],
    expected_dupsets: &[],
    expected_rep_hashes_hex: &[],
    expected_result_digest_v31_hex:
        "fb424c68ea03c11261dbd6791b4527f59878cb84d2e37ea2af00b3be472e6b7b",
};

// VECTOR-3: 4-file, 2 groups of 2 each. Tests group ordering + multi-
// rep wire layout.
const VEC_03: V31Vector = V31Vector {
    name: "VECTOR-3 (4-file, 2 groups of 2 each, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 1, size: 64 },
        FileSpec { path_index: 2, content_id: 0, size: 64 },
        FileSpec { path_index: 3, content_id: 1, size: 64 },
    ],
    expected_dupsets: &[&[0, 2], &[1, 3]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
        "c55da85c3590624317229358705e9d26a23a036220b7376a88a1adf97d9975fd",
    ],
    expected_result_digest_v31_hex:
        "f76535431e51b3cfc33cfcb7ad9538259499aea2c31be1ce74858e925d93441d",
};

// VECTOR-4: bimodal class with size > one ChaCha20 block (64 B).
// File size = 256 B = 4 ChaCha20 blocks → exercises multi-block
// keystream both for canonical content generation AND for the
// per-file V3 mutation keystream.
const VEC_04: V31Vector = V31Vector {
    name: "VECTOR-4 (2-file, size=256 spans multiple ChaCha20 blocks, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 256 },
        FileSpec { path_index: 1, content_id: 0, size: 256 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "0a1d75d41d8d8a5d14515c355d8f7851d4a4e0129be5386809d815b5bb11d994",
    ],
    expected_result_digest_v31_hex:
        "580ecf0c683698124d7f66fd2ee5f70df43ea1dfc0b9cbff6bc89ec9d6d20e93",
};

// VECTOR-5: single group of 3 members. Exercises groups of size != 2
// in the result_digest wire encoding.
const VEC_05: V31Vector = V31Vector {
    name: "VECTOR-5 (1 group of 3 members, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 128 },
        FileSpec { path_index: 1, content_id: 0, size: 128 },
        FileSpec { path_index: 2, content_id: 0, size: 128 },
    ],
    expected_dupsets: &[&[0, 1, 2]],
    expected_rep_hashes_hex: &[
        "d0747edb7077b3ddb53f524e4be8d7a49e694474a7ec03328ea1e8f63e281564",
    ],
    expected_result_digest_v31_hex:
        "3bc5a7155cb1dda624be742c84f0937685b4dfc1096d515f13e370373437c81d",
};

// VECTOR-6: mixed singletons + groups. Tests canonical ordering
// across non-uniform group sizes (singletons emit as 1-member
// groups with the singleton being its own rep).
const VEC_06: V31Vector = V31Vector {
    name: "VECTOR-6 (mixed: singleton at 0, group at [1,2], singleton at 3, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 1, size: 64 },
        FileSpec { path_index: 2, content_id: 1, size: 64 },
        FileSpec { path_index: 3, content_id: 2, size: 64 },
    ],
    expected_dupsets: &[&[0], &[1, 2], &[3]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
        "c55da85c3590624317229358705e9d26a23a036220b7376a88a1adf97d9975fd",
        "90e9846e8b7c47b34284200c26ed95b28335010f7db1df969c2ed50517adb1f4",
    ],
    expected_result_digest_v31_hex:
        "850fbf0fffa0499cecdf1b34a066df3cfdb78c40ce2982a78096aae56d053fb4",
};

// VECTOR-7: all-singletons corpus (3 distinct content_ids → 3
// 1-member groups). Forces every file to be its own rep.
const VEC_07: V31Vector = V31Vector {
    name: "VECTOR-7 (3 singletons, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 1, size: 64 },
        FileSpec { path_index: 2, content_id: 2, size: 64 },
    ],
    expected_dupsets: &[&[0], &[1], &[2]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
        "c55da85c3590624317229358705e9d26a23a036220b7376a88a1adf97d9975fd",
        "c4800f5c69461e49f1245b639f5706d81b6e97f5fb3ca3a2c674293b001025ad",
    ],
    expected_result_digest_v31_hex:
        "176f32ef0834961d272b97ee181f31d5aab730b407fdb2a95e8e67d4ef07f1e1",
};

// VECTOR-8: non-monotonic input group order. We declare the
// dupsets out of canonical order ([7, 0, 2]); the reference impl
// must canonicalize internally + still produce a stable digest.
const VEC_08: V31Vector = V31Vector {
    name: "VECTOR-8 (group [7,0,2] declared non-canonically, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 2, content_id: 0, size: 64 },
        FileSpec { path_index: 7, content_id: 0, size: 64 },
    ],
    // Declared in a non-canonical order — implementation MUST
    // sort members ascending. Rep = min = 0.
    expected_dupsets: &[&[7, 0, 2]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
    ],
    expected_result_digest_v31_hex:
        "2b084568e4cbdfef4b7354b03fb65e428204b3ee240eab15d5113e2d1cf5192b",
};

// VECTOR-9: group of 10 members. Stresses the wire encoding for a
// large group_len + many member path_indexes.
const VEC_09: V31Vector = V31Vector {
    name: "VECTOR-9 (1 group of 10 members, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 0, size: 64 },
        FileSpec { path_index: 2, content_id: 0, size: 64 },
        FileSpec { path_index: 3, content_id: 0, size: 64 },
        FileSpec { path_index: 4, content_id: 0, size: 64 },
        FileSpec { path_index: 5, content_id: 0, size: 64 },
        FileSpec { path_index: 6, content_id: 0, size: 64 },
        FileSpec { path_index: 7, content_id: 0, size: 64 },
        FileSpec { path_index: 8, content_id: 0, size: 64 },
        FileSpec { path_index: 9, content_id: 0, size: 64 },
    ],
    expected_dupsets: &[&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
    ],
    expected_result_digest_v31_hex:
        "a2a826122700a7f07d63e94c3b430190c0e2b2f8c234f310e45d0653bb411fea",
};

// VECTOR-10: two distinct content_ids with same size → distinct
// rep_hashes, distinct singletons. Negative control on content_id
// binding (same size + different content_id MUST NOT collide).
const VEC_10: V31Vector = V31Vector {
    name: "VECTOR-10 (2 singletons, distinct content_ids same size, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 100, size: 64 },
        FileSpec { path_index: 1, content_id: 200, size: 64 },
    ],
    expected_dupsets: &[&[0], &[1]],
    expected_rep_hashes_hex: &[
        "5703c8e15470c520826a7aa12176e3092e14d2c2423ddeddcf3be75e4513fa9a",
        "15e97c200d8afaf5a17a1fb7069164b86ef0f845fa2cd33249d947f49d44a67a",
    ],
    expected_result_digest_v31_hex:
        "df0d883a411a25a8ca893b960ec67f8c55f0083dd5023cbb1aea79dc70886fa2",
};

// VECTOR-11: same content_id, different sizes. The canonical
// content for (cid=0, size=64) differs from (cid=0, size=128)
// because file_size binds into mutated bytes + rep_hash. They
// hash differently → distinct singletons. Pins the file_size
// binding at the partition + rep_hash level.
const VEC_11: V31Vector = V31Vector {
    name: "VECTOR-11 (same content_id different sizes -> distinct singletons, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 0, size: 128 },
    ],
    expected_dupsets: &[&[0], &[1]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
        "ca0fe21460d7ba595e004ed41cb05a93b2f6c053aeca1a77fb85027f4f0a5507",
    ],
    expected_result_digest_v31_hex:
        "be104ee09f6833c7184104dc346f68884396741a6e0991da4fbcfd6700def317",
};

// VECTOR-12: K = all-zeros. Negative control for K binding —
// produces an entirely different digest from VEC-1 over the same
// corpus shape.
const VEC_12: V31Vector = V31Vector {
    name: "VECTOR-12 (VEC-1 shape but K=0x00*32)",
    seed: SEED_ZEROS,
    k: K_ZEROS,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 0, size: 64 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "fc9a1b7ef3ffcf73a2b1b198bd690dcb5d0ebd64fe96e9687b4b5d75f74d12db",
    ],
    expected_result_digest_v31_hex:
        "fa7559fe98bc74a89d7e0e7e7f75a14d2867733e442181ed4dc68666270e488f",
};

// VECTOR-13: K = all-ones. Same shape as VEC-1/12 with K=0xff*32
// — pins K binding over the full byte range.
const VEC_13: V31Vector = V31Vector {
    name: "VECTOR-13 (VEC-1 shape but K=0xff*32)",
    seed: SEED_ZEROS,
    k: K_ONES,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 0, size: 64 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "e3b4dd724ddd50a9633970ddfd600bfe5cc06e4db5e21f0289f18b5538a6dd94",
    ],
    expected_result_digest_v31_hex:
        "abca9bb04881f986cbaa844fec05e8535886be1ce294ff389f3619beb775f10e",
};

// VECTOR-14: file_size = 0 edge. Empty content → blake3(empty) ->
// well-defined file_hash → defined per_file_key → empty keystream
// XOR empty → empty mutated. rep_hash = BLAKE3(0x05 ||
// u64le(rep) || u64le(0) || (empty) || K).
const VEC_14: V31Vector = V31Vector {
    name: "VECTOR-14 (2 files size=0 -> 1 group, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 0 },
        FileSpec { path_index: 1, content_id: 0, size: 0 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "19cc261dd407f7963f8ee2036742ffb96406efdf5a05bf3f60cddeafaa212302",
    ],
    expected_result_digest_v31_hex:
        "1052527a2445f56d85c6654cbe2c945516a538d9993604b1e060a6b28d65317c",
};

// VECTOR-15: file_size = 1024 (16 ChaCha20 blocks). Exercises
// V3 mutation keystream across a multi-block window deeper than
// VEC-4.
const VEC_15: V31Vector = V31Vector {
    name: "VECTOR-15 (size=1024 multi-block, 1 group of 2, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 1024 },
        FileSpec { path_index: 1, content_id: 0, size: 1024 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "ea72ecd359cf67d670644450445af084be0db4bbf3e5aec8999334d14d3755b0",
    ],
    expected_result_digest_v31_hex:
        "47881cb95a9875674e6e946632b993256ea66b6c9687f3db8ce2c9da5bbe91d1",
};

// VECTOR-16: large rep (>= 1 MiB) — pins BLAKE3 chunked-mode +
// ChaCha20 keystream over a large window. Size = 1 MiB exactly
// (one corpus Merkle chunk boundary; also one BLAKE3 chunk
// boundary at 1024 bytes per internal chunk).
const VEC_16: V31Vector = V31Vector {
    name: "VECTOR-16 (1 MiB file, 1 group of 2, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 1 << 20 },
        FileSpec { path_index: 1, content_id: 0, size: 1 << 20 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "586df6879248a5c8155896b9f0af60fa030c009f8e12531598fc4803bf798817",
    ],
    expected_result_digest_v31_hex:
        "42b9c252c773379dfbe82b2f2e7ac88d9ccfe253418b79ac9e0346c84de80e57",
};

// VECTOR-17: groups appearing after singletons in path_index
// space. file_0 singleton, files 1+2 grouped, file 3 singleton.
// Sames-shape as VEC-6 but inserted to exercise the canonical-
// ordering tie-breaker rules under a tighter range.
const VEC_17: V31Vector = V31Vector {
    name: "VECTOR-17 (sing, group, sing — pi=[0],[1,2],[3], K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 10, size: 64 },
        FileSpec { path_index: 1, content_id: 20, size: 64 },
        FileSpec { path_index: 2, content_id: 20, size: 64 },
        FileSpec { path_index: 3, content_id: 30, size: 64 },
    ],
    expected_dupsets: &[&[0], &[1, 2], &[3]],
    expected_rep_hashes_hex: &[
        "dcdff5727ce97d9f2e882113ef453257e2ad382233bd5e3999b38dcd0e49a9bc",
        "395f38060f07fbc8344776952d99f888c4af36f11bc0910f7291afd50a495e1c",
        "b9ec6e792b2ff6de68dd922d64f0827ae2b8ac7974b0935a9354f911dd7d2b45",
    ],
    expected_result_digest_v31_hex:
        "23c1794120e1fd24021b4a5768fb0408301f0e8c00720e7d08c513702f34b47b",
};

// VECTOR-18: complex mixed corpus — 2 singletons + 2 groups
// + variable sizes (forces per-group size to vary across reps).
const VEC_18: V31Vector = V31Vector {
    name: "VECTOR-18 (2 singletons + 2 groups, mixed sizes, K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 1, size: 128 },
        FileSpec { path_index: 2, content_id: 2, size: 96 },
        FileSpec { path_index: 3, content_id: 0, size: 64 },
        FileSpec { path_index: 4, content_id: 1, size: 128 },
        FileSpec { path_index: 5, content_id: 3, size: 96 },
    ],
    expected_dupsets: &[&[0, 3], &[1, 4], &[2], &[5]],
    expected_rep_hashes_hex: &[
        "02608dbc1e36835203c263fbcb8e8edea297e25cff43c59ee55112752c99e4c8",
        "7619860e126386836fd13578eec0e70979edd7e49fe99cc6dc5ff125abe66ef0",
        "64c306474bdc261a6c79995dee4077426fd52ce202c07f93ef70891ad030b0a8",
        "7ca4c2550fd340ac3a5a5e5e64045263436198e20123e6954de918f60d01b567",
    ],
    expected_result_digest_v31_hex:
        "87f1405eeb12c773e242947b4d786a84d5b76b8af831ba04613c901a84c50278",
};

// VECTOR-19: seed = all-ones. Different seed → different
// K_content → different canonical content → different file_hash
// → different per_file_key → different mutated → different
// rep_hash. Pins the seed binding through the entire chain.
const VEC_19: V31Vector = V31Vector {
    name: "VECTOR-19 (VEC-1 shape but seed=0xff*32, K=0x11*32)",
    seed: SEED_ONES,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 0, content_id: 0, size: 64 },
        FileSpec { path_index: 1, content_id: 0, size: 64 },
    ],
    expected_dupsets: &[&[0, 1]],
    expected_rep_hashes_hex: &[
        "b00fdbfa60f24847647c4b9d1d8498be3de4ce53165fb1affd4be19e8a411abf",
    ],
    expected_result_digest_v31_hex:
        "e52e868e458c8e30f9487139a9349b6dde54fcf04e3a8aa5a08fd985bb51c446",
};

// VECTOR-20: rep_path_index != 0 (group does not start at 0).
// Pins that the rep_path_index encoded into the rep_hash body is
// the GROUP'S min path_index (not always 0). Critical: a forger
// who hardcodes rep_path_index=0 in their forged rep_hash will
// fail verification on this vector.
const VEC_20: V31Vector = V31Vector {
    name: "VECTOR-20 (rep_path_index != 0; group=[5,7,9], K=0x11*32)",
    seed: SEED_ZEROS,
    k: K_0X11,
    files: &[
        FileSpec { path_index: 5, content_id: 0, size: 64 },
        FileSpec { path_index: 7, content_id: 0, size: 64 },
        FileSpec { path_index: 9, content_id: 0, size: 64 },
    ],
    expected_dupsets: &[&[5, 7, 9]],
    expected_rep_hashes_hex: &[
        "b981319e22c7ececb16534d7144ce484a5193fd23191869ae1f438fd30d37924",
    ],
    expected_result_digest_v31_hex:
        "ca6befce397b10b2b62ac62a59fa2f78145475c8786fddf3a560ab33b7b81d51",
};

const ALL_VECTORS: &[&V31Vector] = &[
    &VEC_01, &VEC_02, &VEC_03, &VEC_04, &VEC_05, &VEC_06, &VEC_07, &VEC_08,
    &VEC_09, &VEC_10, &VEC_11, &VEC_12, &VEC_13, &VEC_14, &VEC_15, &VEC_16,
    &VEC_17, &VEC_18, &VEC_19, &VEC_20,
];

// ============================================================
// Reference engine: given a vector, compute the rep_hashes and
// result_digest. Pure-Rust port of the spec algorithm.
// ============================================================

fn compute_outputs(v: &V31Vector) -> (Vec<[u8; 32]>, [u8; 32]) {
    let (k_content, _k_control) = corpus_keys(&v.seed);

    // Step 1: canonical content per file (by content_id + size). Two
    // files with same content_id + same size produce identical bytes →
    // identical file_hash → identical mutated bytes → duplicate.
    let canonical_content: Vec<Vec<u8>> = v
        .files
        .iter()
        .map(|f| {
            let mut buf = vec![0u8; f.size as usize];
            content_bytes_at(&k_content, f.content_id, 0, &mut buf);
            buf
        })
        .collect();

    // Step 2: per-file mutated content (canonical XOR keystream at the
    // per_file_key derived from K + file_hash). Reuses the existing V3
    // primitives (per_file_key_v3 + mutate_bytes_v3) per the spec —
    // V3.1 doesn't change the mutation function, only adds rep_hash +
    // result_digest_v3.1 on top.
    let mutated_per_file: Vec<Vec<u8>> = canonical_content
        .iter()
        .map(|canon| {
            let file_hash = *blake3::hash(canon).as_bytes();
            let pf_key = per_file_key_v3(&v.k, &file_hash);
            mutate_bytes_v3(&pf_key, 0, canon)
        })
        .collect();

    // Step 3: rep_hash per dup group (rep = min path_index in group).
    // For result_digest input order we use the order `expected_dupsets`
    // is declared in (the function below resorts to canonical lockstep).
    let mut reps: Vec<[u8; 32]> = Vec::with_capacity(v.expected_dupsets.len());
    if v.files.is_empty() && v.expected_dupsets.is_empty() {
        // VECTOR-2 shape: empty corpus / 0 groups. Skip the per-group
        // rep computation; result_digest computes over the empty
        // vector (BLAKE3 of prefix || K || u64le(0)).
        let dupsets_owned: Vec<Vec<u64>> = Vec::new();
        let digest = result_digest_v31(&dupsets_owned, &reps, &v.k);
        return (reps, digest);
    }
    for group in v.expected_dupsets {
        let rep_path_index = *group.iter().min().expect("group must be non-empty");
        // Locate the FileSpec for rep_path_index.
        let rep_file = v
            .files
            .iter()
            .find(|f| f.path_index == rep_path_index)
            .expect("group rep must exist in files table");
        let rep_idx_in_files = v
            .files
            .iter()
            .position(|f| f.path_index == rep_path_index)
            .unwrap();
        let canon = &canonical_content[rep_idx_in_files];
        let file_hash = *blake3::hash(canon).as_bytes();
        let pf_key = per_file_key_v3(&v.k, &file_hash);
        let mutated = &mutated_per_file[rep_idx_in_files];
        let rep = rep_hash_v31(&pf_key, rep_path_index, &v.k, rep_file.size, mutated);
        reps.push(rep);
    }

    // Step 4: result_digest_v3.1 over (dupsets, reps, K).
    let dupsets_owned: Vec<Vec<u64>> = v
        .expected_dupsets
        .iter()
        .map(|g| g.to_vec())
        .collect();
    let digest = result_digest_v31(&dupsets_owned, &reps, &v.k);

    (reps, digest)
}

// ============================================================
// Tests
// ============================================================

/// One-shot for ALL declared vectors: compute rep_hashes + result_digest,
/// assert byte-exact against the locked-in hex (or print + fail for
/// any not yet locked, so the maintainer can paste the hex back).
#[test]
fn v31_goldens_locked_byte_exact() {
    let mut unlocked = Vec::new();
    for v in ALL_VECTORS {
        let (reps, digest) = compute_outputs(v);
        // Reps
        assert_eq!(
            reps.len(),
            v.expected_rep_hashes_hex.len(),
            "{}: rep count mismatch (computed {} vs expected {})",
            v.name,
            reps.len(),
            v.expected_rep_hashes_hex.len()
        );
        for (i, rep) in reps.iter().enumerate() {
            let actual = hex32(rep);
            let expected = v.expected_rep_hashes_hex[i];
            if expected == "LOCK" {
                eprintln!(
                    "[{}] rep_hash[{i}] (paste back into expected_rep_hashes_hex):\n  \"{}\"",
                    v.name, actual
                );
                unlocked.push(format!("{} rep_hash[{i}]", v.name));
            } else {
                assert_eq!(
                    &actual, expected,
                    "{}: rep_hash[{i}] byte-exact mismatch",
                    v.name
                );
            }
        }
        // Digest
        let actual_digest = hex32(&digest);
        if v.expected_result_digest_v31_hex == "LOCK" {
            eprintln!(
                "[{}] result_digest_v3.1 (paste back into expected_result_digest_v31_hex):\n  \"{}\"",
                v.name, actual_digest
            );
            unlocked.push(format!("{} result_digest", v.name));
        } else {
            assert_eq!(
                &actual_digest, v.expected_result_digest_v31_hex,
                "{}: result_digest_v3.1 byte-exact mismatch",
                v.name
            );
        }
    }
    if !unlocked.is_empty() {
        panic!(
            "{} unlocked golden value(s) -- see stderr output above for hex strings to paste back: {:?}",
            unlocked.len(),
            unlocked
        );
    }
}

// ============================================================
// Unit-level invariants for the V3.1 reference primitives themselves.
// These are independent of the corpus vectors above and pin properties
// the spec mandates (domain separation, K binding, determinism).
// ============================================================

#[test]
fn rep_hash_v31_is_deterministic() {
    let pfk = [0u8; 32];
    let k = K_0X11;
    let mutated = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let a = rep_hash_v31(&pfk, 0, &k, 8, &mutated);
    let b = rep_hash_v31(&pfk, 0, &k, 8, &mutated);
    assert_eq!(a, b);
}

#[test]
fn rep_hash_v31_binds_path_index() {
    let pfk = [0u8; 32];
    let k = K_0X11;
    let mutated = [0u8; 8];
    let h0 = rep_hash_v31(&pfk, 0, &k, 8, &mutated);
    let h1 = rep_hash_v31(&pfk, 1, &k, 8, &mutated);
    assert_ne!(h0, h1, "different rep_path_index must produce different rep_hash");
}

#[test]
fn rep_hash_v31_binds_file_size() {
    let pfk = [0u8; 32];
    let k = K_0X11;
    let mutated = [0u8; 8];
    let h8 = rep_hash_v31(&pfk, 0, &k, 8, &mutated);
    let mutated_alt = [0u8; 16];
    let h16 = rep_hash_v31(&pfk, 0, &k, 16, &mutated_alt);
    assert_ne!(h8, h16, "different file_size must produce different rep_hash");
}

#[test]
fn rep_hash_v31_binds_k() {
    let pfk = [0u8; 32];
    let mutated = [0u8; 8];
    let h_k1 = rep_hash_v31(&pfk, 0, &K_0X11, 8, &mutated);
    let h_k0 = rep_hash_v31(&pfk, 0, &K_ZEROS, 8, &mutated);
    assert_ne!(h_k1, h_k0, "different K must produce different rep_hash");
}

#[test]
fn rep_hash_v31_distinct_from_v3_tags() {
    // Domain-separation sanity: the V3.1 rep_hash uses tag 0x05.
    // Re-hashing the same input under tag 0x04 (V3 challenge_hash tag)
    // MUST produce a different digest.
    let mutated = [0u8; 8];
    let k = K_0X11;
    let rep = rep_hash_v31(&[0u8; 32], 0, &k, 8, &mutated);
    // Construct the same body shape with tag 0x04 instead.
    let mut h = Hasher::new();
    h.update(&[0x04]);
    h.update(&0u64.to_le_bytes());
    h.update(&8u64.to_le_bytes());
    h.update(&mutated);
    h.update(&k);
    let v3ish: [u8; 32] = *h.finalize().as_bytes();
    assert_ne!(rep, v3ish, "tag 0x05 must domain-separate from 0x04");
}

#[test]
fn result_digest_v31_binds_k() {
    let reps = vec![[0u8; 32], [1u8; 32]];
    let groups = vec![vec![0u64, 1], vec![2u64, 3]];
    let d_a = result_digest_v31(&groups, &reps, &K_0X11);
    let d_b = result_digest_v31(&groups, &reps, &K_ZEROS);
    assert_ne!(d_a, d_b, "different K must produce different result_digest_v3.1");
}

#[test]
fn result_digest_v31_binds_rep_hash() {
    let reps_a = vec![[0u8; 32]];
    let reps_b = vec![[1u8; 32]];
    let groups = vec![vec![0u64, 1]];
    let d_a = result_digest_v31(&groups, &reps_a, &K_0X11);
    let d_b = result_digest_v31(&groups, &reps_b, &K_0X11);
    assert_ne!(d_a, d_b, "different rep_hash must produce different result_digest");
}

#[test]
fn result_digest_v31_canonicalizes_group_order() {
    // Same partition declared in two different orders MUST produce
    // the same digest (canonical ordering by min member).
    let reps = vec![[7u8; 32], [3u8; 32]];
    // Original order: group [2,3] (rep 7), group [0,1] (rep 3)
    let groups_a = vec![vec![2u64, 3], vec![0u64, 1]];
    // Canonicalize → group [0,1] (rep 3), group [2,3] (rep 7).
    // Reorder caller arrays in the OTHER input direction:
    let groups_b = vec![vec![0u64, 1], vec![2u64, 3]];
    let reps_b = vec![[3u8; 32], [7u8; 32]];
    let d_a = result_digest_v31(&groups_a, &reps, &K_0X11);
    let d_b = result_digest_v31(&groups_b, &reps_b, &K_0X11);
    assert_eq!(d_a, d_b, "result_digest_v3.1 must be order-independent (canonicalized)");
}

/// Across all 20 vectors, distinct INPUT shapes must produce distinct
/// result_digests. A collision here would indicate either a spec
/// flaw (an input dimension not binding into the digest) or a
/// reference-impl bug. This is the same property the cross-stack
/// lock will rest on once engine + web + testrunner each implement
/// the formula independently.
#[test]
fn all_vector_digests_are_pairwise_distinct() {
    let digests: Vec<(String, [u8; 32])> = ALL_VECTORS
        .iter()
        .map(|v| {
            let (_reps, d) = compute_outputs(v);
            (v.name.to_string(), d)
        })
        .collect();
    for i in 0..digests.len() {
        for j in (i + 1)..digests.len() {
            assert_ne!(
                digests[i].1, digests[j].1,
                "vectors '{}' and '{}' produced identical result_digest_v3.1 — \
                 either the inputs shouldn't collide (spec gap) or the reference \
                 impl loses an input dimension (bug)",
                digests[i].0, digests[j].0,
            );
        }
    }
}

/// Within each vector, distinct rep_hashes — a single rep_hash
/// appearing twice in one vector would mean two distinct groups
/// produced the same rep, which only makes sense if they're literally
/// identical (same rep_path_index + same content + same K). In the
/// vector table that would always be a bug.
///
/// EXCEPTION: VECTOR-3, VECTOR-6, VECTOR-7, VECTOR-18 deliberately
/// reuse cid=0 / pi=0 / sz=64 as the first rep so they tie to VEC-1's
/// rep[0] — that's intentional cross-vector reuse, but WITHIN a single
/// vector the reps must still differ on (rep_path_index, content) so
/// no rep can repeat. This test catches if the reference impl
/// accidentally hashes the same input across two groups in one vector.
#[test]
fn within_vector_rep_hashes_have_no_unintended_collisions() {
    for v in ALL_VECTORS {
        let (reps, _) = compute_outputs(v);
        for i in 0..reps.len() {
            for j in (i + 1)..reps.len() {
                assert_ne!(
                    reps[i], reps[j],
                    "{}: rep[{i}] == rep[{j}] — two groups produced the same rep_hash",
                    v.name
                );
            }
        }
    }
}

#[test]
fn result_digest_v31_distinct_from_v3_domain() {
    // Domain-separation sanity: V3.1 domain `tcorpus-result-v3.1`
    // (19 bytes) vs V3 `tcorpus-result-v3` (17 bytes). Even if reps
    // happened to be empty, the prefixed length differs.
    let reps: Vec<[u8; 32]> = vec![];
    let groups: Vec<Vec<u64>> = vec![];
    let d_v31 = result_digest_v31(&groups, &reps, &K_0X11);
    // Build the V3 shape over the same empty input.
    let mut h = Hasher::new();
    let v3_domain = b"tcorpus-result-v3";
    h.update(&(v3_domain.len() as u32).to_le_bytes());
    h.update(v3_domain);
    h.update(&K_0X11);
    h.update(&0u64.to_le_bytes());
    let d_v3: [u8; 32] = *h.finalize().as_bytes();
    assert_ne!(d_v31, d_v3, "v3.1 domain must separate from v3 domain even on empty input");
}
