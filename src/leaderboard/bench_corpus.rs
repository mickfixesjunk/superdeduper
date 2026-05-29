//! T-BENCH-ME corpus generator (step 2) — the deterministic corpus-v1 layout
//! per research's B.3/B.4 PRODUCTION build contract (design 2026-05-29),
//! built on the FROZEN byte-exact primitives in [`super::bench`].
//!
//! Two layers, separated so the layout logic is unit-testable at any scale
//! without materializing gigabytes:
//!
//! * **PLAN** ([`plan_corpus`]) — pure + fast: per-file `(path_index,
//!   content_id, size)` descriptors in global path order, the
//!   `groundtruth_dupsets`, and the manifest aggregates. No I/O, no content.
//! * **MATERIALIZE** ([`compute_leaves`], [`write_corpus`],
//!   [`build_manifest`]) — generates ChaCha20 content on top of the plan to
//!   produce the Merkle leaves / on-disk corpus / signed-shape manifest.
//!
//! FIXED per-class sizes (B.3): small = 4096, medium = 1 MiB (1 leaf), large
//! = 256 MiB (256 leaves). content_id is GLOBAL sequential in path order
//! (small→medium→large, index-within); originals take `next_content_id++`,
//! dups inherit their source original's content_id (so content-identical dups
//! exist) — but each dup still gets a DISTINCT leaf hash because the path is
//! bound into the leaf preimage (see [`super::bench::leaf_hash`]).
#![cfg(feature = "telemetry")]

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use super::bench::{self, CHUNK_SIZE};

/// FIXED per-class byte sizes (B.3).
pub const SMALL_SIZE: u64 = 4096;
pub const MEDIUM_SIZE: u64 = 1 << 20; // 1 MiB — exactly 1 leaf
pub const LARGE_SIZE: u64 = 256 << 20; // 256 MiB — exactly 256 leaves

/// `generator_id` stamped into the manifest (engine impl + contract revision).
pub const GENERATOR_ID: &str = "superdeduper-bench-gen/corpus-v1";

/// Leaves contributed by one file of `size` bytes: `ceil(size / CHUNK_SIZE)`.
fn leaves_for_size(size: u64) -> u64 {
    size.div_ceil(CHUNK_SIZE)
}

/// One size class: the FIXED per-file size plus its dup-cluster shape (B.3
/// `(F, S, B, G)`). File-local indices `i ∈ [0, F)` are laid out as:
/// `[0, U)` originals · `[U, U+S)` size-2 dups · `[U+S, F)` big-cluster dups.
#[derive(Clone, Copy, Debug)]
pub struct SizeClassSpec {
    /// FIXED per-file byte size for this class.
    pub file_size: u64,
    /// `F` — total files in this class.
    pub file_count: u64,
    /// `S` — number of size-2 (pairwise) dup clusters.
    pub size2_clusters: u64,
    /// `B` — number of big dup clusters.
    pub big_clusters: u64,
    /// `G` — files per big cluster (1 original + `G-1` dups). Unused when `B == 0`.
    pub big_size: u64,
}

impl SizeClassSpec {
    /// `D = S + B·(G−1)` — duplicate files (full redundant copies).
    pub fn dup_count(&self) -> u64 {
        self.size2_clusters + self.big_clusters * self.big_size.saturating_sub(1)
    }
    /// `U = F − D` — original files; the first `U` file-local indices.
    pub fn unique_count(&self) -> u64 {
        self.file_count - self.dup_count()
    }
    /// Leaves contributed by this class: `F · ceil(file_size / CHUNK_SIZE)`.
    pub fn leaf_count(&self) -> u64 {
        self.file_count * leaves_for_size(self.file_size)
    }
    /// Reclaimable bytes: `D · file_size` (each dup is a full redundant copy).
    pub fn dup_bytes(&self) -> u64 {
        self.dup_count() * self.file_size
    }
    /// Total bytes: `F · file_size`.
    pub fn total_bytes(&self) -> u64 {
        self.file_count * self.file_size
    }
}

/// A complete tier (the three size classes, processed small→medium→large).
#[derive(Clone, Copy, Debug)]
pub struct TierSpec {
    pub corpus_version: &'static str,
    pub small: SizeClassSpec,
    pub medium: SizeClassSpec,
    pub large: SizeClassSpec,
}

impl TierSpec {
    fn classes(&self) -> [SizeClassSpec; 3] {
        [self.small, self.medium, self.large]
    }
    /// Total reclaimable fraction of bytes (D-bytes / total-bytes) — the
    /// headline number the boards quote.
    pub fn reclaimable_fraction(&self) -> f64 {
        let dup: u64 = self.classes().iter().map(|c| c.dup_bytes()).sum();
        let total: u64 = self.classes().iter().map(|c| c.total_bytes()).sum();
        dup as f64 / total as f64
    }
}

/// `corpus-sample` — a tiny cross-validation fixture (~34 MB, 1030 files)
/// that exercises EVERY content_id-assignment branch (originals, size-2 dups,
/// big-cluster dups) across two size classes, so an independent verifier
/// (e.g. web's TS `bench-corpus-gen.ts`) can byte-lock its layout port against
/// the engine generator BEFORE any large regen. Not a leaderboard tier — no
/// `large` (256 MiB) class to keep it small; the large-class cid logic is
/// byte-identical (only file_size differs).
pub fn sample_tier() -> TierSpec {
    TierSpec {
        corpus_version: "corpus-sample",
        small: SizeClassSpec { file_size: SMALL_SIZE, file_count: 1_000, size2_clusters: 200, big_clusters: 10, big_size: 5 },
        medium: SizeClassSpec { file_size: MEDIUM_SIZE, file_count: 30, size2_clusters: 5, big_clusters: 2, big_size: 3 },
        large: SizeClassSpec { file_size: LARGE_SIZE, file_count: 0, size2_clusters: 0, big_clusters: 0, big_size: 0 },
    }
}

/// `corpus-v1-quick` — the `--bench-me` default (~2.53 GB, ~26.8% reclaimable).
pub fn quick_tier() -> TierSpec {
    TierSpec {
        corpus_version: "corpus-v1-quick",
        small: SizeClassSpec { file_size: SMALL_SIZE, file_count: 120_000, size2_clusters: 35_000, big_clusters: 50, big_size: 21 },
        medium: SizeClassSpec { file_size: MEDIUM_SIZE, file_count: 1_700, size2_clusters: 500, big_clusters: 2, big_size: 6 },
        large: SizeClassSpec { file_size: LARGE_SIZE, file_count: 1, size2_clusters: 0, big_clusters: 0, big_size: 0 },
    }
}

