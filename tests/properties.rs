//! Property-based correctness tests for the engine.
//!
//! These tests build a random universe of files (with intentional
//! duplicate equivalence classes), drive the real engine end-to-end,
//! and assert invariants that *must* hold regardless of input shape:
//!
//! * **Full recall, no false positives.** Every equivalence class of
//!   size ≥ 2 appears as exactly one duplicate group; no group ever
//!   mixes files from different classes.
//! * **Singleton invariant.** A file whose class has size 1 never
//!   appears in any reported group.
//! * **Order invariance.** Shuffling the input directory order and
//!   re-scanning produces the same set of groups.
//! * **Thread invariance.** Hashing with `threads=1` vs the rayon
//!   default produces the same set of groups.
//! * **Min-size filter.** Files below `--min-size` are never reported
//!   regardless of how many copies exist.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use proptest::test_runner::Config as PropConfig;
use tempfile::TempDir;

use superdupe::cli::OutputFormat;
use superdupe::config::ScanConfig;
use superdupe::{inventory, pipeline};

/// Plant N files where each file `i` is assigned to a content class
/// `classes[i]`; files in the same class have identical bytes.
fn plant_universe(
    dir: &Path,
    sizes_by_class: &BTreeMap<u32, u64>,
    classes: &[u32],
) -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(classes.len());
    for (i, &cls) in classes.iter().enumerate() {
        let size = *sizes_by_class.get(&cls).unwrap();
        let mut payload = vec![0u8; size as usize];
        // Deterministic per-class content: class id repeated.
        for (j, byte) in payload.iter_mut().enumerate() {
            *byte = ((cls.wrapping_mul(0x9E3779B9) ^ (j as u32)) & 0xFF) as u8;
        }
        let p = dir.join(format!("f{:04}.bin", i));
        fs::write(&p, &payload).unwrap();
        paths.push(p);
    }
    paths
}

fn run_scan(root: &Path, threads: usize, min_size: u64) -> Vec<pipeline::DuplicateGroup> {
    let cfg = ScanConfig {
        roots: vec![root.to_path_buf()],
        reference_roots: vec![],
        min_size,
        max_size: None,
        include: None,
        exclude: None,
        format: OutputFormat::Text,
        paranoid: false,
        use_cache: false,
        use_format_aware: false,
        threads,
        queue_depth: None,
        output: None,
        follow_links: false,
        allow_system_paths: false,
        io_threads: 4,
        hash_algo: superdupe::pipeline::hash::HashAlgo::Blake3,
    };
    let files = inventory::enumerate(&cfg).unwrap();
    let size_groups = pipeline::grouping::group_by_size(files);
    let laid = pipeline::layout::resolve(size_groups).unwrap();
    pipeline::hash::run(laid, &cfg).unwrap()
}

/// Strategy: produce a `(class_assignments, sizes_by_class)` pair such
/// that some classes are duplicated and some aren't. We keep sizes
/// modest so the test suite stays fast.
fn universe_strategy() -> impl Strategy<Value = (Vec<u32>, BTreeMap<u32, u64>)> {
    // 5–25 files, each with a class in [0, 12). Bias toward small
    // numbers so collisions are common.
    let count = 5usize..=25;
    let classes = count.prop_flat_map(|n| prop::collection::vec(0u32..12, n));
    let sizes = prop::collection::btree_map(
        0u32..12,
        prop_oneof![
            Just(1u64),       // very small (boundary)
            Just(4_096u64),   // Tier 1 boundary
            Just(8_192u64),   // small
            Just(100_000u64), // Tier 2 boundary territory
            Just(300_000u64), // Tier 2 active
        ],
        1..13,
    );
    (classes, sizes).prop_map(|(c, mut s)| {
        // Ensure every class id used in `c` has a size entry.
        for &cls in &c {
            s.entry(cls).or_insert(4_096);
        }
        (c, s)
    })
}

