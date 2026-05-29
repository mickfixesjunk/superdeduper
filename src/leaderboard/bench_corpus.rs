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
use std::io::Write;
use std::path::Path;

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

/// `corpus-v1-quick` — the `--bench-me` default (~2.53 GB, ~26.8% reclaimable).
pub fn quick_tier() -> TierSpec {
    TierSpec {
        corpus_version: "corpus-v1-quick",
        small: SizeClassSpec { file_size: SMALL_SIZE, file_count: 120_000, size2_clusters: 35_000, big_clusters: 50, big_size: 21 },
        medium: SizeClassSpec { file_size: MEDIUM_SIZE, file_count: 1_700, size2_clusters: 500, big_clusters: 2, big_size: 6 },
        large: SizeClassSpec { file_size: LARGE_SIZE, file_count: 1, size2_clusters: 0, big_clusters: 0, big_size: 0 },
    }
}

/// `corpus-v1-full` — opt-in (~18.8 GB, ~30.5% reclaimable). Byte volume is
/// PROVISIONAL per B.3 (reparametrizes when the benchmarker knee lands — only
/// the constants here change, never the layout logic).
pub fn full_tier() -> TierSpec {
    TierSpec {
        corpus_version: "corpus-v1-full",
        small: SizeClassSpec { file_size: SMALL_SIZE, file_count: 1_000_000, size2_clusters: 295_000, big_clusters: 250, big_size: 21 },
        medium: SizeClassSpec { file_size: MEDIUM_SIZE, file_count: 12_000, size2_clusters: 3_500, big_clusters: 5, big_size: 21 },
        large: SizeClassSpec { file_size: LARGE_SIZE, file_count: 12, size2_clusters: 2, big_clusters: 1, big_size: 3 },
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
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
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

/// The bench proof a `--bench-me` run submits: the committed root plus the
/// challenged sample set. The proof hash is BLAKE3 throughout (cryptographic);
/// river5 is NEVER used here (river5 stays the internal dedupe content hash).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct BenchProof {
    pub bench_challenge_id: String,
    pub corpus_version: String,
    pub leaf_count: u64,
    pub merkle_root: String,
    pub samples: Vec<SampleProof>,
}

/// Build the bench proof for a challenge: materialize the leaves, derive the
/// challenged positions, and emit each sample with its audit path. SELF-VERIFY:
/// every sample's audit path is checked to reconstruct the committed root
/// before it is emitted (a proof that fails its own verifier is never sent).
pub fn build_bench_proof(
    spec: &TierSpec,
    plan: &CorpusPlan,
    k_content: &[u8; 32],
    bench_challenge_id: &str,
    sample_n: usize,
) -> BenchProof {
    let leaves = compute_leaves(k_content, plan);
    assert_eq!(leaves.len() as u64, plan.leaf_count, "self-verify: leaf count");
    let locs = leaf_locations(plan);
    let root = bench::merkle_root(&leaves).expect("non-empty corpus");
    let positions = bench::challenge_positions(bench_challenge_id, plan.leaf_count, sample_n);

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
        bench_challenge_id: bench_challenge_id.to_string(),
        corpus_version: spec.corpus_version.to_string(),
        leaf_count: plan.leaf_count,
        merkle_root: bench::root_base64(&root),
        samples,
    }
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
    fn full_tier_matches_published_aggregates() {
        let plan = plan_corpus(&full_tier());
        assert_eq!(plan.file_count, 1_012_012, "full file_count");
        assert_eq!(plan.leaf_count, 1_015_072, "full leaf_count");
        assert_eq!(plan.total_bytes, 19_900_137_472, "full total_bytes (~18.8 GiB)");
        let dups: u64 = plan.dupsets.iter().map(|s| s.len() as u64 - 1).sum();
        assert_eq!(dups, 300_000 + 3_600 + 4, "full duplicate-file count");
        let frac = full_tier().reclaimable_fraction();
        assert!((0.304..0.306).contains(&frac), "full reclaimable ~30.5%, got {frac}");
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
        let proof = build_bench_proof(&spec, &plan, &kc, "bench-chal-tiny", 32);

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