/// `corpus-v2-full` — the COMPETITIVE Hall-of-Fame tier. design BLESSED
/// research's 3-CLASS composition (2026-05-29): ~1M x 4KiB small (the
/// syscall-bound forge invariant — 1M file-opens dominate the wall, which
/// in-memory recovery can't fake + makes it self-cold even on high-RAM rigs)
/// + ~2.1 GB medium/large for realism (mixed sizes = representative dedup).
/// ~6.25 GB total, ~29% reclaimable.
///
/// ALL three size classes are 4KiB-MULTIPLES (4KiB / 1 MiB / 256 MiB) so every
/// file is sector-aligned and the cold-enforce read-path (O_DIRECT /
/// NO_BUFFERING) cold-reads all of it — the "uniform 4K-multiple sizes" half of
/// the Windows cold-enforce fix (the engine read-path is the other half).
///
/// `corpus_version` is the generator's internal label; web sets the SERVED id.
/// Per-class dup constants are engine-proposed to unblock the launch critical
/// path (design-macro-aligned); research/design may tune them before the corpus
/// is finalized — only constants change, the layout logic never does.
pub fn full_tier() -> TierSpec {
    TierSpec {
        corpus_version: "corpus-v2-full",
        small: SizeClassSpec { file_size: SMALL_SIZE, file_count: 1_000_000, size2_clusters: 295_000, big_clusters: 250, big_size: 21 },
        medium: SizeClassSpec { file_size: MEDIUM_SIZE, file_count: 1_800, size2_clusters: 520, big_clusters: 3, big_size: 21 },
        large: SizeClassSpec { file_size: LARGE_SIZE, file_count: 1, size2_clusters: 0, big_clusters: 0, big_size: 0 },
    }
}

/// One planned corpus file (in global path order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePlan {
    /// Global path-lex position (small→medium→large, index-within).
    pub path_index: u64,
    /// GLOBAL sequential content id; dups share their original's id.
    pub content_id: u64,
    /// FIXED byte size of the file's class.
    pub size: u64,
}

impl FilePlan {
    /// Canonical relative path: `f{path_index:010}.bin` (path-lex == global order).
    pub fn path(&self) -> String {
        format!("f{:010}.bin", self.path_index)
    }
}

/// Per-class file counts for the manifest's `size_class_counts`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SizeClassCounts {
    pub small: u64,
    pub medium: u64,
    pub large: u64,
}

/// The pure layout plan: files (global path order), groundtruth dupsets, and
/// the manifest aggregates. Materialization layers on top.
#[derive(Clone, Debug)]
pub struct CorpusPlan {
    pub files: Vec<FilePlan>,
    /// Groundtruth: GLOBAL `path_index` lists, one per dup cluster
    /// (size-2 clusters first, then big clusters, per class in path order).
    /// Singletons are NOT listed.
    pub dupsets: Vec<Vec<u64>>,
    pub file_count: u64,
    pub leaf_count: u64,
    pub total_bytes: u64,
    pub size_class_counts: SizeClassCounts,
}

impl CorpusPlan {
    /// Bytes the size-grouped dedup actually READS: the sum of sizes of files
    /// that share their size with >=1 other file (a size-group of >=2). A
    /// unique-size file is never read (it can't be a duplicate). This is the
    /// throughput-numerator basis the leaderboard band MUST use — NOT
    /// `total_bytes`, which over-states (~2.5x on the multi-size corpora) and
    /// false-rejects honest runs (testrunner L-LEGIT finding). Equals the
    /// client's `run_shape.bytes_scanned` for an honest run.
    pub fn candidate_bytes(&self) -> u64 {
        let mut by_size: std::collections::HashMap<u64, (u64, u64)> = std::collections::HashMap::new();
        for f in &self.files {
            let e = by_size.entry(f.size).or_insert((0, 0));
            e.0 += 1;
            e.1 += f.size;
        }
        by_size.values().filter(|(count, _)| *count >= 2).map(|(_, bytes)| *bytes).sum()
    }
}

/// Build the pure layout plan for a tier. Single forward pass per class; every
/// dup references a strictly-lower (already-assigned) file-local index, so one
/// pass suffices. No I/O, no content generation — safe to call for any tier.
pub fn plan_corpus(spec: &TierSpec) -> CorpusPlan {
    let mut files: Vec<FilePlan> = Vec::new();
    let mut dupsets: Vec<Vec<u64>> = Vec::new();
    let mut gpi: u64 = 0; // global path index base for the current class
    let mut next_cid: u64 = 0; // global content_id counter (originals only)
    let mut leaf_count = 0u64;
    let mut total_bytes = 0u64;

    for class in spec.classes() {
        let f = class.file_count;
        if f == 0 {
            continue;
        }
        let s = class.size2_clusters;
        let b = class.big_clusters;
        let g = class.big_size;
        let u = class.unique_count();
        let size = class.file_size;

        // content_id for each file-local index, in path order.
        let mut cid_local = vec![0u64; f as usize];
        for i in 0..f {
            let cid = if i < u {
                // original: take the next global content_id.
                let c = next_cid;
                next_cid += 1;
                c
            } else if i < u + s {
                // size-2 dup: file (U + s_) copies original s_.
                cid_local[(i - u) as usize]
            } else {
                // big-cluster dup: cluster b_ (G-1 dups) copies original (S + b_).
                let j = i - (u + s);
                let b_ = j / (g - 1);
                cid_local[(s + b_) as usize]
            };
            cid_local[i as usize] = cid;
            files.push(FilePlan { path_index: gpi + i, content_id: cid, size });
        }

        // groundtruth dupsets (global path_index), non-singletons only.
        for s_ in 0..s {
            dupsets.push(vec![gpi + s_, gpi + u + s_]);
        }
        for b_ in 0..b {
            let mut set = Vec::with_capacity(g as usize);
            set.push(gpi + s + b_); // the original
            let base = u + s + b_ * (g - 1);
            for d in 0..(g - 1) {
                set.push(gpi + base + d); // the G-1 dups
            }
            dupsets.push(set);
        }

        leaf_count += class.leaf_count();
        total_bytes += class.total_bytes();
        gpi += f;
    }

    CorpusPlan {
        file_count: gpi,
        leaf_count,
        total_bytes,
        size_class_counts: SizeClassCounts {
            small: spec.small.file_count,
            medium: spec.medium.file_count,
            large: spec.large.file_count,
        },
        files,
        dupsets,
    }
}