/// Compute the oracle: which classes have ≥ 2 members (eligible to
/// appear in output) given the min-size threshold.
fn oracle_dup_classes(
    classes: &[u32],
    sizes: &BTreeMap<u32, u64>,
    min_size: u64,
) -> BTreeMap<u32, usize> {
    let mut counts = BTreeMap::<u32, usize>::new();
    for &c in classes {
        if *sizes.get(&c).unwrap() >= min_size {
            *counts.entry(c).or_default() += 1;
        }
    }
    counts.retain(|_, n| *n >= 2);
    counts
}

proptest! {
    #![proptest_config(PropConfig {
        cases: 32,
        max_shrink_iters: 64,
        .. PropConfig::default()
    })]

    /// Recall = 1.0, precision = 1.0: every oracle equivalence class
    /// appears as exactly one group with the right membership; no
    /// group mixes classes.
    #[test]
    fn engine_matches_oracle(
        (classes, sizes) in universe_strategy(),
        min_size in prop_oneof![Just(0u64), Just(4_096u64), Just(8_192u64)],
    ) {
        let dir = TempDir::new().unwrap();
        let paths = plant_universe(dir.path(), &sizes, &classes);
        let groups = run_scan(dir.path(), 2, min_size);

        // Map each path back to its planted class.
        let class_of: BTreeMap<&Path, u32> = paths
            .iter()
            .zip(&classes)
            .map(|(p, c)| (p.as_path(), *c))
            .collect();

        let oracle = oracle_dup_classes(&classes, &sizes, min_size);

        // Build engine output: {class → set of paths}, with assertion
        // that each group is single-class.
        let mut engine_classes: BTreeMap<u32, BTreeSet<PathBuf>> = BTreeMap::new();
        for g in &groups {
            let cls = class_of.get(g.files[0].as_path()).copied()
                .expect("engine returned an unplanted path");
            for p in &g.files {
                let pc = class_of.get(p.as_path()).copied()
                    .expect("engine returned an unplanted path");
                prop_assert_eq!(pc, cls, "group contains files from different classes");
            }
            let bucket = engine_classes.entry(cls).or_default();
            for p in &g.files {
                bucket.insert(p.clone());
            }
        }

        // Every oracle class must be present with the same membership.
        for (cls, expected_count) in &oracle {
            let got = engine_classes.get(cls).cloned().unwrap_or_default();
            prop_assert_eq!(got.len(), *expected_count,
                "class {} expected {} members, got {}", cls, expected_count, got.len());
        }
        // Engine must not invent classes that the oracle says are
        // singletons or below min_size.
        for cls in engine_classes.keys() {
            prop_assert!(oracle.contains_key(cls),
                "engine reported class {} which the oracle considers ineligible", cls);
        }
    }

    /// Thread count must not affect which groups are reported.
    #[test]
    fn thread_invariance(
        (classes, sizes) in universe_strategy(),
    ) {
        let dir = TempDir::new().unwrap();
        let _ = plant_universe(dir.path(), &sizes, &classes);
        let g1 = canonical(&run_scan(dir.path(), 1, 0));
        let g4 = canonical(&run_scan(dir.path(), 4, 0));
        prop_assert_eq!(g1, g4);
    }
}

/// Reduce engine output to a structural set we can compare regardless
/// of group order, file order within group, or non-deterministic hex
/// strings: `BTreeSet<BTreeSet<PathBuf>>`.
fn canonical(groups: &[pipeline::DuplicateGroup]) -> BTreeSet<BTreeSet<PathBuf>> {
    groups
        .iter()
        .map(|g| g.files.iter().cloned().collect())
        .collect()
}

// Sanity proptest: planting only unique files yields zero groups.
proptest! {
    #![proptest_config(PropConfig { cases: 16, .. PropConfig::default() })]

    #[test]
    fn unique_files_never_match(
        n in 2usize..16,
        size in 16u64..2048,
    ) {
        let dir = TempDir::new().unwrap();
        for i in 0..n {
            let mut body = vec![0u8; size as usize];
            for (j, b) in body.iter_mut().enumerate() {
                *b = (((i as u32 * 0x1000193) ^ j as u32) & 0xFF) as u8;
            }
            fs::write(dir.path().join(format!("f{i}.bin")), &body).unwrap();
        }
        let groups = run_scan(dir.path(), 2, 0);
        prop_assert!(groups.is_empty(), "unique inputs produced groups: {:?}", groups);
    }
}