impl CorpusPlan {
    /// The groundtruth reclaimable CEILING in bytes — the Dedup Efficiency
    /// denominator. Derived independently from `dupsets` (each cluster of `k`
    /// identical files can shed `k-1` redundant copies), NOT from the tier
    /// spec — so it cross-checks that the emitted dupsets encode exactly the
    /// reclaimable layout. All files in a cluster share a class size.
    ///
    /// `files` is in contiguous global path-index order (`files[i].path_index
    /// == i`), so a cluster's size is an O(1) index — not a linear scan (that
    /// would be O(dupsets × files), quadratic on the full tier).
    pub fn reclaimable_ceiling_bytes(&self) -> u64 {
        self.dupsets
            .iter()
            .map(|set| {
                let size = self.files.get(set[0] as usize).map(|f| f.size).unwrap_or(0);
                debug_assert_eq!(self.files[set[0] as usize].path_index, set[0], "files indexed by path_index");
                (set.len() as u64 - 1) * size
            })
            .sum()
    }
}

/// Dedup Efficiency = measured reclaim / groundtruth ceiling, clamped to
/// `[0, 1]` for display (a tool can never reclaim MORE than the ceiling; a
/// value above 1 indicates a measurement bug, so we cap rather than mislead).
/// `ceiling == 0` (a corpus with no dups) yields 0.0.
pub fn dedup_efficiency(measured_reclaim_bytes: u64, ceiling_bytes: u64) -> f64 {
    if ceiling_bytes == 0 {
        return 0.0;
    }
    (measured_reclaim_bytes as f64 / ceiling_bytes as f64).clamp(0.0, 1.0)
}

/// Compute every Merkle leaf over the planned corpus, in global path order
/// (content generated on the fly via the O(1) keystream — no disk). Heavy for
/// production tiers; this is the canonical input to the root + manifest.
pub fn compute_leaves(k_content: &[u8; 32], plan: &CorpusPlan) -> Vec<[u8; 32]> {
    let mut leaves = Vec::with_capacity(plan.leaf_count as usize);
    for fp in &plan.files {
        leaves.extend(bench::file_leaves(k_content, &fp.path(), fp.content_id, fp.size));
    }
    leaves
}

/// The §B.5 signed-shape manifest. The published corpus-v1 manifest is signed
/// at publish time; the engine regenerates locally and checks its root matches.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CorpusManifest {
    pub corpus_version: String,
    pub generator_id: String,
    /// 64-char lowercase hex of the 32-byte corpus seed.
    pub corpus_seed: String,
    pub chunk_size: u64,
    pub file_count: u64,
    pub leaf_count: u64,
    pub total_bytes: u64,
    /// std-base64 (padded) of the 32-byte Merkle root.
    pub merkle_root: String,
    pub groundtruth_dupsets: Vec<Vec<u64>>,
    pub size_class_counts: SizeClassCounts,
}

/// Build the manifest from a plan, materializing the leaves to derive the
/// root. SELF-VERIFY (§B.5 "self-verify root==manifest before emit"): the
/// materialized leaf count MUST equal the planned `leaf_count`, else the plan
/// and the bytes disagree — we panic rather than emit a manifest that lies.
pub fn build_manifest(spec: &TierSpec, seed: &[u8; 32], plan: &CorpusPlan, k_content: &[u8; 32]) -> CorpusManifest {
    let leaves = compute_leaves(k_content, plan);
    assert_eq!(
        leaves.len() as u64,
        plan.leaf_count,
        "self-verify: materialized leaf count must equal planned leaf_count"
    );
    let root = bench::merkle_root(&leaves).expect("non-empty corpus");
    CorpusManifest {
        corpus_version: spec.corpus_version.to_string(),
        generator_id: GENERATOR_ID.to_string(),
        corpus_seed: hex_lower(seed),
        chunk_size: CHUNK_SIZE,
        file_count: plan.file_count,
        leaf_count: plan.leaf_count,
        total_bytes: plan.total_bytes,
        merkle_root: bench::root_base64(&root),
        groundtruth_dupsets: plan.dupsets.clone(),
        size_class_counts: plan.size_class_counts,
    }
}

/// Number of challenge samples in a v1 bench proof.
pub const BENCH_SAMPLE_N: usize = 32;

/// Enumerate the on-disk corpus files (`f{path_index:010}.bin`) in global
/// path-lex order (== `path_index` order) with their byte sizes. The DOWNLOAD
/// flow (`--bench-me`) uses this: the client has the bytes on disk, not the
/// seed/plan. Non-corpus files (e.g. `manifest.json`) are skipped.
pub fn scan_corpus_dir(dir: &Path) -> std::io::Result<Vec<(u64, PathBuf, u64)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if let Some(pi) = parse_corpus_path_index(name) {
            let size = entry.metadata()?.len();
            files.push((pi, path, size));
        }
    }
    files.sort_by_key(|(pi, _, _)| *pi);
    Ok(files)
}

/// Build the bench proof from the DOWNLOADED corpus on disk — the seedless,
/// real-IO path for `--bench-me`. Reads every chunk from disk (the work-proof
/// AND the I/O signature web's gate requires), hashes the leaves, folds the
/// Merkle root, derives the BC-bound challenge positions, and emits each
/// sample with its audit path. Returns the proof plus `bytes_read` (the total
/// bytes pulled from disk — a no-IO submission has `bytes_read == 0`). The
/// leaf order + hashing match the seed-derived path, so the root equals what
/// the server (which holds the seed) computes.
pub fn build_bench_proof_from_dir(
    dir: &Path,
    bc: &bench::BenchContext,
    sample_n: usize,
) -> std::io::Result<(BenchProof, u64)> {
    let files = scan_corpus_dir(dir)?;
    let mut leaves: Vec<[u8; 32]> = Vec::new();
    let mut locs: Vec<(u64, u64, u64)> = Vec::new(); // (path_index, offset, len)
    let mut bytes_read = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE as usize];
    for (pi, path, size) in &files {
        let name = format!("f{pi:010}.bin");
        let mut f = std::fs::File::open(path)?;
        let mut off = 0u64;
        while off < *size {
            let len = ((*size - off).min(CHUNK_SIZE)) as usize;
            f.read_exact(&mut buf[..len])?; // real disk IO — the I/O signature
            leaves.push(bench::leaf_hash(&name, off, len as u64, &buf[..len]));
            locs.push((*pi, off, len as u64));
            off += len as u64;
            bytes_read += len as u64;
        }
    }
    let root = bench::merkle_root(&leaves).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "empty corpus directory")
    })?;
    let positions = bench::challenge_positions_from_bc(&bc.encode(), leaves.len() as u64, sample_n);
    let mut samples = Vec::with_capacity(positions.len());
    for pos in positions {
        let m = pos as usize;
        let path = bench::audit_path(m, &leaves);
        debug_assert_eq!(bench::root_from_path(leaves[m], &path, m, leaves.len()), root);
        let (path_index, byte_offset, byte_length) = locs[m];
        samples.push(SampleProof {
            leaf_index: pos,
            path_index,
            byte_offset,
            byte_length,
            leaf_hash: bench::root_base64(&leaves[m]),
            audit_path: path.iter().map(bench::root_base64).collect(),
        });
    }
    let proof = BenchProof {
        bench_run_id: bc.bench_run_id.to_string(),
        corpus_version: bc.corpus_version.to_string(),
        leaf_count: leaves.len() as u64,
        merkle_root: bench::root_base64(&root),
        samples,
    };
    Ok((proof, bytes_read))
}

// (The Merkle-based `build_canonical_bench` was removed when the model pivoted
// to server-direct-verify; the CanonicalBench is now assembled client-side in
// `bench_client::to_canonical_bench` from challenge answers + result_digest.)

/// The PUBLIC manifest the server serves to a client (work-proof spec): it
/// deliberately carries NO `merkle_root` and NO `groundtruth_dupsets`. The
/// server keeps those private and regenerates them to validate a submission;
/// the client must hash ALL leaves itself to compute the root (the proof of
/// work) and cannot copy a served answer. `manifest_hash = BLAKE3(served
/// bytes)` version-binds the BC to exactly this served manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServedManifest {
    pub corpus_version: String,
    pub generator_id: String,
    pub corpus_seed: String,
    pub chunk_size: u64,
    pub file_count: u64,
    pub leaf_count: u64,
    pub total_bytes: u64,
    /// Bytes the size-grouped dedup actually reads (size-group >=2). The
    /// leaderboard band's throughput numerator MUST use THIS, not total_bytes
    /// (which over-states + false-rejects). See `CorpusPlan::candidate_bytes`.
    pub candidate_bytes: u64,
    pub size_class_counts: SizeClassCounts,
}

/// Build the public served manifest from a plan — pure, NO leaf materialization
/// (it carries no root), so it is cheap even for the full tier.
pub fn served_manifest(spec: &TierSpec, seed: &[u8; 32], plan: &CorpusPlan) -> ServedManifest {
    ServedManifest {
        corpus_version: spec.corpus_version.to_string(),
        generator_id: GENERATOR_ID.to_string(),
        corpus_seed: hex_lower(seed),
        chunk_size: CHUNK_SIZE,
        file_count: plan.file_count,
        leaf_count: plan.leaf_count,
        total_bytes: plan.total_bytes,
        candidate_bytes: plan.candidate_bytes(),
        size_class_counts: plan.size_class_counts,
    }
}

/// Domain tag for the canonical binary manifest encoding M (research-pinned).
pub const MANIFEST_DOMAIN_V1: &[u8] = b"tcorpus-manifest-v1";

/// Build the canonical binary manifest encoding **M** — the `manifest_hash`
/// preimage. M is a FIXED binary layout (NOT the served JSON: hashing M
/// sidesteps all JSON-canonicalization drift, so client + server compute an
/// identical `manifest_hash` regardless of JSON formatting). The manifest is
/// still SERVED as JSON; the client reconstructs M from the JSON fields.
/// Layout (research, design-relayed 2026-05-29): `lp(domain) ‖ lp(protocol) ‖
/// lp(corpus_version) ‖ seed[32 raw] ‖ u32le(chunk_size) ‖ lp(tier) ‖
/// u64le(file_count) ‖ u64le(leaf_count) ‖ u64le(small) ‖ u64le(medium) ‖
/// u64le(large)`. NO root, NO groundtruth (server-private). `lp` =
/// `u32le(len) ‖ utf8`.
pub fn manifest_m(
    protocol_version: &str,
    corpus_version: &str,
    seed: &[u8; 32],
    chunk_size: u32,
    tier: &str,
    file_count: u64,
    leaf_count: u64,
    counts: SizeClassCounts,
) -> Vec<u8> {
    fn lp(out: &mut Vec<u8>, b: &[u8]) {
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    }
    let mut m = Vec::new();
    lp(&mut m, MANIFEST_DOMAIN_V1);
    lp(&mut m, protocol_version.as_bytes());
    lp(&mut m, corpus_version.as_bytes());
    m.extend_from_slice(seed); // 32 raw bytes, no length prefix
    m.extend_from_slice(&chunk_size.to_le_bytes());
    lp(&mut m, tier.as_bytes());
    m.extend_from_slice(&file_count.to_le_bytes());
    m.extend_from_slice(&leaf_count.to_le_bytes());
    m.extend_from_slice(&counts.small.to_le_bytes());
    m.extend_from_slice(&counts.medium.to_le_bytes());
    m.extend_from_slice(&counts.large.to_le_bytes());
    m
}

/// `manifest_hash = BLAKE3(M)` — the 32-byte digest fed (as raw bytes) into the
/// BC. Pass the bytes from [`manifest_m`]; both client and server hash the same
/// M, so positions match without any JSON-canon agreement.
pub fn manifest_hash(m: &[u8]) -> [u8; 32] {
    *blake3::hash(m).as_bytes()
}

/// Materialize the corpus to `dir` as `f{path_index:010}.bin` files (flat
/// layout). Returns the total bytes written. The `--bench-me` dedupe pass then
/// scans `dir`. Content is streamed a chunk at a time (bounded memory).
pub fn write_corpus(dir: &Path, k_content: &[u8; 32], plan: &CorpusPlan) -> std::io::Result<u64> {
    std::fs::create_dir_all(dir)?;
    let mut written = 0u64;
    let mut buf = vec![0u8; CHUNK_SIZE as usize];
    for fp in &plan.files {
        let mut file = std::fs::File::create(dir.join(fp.path()))?;
        let mut off = 0u64;
        while off < fp.size {
            let len = ((fp.size - off).min(CHUNK_SIZE)) as usize;
            let slice = &mut buf[..len];
            bench::content_bytes_at(k_content, fp.content_id, off, slice);
            file.write_all(slice)?;
            off += len as u64;
            written += len as u64;
        }
    }
    Ok(written)
}

/// The corpus location of one global Merkle leaf — which file it belongs to
/// and the byte window inside it. Parallels [`compute_leaves`] order exactly;
/// a verifier maps a challenged leaf index to its (file, offset, len) through
/// this (pure, no content generation).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeafLoc {
    pub global_leaf: u64,
    pub path_index: u64,
    pub content_id: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
}

/// Flat leaf→location map in global leaf order (matches [`compute_leaves`]).
pub fn leaf_locations(plan: &CorpusPlan) -> Vec<LeafLoc> {
    let mut locs = Vec::with_capacity(plan.leaf_count as usize);
    let mut g = 0u64;
    for fp in &plan.files {
        let mut off = 0u64;
        while off < fp.size {
            let len = (fp.size - off).min(CHUNK_SIZE);
            locs.push(LeafLoc { global_leaf: g, path_index: fp.path_index, content_id: fp.content_id, byte_offset: off, byte_length: len });
            g += 1;
            off += len;
        }
    }
    locs
}

/// One sampled leaf in a [`BenchProof`]: enough for a verifier to (a)
/// regenerate the chunk from the seed + plan and recompute the leaf hash, and
/// (b) reconstruct the Merkle root via the audit path. All hashes std-base64.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SampleProof {
    /// `m` — the challenged global leaf index.
    pub leaf_index: u64,
    pub path_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
    pub leaf_hash: String,
    /// RFC-6962 audit path, deepest sibling first.
    pub audit_path: Vec<String>,
}

/// The bench proof a `--bench-me` run submits: the client-computed root (the
/// work-proof — the canonical root is NEVER served, so a real run must hash
/// ALL leaves to produce it) plus the BC-challenged sample set. The proof hash
/// is BLAKE3 throughout (cryptographic); river5 is NEVER used here (river5
/// stays the internal dedupe content hash). Field shape adopted verbatim by
/// web's #160 verifier (samples / leaf_index / path_index / audit_path).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchProof {
    /// The run this proof is bound to (the BC's bench_run_id).
    pub bench_run_id: String,
    pub corpus_version: String,
    pub leaf_count: u64,
    pub merkle_root: String,
    pub samples: Vec<SampleProof>,
}

/// Build the bench proof for a run: materialize the leaves, derive the
/// challenged positions from the version-binding context `bc` (so the proof is
/// bound to this exact corpus build + run and cannot be replayed), and emit
/// each sample with its audit path. SELF-VERIFY: every sample's audit path is
/// checked to reconstruct the committed root before it is emitted (a proof
/// that fails its own verifier is never sent).
pub fn build_bench_proof(
    plan: &CorpusPlan,
    k_content: &[u8; 32],
    bc: &bench::BenchContext,
    sample_n: usize,
) -> BenchProof {
    let leaves = compute_leaves(k_content, plan);
    assert_eq!(leaves.len() as u64, plan.leaf_count, "self-verify: leaf count");
    let locs = leaf_locations(plan);
    let root = bench::merkle_root(&leaves).expect("non-empty corpus");
    let positions = bench::challenge_positions_from_bc(&bc.encode(), plan.leaf_count, sample_n);

    let mut samples = Vec::with_capacity(positions.len());
    for pos in positions {
        let m = pos as usize;
        let path = bench::audit_path(m, &leaves);
        assert_eq!(
            bench::root_from_path(leaves[m], &path, m, leaves.len()),
            root,
            "self-verify: sample {m} must reconstruct the committed root"
        );
        let loc = &locs[m];
        samples.push(SampleProof {
            leaf_index: pos,
            path_index: loc.path_index,
            byte_offset: loc.byte_offset,
            byte_length: loc.byte_length,
            leaf_hash: bench::root_base64(&leaves[m]),
            audit_path: path.iter().map(bench::root_base64).collect(),
        });
    }
    BenchProof {
        bench_run_id: bc.bench_run_id.to_string(),
        corpus_version: bc.corpus_version.to_string(),
        leaf_count: plan.leaf_count,
        merkle_root: bench::root_base64(&root),
        samples,
    }
}

/// Parse a corpus file path or name `f{path_index:010}.bin` → `path_index`.
/// Accepts a full path (uses the final component) or a bare name; returns
/// `None` for anything that isn't a corpus file.
pub fn parse_corpus_path_index(path: &str) -> Option<u64> {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.strip_prefix('f')?.strip_suffix(".bin")?.parse::<u64>().ok()
}

/// Convert the dedupe's found duplicate groups (each a list of corpus file
/// paths) into the canonical `client_found_dupsets` for the submission:
/// `path_index` lists, sorted within each group and groups sorted, so web can
/// exact-compare against its server-regenerated groundtruth (client ==
/// groundtruth, zero false-positives). Groups with fewer than 2 parseable
/// corpus members are dropped (a duplicate set needs ≥2 files).
pub fn client_found_dupsets(groups: &[Vec<String>]) -> Vec<Vec<u64>> {
    let mut out: Vec<Vec<u64>> = groups
        .iter()
        .filter_map(|g| {
            let mut idxs: Vec<u64> = g.iter().filter_map(|p| parse_corpus_path_index(p)).collect();
            idxs.sort_unstable();
            idxs.dedup();
            (idxs.len() >= 2).then_some(idxs)
        })
        .collect();
    out.sort_unstable();
    out
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut s = String::with_capacity(64);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny tier (sizes < 1 MiB so leaves are cheap) exercising every
    /// placement branch: size-2 dups, a big cluster, and singletons.
    /// small: F=10 S=2 B=1 G=3 (U=6); medium: F=3 S=1 (U=2); large: empty.
    fn tiny_tier() -> TierSpec {
        TierSpec {
            corpus_version: "corpus-tiny-test",
            small: SizeClassSpec { file_size: 64, file_count: 10, size2_clusters: 2, big_clusters: 1, big_size: 3 },
            medium: SizeClassSpec { file_size: 128, file_count: 3, size2_clusters: 1, big_clusters: 0, big_size: 0 },
            large: SizeClassSpec { file_size: 0, file_count: 0, size2_clusters: 0, big_clusters: 0, big_size: 0 },
        }
    }

    #[test]
    fn quick_tier_matches_published_aggregates() {
        let plan = plan_corpus(&quick_tier());
        assert_eq!(plan.file_count, 121_701, "quick file_count");
        assert_eq!(plan.leaf_count, 121_956, "quick leaf_count");
        assert_eq!(plan.total_bytes, 2_542_534_656, "quick total_bytes (~2.53 GB)");
        assert_eq!(plan.size_class_counts, SizeClassCounts { small: 120_000, medium: 1_700, large: 1 });
        // dup files D = S + B(G-1): small 36000, medium 510, large 0.
        let dups: u64 = plan.dupsets.iter().map(|s| s.len() as u64 - 1).sum();
        assert_eq!(dups, 36_000 + 510, "quick duplicate-file count");
        let frac = quick_tier().reclaimable_fraction();
        assert!((0.267..0.269).contains(&frac), "quick reclaimable ~26.8%, got {frac}");
    }

    #[test]
    fn sample_tier_xval_fixture_is_pinned() {
        // The cross-validation fixture web byte-locks its TS layout port
        // against. If these aggregates drift, web's pinned merkle_root +
        // groundtruth break — so pin them here as a regression guard.
        let plan = plan_corpus(&sample_tier());
        assert_eq!(plan.file_count, 1_030, "sample file_count");
        assert_eq!(plan.size_class_counts, SizeClassCounts { small: 1_000, medium: 30, large: 0 });
        assert_eq!(plan.total_bytes, 35_553_280, "sample total_bytes (~34 MB)");
        // dups: small 200 + 10*(5-1)=240 ; medium 5 + 2*(3-1)=9 ; total 249.
        let dups: u64 = plan.dupsets.iter().map(|s| s.len() as u64 - 1).sum();
        assert_eq!(dups, 240 + 9, "sample duplicate-file count");
        // 217 non-singleton dup groups (small 200+10, medium 5+2).
        assert_eq!(plan.dupsets.len(), 200 + 10 + 5 + 2, "sample dup-group count");
        // Exercises all branches across two classes for the verifier port.
        assert_eq!(plan.dupsets[0], vec![0, 760], "first size-2 set = {{original 0, dup at u=760}}");
    }

    #[test]
    fn full_tier_matches_published_aggregates() {
        // corpus-v2-full: research's 3-class (1M x4KiB small + 1800 x1MiB
        // medium + 1 x256MiB large), all 4KiB-aligned. ~6.25 GB, ~29%.
        let plan = plan_corpus(&full_tier());
        assert_eq!(plan.file_count, 1_000_000 + 1_800 + 1, "full file_count");
        assert_eq!(plan.size_class_counts, SizeClassCounts { small: 1_000_000, medium: 1_800, large: 1 });
        // leaves: small 1 each, medium 1 each (1MiB), large 256 (256MiB).
        assert_eq!(plan.leaf_count, 1_000_000 + 1_800 + 256, "full leaf_count");
        assert_eq!(plan.total_bytes, 6_251_872_256, "full total_bytes (~6.25 GB)");
        let dups: u64 = plan.dupsets.iter().map(|s| s.len() as u64 - 1).sum();
        // small: 295_000 + 250*20 ; medium: 520 + 3*20 ; large: 0.
        assert_eq!(dups, (295_000 + 250 * 20) + (520 + 3 * 20), "full duplicate-file count");
        let frac = full_tier().reclaimable_fraction();
        assert!((0.29..0.30).contains(&frac), "full reclaimable ~29%, got {frac}");
        // candidate-bytes = small(1M x4KiB) + medium(1800 x1MiB); the lone
        // large (unique size) is PRUNED. This is the band's throughput
        // numerator (NOT total_bytes). Must match the server constant 5.98GB.
        assert_eq!(plan.candidate_bytes(), 4_096_000_000 + 1_887_436_800, "full candidate_bytes (5.98GB, large pruned)");
    }

    #[test]
    fn plan_dup_layout_invariants() {
        let plan = plan_corpus(&tiny_tier());
        assert_eq!(plan.file_count, 13);
        assert_eq!(plan.leaf_count, 13, "all tiny files are <1 leaf each");
        assert_eq!(plan.total_bytes, 10 * 64 + 3 * 128);

        // content_ids are global-sequential & contiguous over the originals
        // (8 originals: 6 small + 2 medium), assigned in path order.
        let max_cid = plan.files.iter().map(|f| f.content_id).max().unwrap();
        assert_eq!(max_cid, 7, "8 distinct originals -> content_ids 0..=7");
        for (expected, fp) in plan.files.iter().filter(|f| is_original(&plan, f.path_index)).enumerate() {
            assert_eq!(fp.content_id, expected as u64, "originals take next_content_id++ in path order");
        }

        // expected dupsets (global path_index): small {0,6} {1,7} {2,8,9}; medium {10,12}.
        assert_eq!(plan.dupsets, vec![vec![0, 6], vec![1, 7], vec![2, 8, 9], vec![10, 12]]);

        // every dup file's content_id equals its source original's content_id.
        for set in &plan.dupsets {
            let orig_cid = cid_of(&plan, set[0]);
            for &pi in &set[1..] {
                assert_eq!(cid_of(&plan, pi), orig_cid, "dup {pi} must inherit original {}'s content_id", set[0]);
            }
        }
        // singletons (small 3,4,5; medium 11) appear in no dupset.
        let in_set: std::collections::HashSet<u64> = plan.dupsets.iter().flatten().copied().collect();
        for pi in [3u64, 4, 5, 11] {
            assert!(!in_set.contains(&pi), "path_index {pi} is a singleton, must not be in any dupset");
        }
    }

    #[test]
    fn dup_files_have_byte_identical_content_distinct_leaves() {
        let (kc, _) = bench::corpus_keys(&[3u8; 32]);
        let plan = plan_corpus(&tiny_tier());
        // {2,8,9} is the big cluster: same content, but path-bound leaves differ.
        let f2 = file_at(&plan, 2);
        let f8 = file_at(&plan, 8);
        let mut c2 = vec![0u8; f2.size as usize];
        let mut c8 = vec![0u8; f8.size as usize];
        bench::content_bytes_at(&kc, f2.content_id, 0, &mut c2);
        bench::content_bytes_at(&kc, f8.content_id, 0, &mut c8);
        assert_eq!(c2, c8, "exact-dup files share byte-identical content");
        let l2 = bench::file_leaves(&kc, &f2.path(), f2.content_id, f2.size);
        let l8 = bench::file_leaves(&kc, &f8.path(), f8.content_id, f8.size);
        assert_ne!(l2, l8, "but their leaf hashes differ (path bound into the leaf)");
    }

    #[test]
    fn manifest_self_verifies_and_roundtrips() {
        let seed = [0x5Au8; 32];
        let (kc, _) = bench::corpus_keys(&seed);
        let spec = tiny_tier();
        let plan = plan_corpus(&spec);
        let m = build_manifest(&spec, &seed, &plan, &kc); // panics if root != plan
        assert_eq!(m.chunk_size, CHUNK_SIZE);
        assert_eq!(m.file_count, 13);
        assert_eq!(m.leaf_count, 13);
        assert_eq!(m.corpus_seed.len(), 64, "seed is 64-char hex");
        assert_eq!(m.merkle_root.len(), 44, "root is 44-char padded std-base64");
        assert_eq!(m.size_class_counts, SizeClassCounts { small: 10, medium: 3, large: 0 });
        // deterministic + serde round-trips through canonical JSON.
        let m2 = build_manifest(&spec, &seed, &plan, &kc);
        assert_eq!(m, m2, "manifest is deterministic");
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"corpus_version\":\"corpus-tiny-test\""));
        assert!(json.contains(&format!("\"merkle_root\":\"{}\"", m.merkle_root)));
    }

    #[test]
    fn write_corpus_roundtrips_on_disk_bytes() {
        let seed = [0x11u8; 32];
        let (kc, _) = bench::corpus_keys(&seed);
        let plan = plan_corpus(&tiny_tier());
        let dir = std::env::temp_dir().join(format!("sd-bench-corpus-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = write_corpus(&dir, &kc, &plan).expect("write corpus");
        assert_eq!(written, plan.total_bytes, "bytes written == planned total");

        // a written file's on-disk bytes reproduce its planned leaves exactly.
        let f9 = file_at(&plan, 9);
        let on_disk = std::fs::read(dir.join(f9.path())).expect("read f9");
        assert_eq!(on_disk.len() as u64, f9.size);
        let disk_leaf = bench::leaf_hash(&f9.path(), 0, f9.size, &on_disk);
        let planned_leaf = bench::file_leaves(&kc, &f9.path(), f9.content_id, f9.size)[0];
        assert_eq!(disk_leaf, planned_leaf, "on-disk content reproduces the planned leaf");

        // dup {2,8,9} files are byte-identical on disk.
        let b2 = std::fs::read(dir.join(file_at(&plan, 2).path())).unwrap();
        let b8 = std::fs::read(dir.join(file_at(&plan, 8).path())).unwrap();
        assert_eq!(b2, b8, "exact-dup files are byte-identical on disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reclaimable_ceiling_cross_checks_spec_and_drives_efficiency() {
        // The dupset-derived ceiling must equal the spec-derived dup_bytes —
        // proving the emitted groundtruth encodes exactly the reclaimable layout.
        for spec in [quick_tier(), full_tier()] {
            let plan = plan_corpus(&spec);
            let spec_dup_bytes: u64 = [spec.small, spec.medium, spec.large].iter().map(|c| c.dup_bytes()).sum();
            assert_eq!(
                plan.reclaimable_ceiling_bytes(),
                spec_dup_bytes,
                "{}: dupset-derived ceiling must equal spec dup_bytes",
                spec.corpus_version
            );
        }
        let plan = plan_corpus(&tiny_tier());
        let ceiling = plan.reclaimable_ceiling_bytes();
        // tiny: small dups 4 files (2 size-2 + 2 big-dups) @64 + medium 1 dup @128.
        assert_eq!(ceiling, 4 * 64 + 1 * 128);
        // Dedup Efficiency: a perfect tool reclaims the whole ceiling -> 1.0;
        // half -> 0.5; over-claim is capped; zero ceiling -> 0.0.
        assert_eq!(dedup_efficiency(ceiling, ceiling), 1.0);
        assert!((dedup_efficiency(ceiling / 2, ceiling) - 0.5).abs() < 0.02);
        assert_eq!(dedup_efficiency(ceiling * 2, ceiling), 1.0, "over-claim capped at 1.0");
        assert_eq!(dedup_efficiency(100, 0), 0.0, "no-dup corpus -> 0.0");
    }

    #[test]
    fn parse_corpus_path_index_handles_paths_and_rejects_noise() {
        assert_eq!(parse_corpus_path_index("f0000000012.bin"), Some(12));
        assert_eq!(parse_corpus_path_index("/tmp/corpus/f0000000007.bin"), Some(7));
        assert_eq!(parse_corpus_path_index(r"C:\corpus\f0000000123.bin"), Some(123));
        assert_eq!(parse_corpus_path_index("f0000000000.bin"), Some(0));
        assert_eq!(parse_corpus_path_index("notes.txt"), None);
        assert_eq!(parse_corpus_path_index("f00.png"), None);
        assert_eq!(parse_corpus_path_index("fXYZ.bin"), None);
    }

    #[test]
    fn client_found_dupsets_perfect_dedupe_equals_groundtruth() {
        // A perfect dedupe finds exactly the groundtruth clusters; rendered as
        // corpus paths then parsed back, client_found_dupsets must equal the
        // groundtruth (canonically sorted) — web's client==groundtruth gate.
        let plan = plan_corpus(&tiny_tier());
        let groups: Vec<Vec<String>> = plan
            .dupsets
            .iter()
            .map(|set| set.iter().map(|&pi| format!("/corpus/f{pi:010}.bin")).collect())
            .collect();
        let mut expected = plan.dupsets.clone();
        for g in &mut expected {
            g.sort_unstable();
        }
        expected.sort_unstable();
        assert_eq!(client_found_dupsets(&groups), expected, "perfect dedupe == groundtruth");

        // a group with a non-corpus file still parses its corpus members;
        // a singleton / unparseable group is dropped.
        let noisy = vec![
            vec!["/c/f0000000001.bin".into(), "/c/f0000000006.bin".into(), "/c/readme.txt".into()],
            vec!["/c/f0000000003.bin".into()], // singleton -> dropped
            vec!["/c/notes.md".into(), "/c/other.md".into()], // no corpus files -> dropped
        ];
        assert_eq!(client_found_dupsets(&noisy), vec![vec![1u64, 6]]);
    }

    #[test]
    fn manifest_m_matches_research_golden_anchor() {
        // research's M golden anchor (design-relayed): corpus-v1-quick fields
        // with EXAMPLE seed 0x20..0x3f -> BLAKE3(M) == the published hash.
        // Reproducing it byte-for-byte locks my M serializer against web's.
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = 0x20 + i as u8;
        }
        let m = manifest_m(
            "tbench-1",
            "corpus-v1-quick",
            &seed,
            1_048_576,
            "quick",
            121_701,
            121_956,
            SizeClassCounts { small: 120_000, medium: 1_700, large: 1 },
        );
        assert_eq!(
            hex_lower(&manifest_hash(&m)),
            "c4138dbebc1d9180b8d684d175c3d201acf9102133015afdeef8f668754c0246",
            "manifest_hash = BLAKE3(M) must match research's golden anchor"
        );
    }

    #[test]
    fn served_manifest_excludes_root_and_groundtruth() {
        let seed = [0x33u8; 32];
        let spec = tiny_tier();
        let plan = plan_corpus(&spec);
        let served = served_manifest(&spec, &seed, &plan);
        // carries the public aggregates...
        assert_eq!(served.leaf_count, plan.leaf_count);
        assert_eq!(served.file_count, plan.file_count);
        assert_eq!(served.total_bytes, plan.total_bytes);
        assert_eq!(served.chunk_size, CHUNK_SIZE);
        // ...but the serialized form leaks NEITHER the root NOR the groundtruth
        // (the work-proof invariant: client must compute the root itself).
        let json = serde_json::to_string(&served).unwrap();
        assert!(!json.contains("merkle_root"), "served manifest must not carry the root");
        assert!(!json.contains("groundtruth"), "served manifest must not carry groundtruth");
        // manifest_hash is BLAKE3 of the served bytes, deterministic.
        let h1 = manifest_hash(json.as_bytes());
        let h2 = manifest_hash(serde_json::to_string(&served_manifest(&spec, &seed, &plan)).unwrap().as_bytes());
        assert_eq!(h1, h2, "manifest_hash deterministic over served bytes");
        // a different seed -> different served manifest -> different hash.
        let other = served_manifest(&spec, &[0x44u8; 32], &plan);
        assert_ne!(h1, manifest_hash(serde_json::to_string(&other).unwrap().as_bytes()));
    }

    #[test]
    fn disk_proof_equals_seed_proof_and_records_io() {
        // The DOWNLOAD flow: the server generates the corpus from the private
        // seed; the client downloads the bytes and builds the proof from DISK
        // (no seed). The disk-derived root MUST equal the seed-derived root, so
        // the server's verifier (which holds the seed) accepts the client proof.
        let seed = [0x6Cu8; 32];
        let (kc, _) = bench::corpus_keys(&seed);
        let spec = tiny_tier();
        let plan = plan_corpus(&spec);
        let dir = std::env::temp_dir().join(format!("sd-bench-dlproof-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let written = write_corpus(&dir, &kc, &plan).expect("write corpus"); // server side

        let mhash = manifest_hash(&manifest_m(
            "tbench-1", spec.corpus_version, &seed, CHUNK_SIZE as u32, "tiny",
            plan.file_count, plan.leaf_count, plan.size_class_counts,
        ));
        let bc = bench::BenchContext {
            protocol_version: "tbench-1",
            corpus_version: spec.corpus_version,
            manifest_hash: mhash,
            install_id: "i",
            bench_run_id: "run-dl",
            tier: "tiny",
            leaf_count: plan.leaf_count,
            chunk_size: CHUNK_SIZE as u32,
        };

        // seed-derived (server / golden) proof vs disk-derived (client) proof.
        let seed_proof = build_bench_proof(&plan, &kc, &bc, 32);
        let (disk_proof, bytes_read) = build_bench_proof_from_dir(&dir, &bc, 32).expect("disk proof");

        assert_eq!(disk_proof.merkle_root, seed_proof.merkle_root, "disk root == seed root");
        assert_eq!(disk_proof.leaf_count, seed_proof.leaf_count);
        assert_eq!(disk_proof, seed_proof, "full proof identical (same challenge → same samples)");
        // I/O signature: the client actually read every byte from disk.
        assert_eq!(bytes_read, written, "bytes_read == corpus bytes on disk (real IO)");
        assert_eq!(bytes_read, plan.total_bytes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leaf_locations_parallel_compute_leaves() {
        let plan = plan_corpus(&tiny_tier());
        let locs = leaf_locations(&plan);
        assert_eq!(locs.len() as u64, plan.leaf_count);
        // global leaf indices are contiguous from 0.
        for (i, loc) in locs.iter().enumerate() {
            assert_eq!(loc.global_leaf, i as u64);
        }
        // each loc's content_id matches its file's plan entry.
        for loc in &locs {
            assert_eq!(loc.content_id, file_at(&plan, loc.path_index).content_id);
        }
    }

    #[test]
    fn bench_proof_is_independently_web_verifiable() {
        // The load-bearing interop test: a verifier holding ONLY the seed, the
        // (deterministic) plan, and the proof — never the engine's in-memory
        // leaves — regenerates each sampled chunk, recomputes the leaf, and
        // reconstructs the root. This is exactly what web's #160 verifier does.
        use base64::Engine;
        let seed = [0x77u8; 32];
        let (kc, _) = bench::corpus_keys(&seed);
        let spec = tiny_tier();
        let plan = plan_corpus(&spec);
        // BC binds the proof to this corpus build + run (manifest_hash = BLAKE3(M)).
        let mhash = manifest_hash(&manifest_m(
            "tbench-1", spec.corpus_version, &seed, CHUNK_SIZE as u32, "tiny",
            plan.file_count, plan.leaf_count, plan.size_class_counts,
        ));
        let bc = bench::BenchContext {
            protocol_version: "tcorpus-1",
            corpus_version: spec.corpus_version,
            manifest_hash: mhash,
            install_id: "install-test",
            bench_run_id: "run-tiny-1",
            tier: "tiny",
            leaf_count: plan.leaf_count,
            chunk_size: CHUNK_SIZE as u32,
        };
        let proof = build_bench_proof(&plan, &kc, &bc, 32);
        assert_eq!(proof.bench_run_id, "run-tiny-1");

        // sample_n (32) capped at leaf_count (13); positions distinct & in range.
        assert_eq!(proof.samples.len(), 13);
        assert_eq!(proof.leaf_count, 13);
        let mut seen = std::collections::HashSet::new();
        for sm in &proof.samples {
            assert!(sm.leaf_index < 13 && seen.insert(sm.leaf_index), "distinct in-range positions");
        }

        let dec = |s: &str| -> [u8; 32] {
            let v = base64::engine::general_purpose::STANDARD.decode(s).unwrap();
            let mut a = [0u8; 32];
            a.copy_from_slice(&v);
            a
        };
        for sm in &proof.samples {
            // map path_index -> content_id via the deterministic plan (web rebuilds it).
            let fp = file_at(&plan, sm.path_index);
            let mut chunk = vec![0u8; sm.byte_length as usize];
            bench::content_bytes_at(&kc, fp.content_id, sm.byte_offset, &mut chunk);
            let leaf = bench::leaf_hash(&fp.path(), sm.byte_offset, sm.byte_length, &chunk);
            assert_eq!(bench::root_base64(&leaf), sm.leaf_hash, "regenerated leaf must match the proof");
            let path: Vec<[u8; 32]> = sm.audit_path.iter().map(|s| dec(s)).collect();
            let recon = bench::root_from_path(leaf, &path, sm.leaf_index as usize, proof.leaf_count as usize);
            assert_eq!(bench::root_base64(&recon), proof.merkle_root, "reconstructed root must match the proof");
        }

        // proof round-trips through JSON (the wire form web ingests).
        let json = serde_json::to_string(&proof).unwrap();
        let back: BenchProof = serde_json::from_str(&json).unwrap();
        assert_eq!(back, proof);
    }

    // -- test helpers --
    fn file_at(plan: &CorpusPlan, path_index: u64) -> FilePlan {
        *plan.files.iter().find(|f| f.path_index == path_index).unwrap()
    }
    fn cid_of(plan: &CorpusPlan, path_index: u64) -> u64 {
        file_at(plan, path_index).content_id
    }
    fn is_original(plan: &CorpusPlan, path_index: u64) -> bool {
        // an original is the first member of no dupset's tail; equivalently it
        // is either a cluster head or a singleton (never a dupset tail entry).
        !plan.dupsets.iter().any(|set| set[1..].contains(&path_index))
    }
}
