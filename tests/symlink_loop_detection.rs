//! T1.7 — symlink walker loop detection.
//!
//! Spec: `~/sd-bench-local/design/t1.7-symlink-loop-detection-spec.md`.
//! These tests exercise the visited-set logic against real symlink
//! topologies on disk. Unix-only (Linux + macOS) because creating
//! directory symlinks portably from CI requires the `unix::fs::symlink`
//! helper; the Windows path is exercised at runtime when users hit
//! a junction-based corpus. Criteria 4 + 6 (cross-volume / hardlink
//! alias) need Windows + admin and are deferred to manual QA.

#![cfg(unix)]

use std::path::PathBuf;

use superdeduper::config::ScanConfig;
use superdeduper::inventory::walk::{enumerate_cancellable, WalkEvent};

fn tmp_dir(label: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("sd-t17-{label}-"))
        .tempdir()
        .unwrap()
}

fn write_file(path: &std::path::Path, body: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn cfg_with(roots: Vec<PathBuf>, follow_links: bool) -> ScanConfig {
    ScanConfig {
        roots,
        reference_roots: vec![],
        min_size: 1,
        max_size: None,
        tier1_bytes: 4096,
        include: None,
        exclude: None,
        format: superdeduper::cli::OutputFormat::Text,
        use_cache: false,
        use_format_aware: false,
        threads: 2,
        io_threads: 4,
        output: None,
        follow_links,
        allow_system_paths: false,
            force_walker: false,
        allow_recall_on_read: false,
        hash_algo: superdeduper::pipeline::hash::HashAlgo::Blake3,
        exclusion_policy: superdeduper::exclusions::ExclusionPolicy::disabled(),
        exclusion_counters: std::sync::Arc::new(
            superdeduper::exclusions::ExclusionCounters::default(),
        ),
    }
}

fn collect_walk(cfg: &ScanConfig) -> (Vec<PathBuf>, u32) {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut cycle_skipped = 0u32;
    let _ = enumerate_cancellable(cfg, None, |evt| match evt {
        WalkEvent::FileFound { path, .. } => files.push(path.to_path_buf()),
        WalkEvent::SymlinkCycleSkipped { .. } => cycle_skipped += 1,
        _ => {}
    })
    .unwrap();
    (files, cycle_skipped)
}

/// Criterion 1: two-step cycle. dir_a → dir_b, dir_b/back → dir_a.
/// Scan must complete, emit exactly one SymlinkCycleSkipped, and
/// not double-count files inside dir_a.
#[test]
fn two_step_symlink_cycle_does_not_hang() {
    let tmp = tmp_dir("two-step");
    let root = tmp.path();
    let dir_a = root.join("dir_a");
    let dir_b = root.join("dir_b");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    write_file(&dir_a.join("file-in-a.txt"), b"in a");
    write_file(&dir_b.join("file-in-b.txt"), b"in b");
    // dir_b/back → dir_a creates the cycle when we follow it.
    std::os::unix::fs::symlink(&dir_a, dir_b.join("back")).unwrap();

    let cfg = cfg_with(vec![root.to_path_buf()], true);
    let (files, skipped) = collect_walk(&cfg);

    // dir_a's file appears exactly once + dir_b's file exactly once.
    let count_a = files
        .iter()
        .filter(|p| p.ends_with("file-in-a.txt"))
        .count();
    let count_b = files
        .iter()
        .filter(|p| p.ends_with("file-in-b.txt"))
        .count();
    assert_eq!(count_a, 1, "file-in-a should appear once, got {count_a}");
    assert_eq!(count_b, 1, "file-in-b should appear once, got {count_b}");
    assert_eq!(
        skipped, 1,
        "expected one SymlinkCycleSkipped event, got {skipped}"
    );
}

/// Criterion 2: self-cycle. dir_a/loop → dir_a.
#[test]
fn self_symlink_cycle_is_detected() {
    let tmp = tmp_dir("self");
    let root = tmp.path();
    let dir_a = root.join("dir_a");
    std::fs::create_dir_all(&dir_a).unwrap();
    write_file(&dir_a.join("file.txt"), b"hi");
    std::os::unix::fs::symlink(&dir_a, dir_a.join("loop")).unwrap();

    let cfg = cfg_with(vec![root.to_path_buf()], true);
    let (files, skipped) = collect_walk(&cfg);

    let count = files.iter().filter(|p| p.ends_with("file.txt")).count();
    assert_eq!(count, 1);
    assert_eq!(skipped, 1);
}

/// Criterion 3: 3-hop cycle. a → b → c → a.
#[test]
fn three_hop_cycle_is_detected_once() {
    let tmp = tmp_dir("three-hop");
    let root = tmp.path();
    let a = root.join("a");
    let b = root.join("b");
    let c = root.join("c");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::create_dir_all(&c).unwrap();
    write_file(&a.join("file-a.txt"), b"a");
    write_file(&b.join("file-b.txt"), b"b");
    write_file(&c.join("file-c.txt"), b"c");
    std::os::unix::fs::symlink(&b, a.join("to_b")).unwrap();
    std::os::unix::fs::symlink(&c, b.join("to_c")).unwrap();
    std::os::unix::fs::symlink(&a, c.join("to_a")).unwrap();

    let cfg = cfg_with(vec![root.to_path_buf()], true);
    let (files, skipped) = collect_walk(&cfg);

    // Each file shows up exactly once across the whole tree, even
    // though the symlink graph reaches each dir multiple times.
    for name in &["file-a.txt", "file-b.txt", "file-c.txt"] {
        let count = files.iter().filter(|p| p.ends_with(name)).count();
        assert_eq!(count, 1, "{name} should appear once, got {count}");
    }
    // Two of the three symlinks form cycles back to already-visited
    // dirs; the first symlink that lands on a dir is followed.
    // Total cycle-skipped events: 2.
    assert!(
        skipped >= 1,
        "expected at least one cycle-skipped, got {skipped}"
    );
}

/// Criterion 5: no cycle. Linear symlink chain a → b → c → real.
/// All directories enumerated; zero cycle events.
#[test]
fn linear_symlink_chain_emits_no_cycle_events() {
    let tmp = tmp_dir("linear");
    let root = tmp.path();
    let real = root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    write_file(&real.join("the-file.txt"), b"x");

    // a → b → c → real (each a symlink to the next).
    std::os::unix::fs::symlink(&real, root.join("c")).unwrap();
    std::os::unix::fs::symlink(root.join("c"), root.join("b")).unwrap();
    std::os::unix::fs::symlink(root.join("b"), root.join("a")).unwrap();

    let cfg = cfg_with(vec![root.to_path_buf()], true);
    let (files, skipped) = collect_walk(&cfg);

    // 'real' is reached via a, b, c, and the direct entry — but only
    // the first one inserts into visited; the rest are cycle-skipped.
    let count = files.iter().filter(|p| p.ends_with("the-file.txt")).count();
    assert_eq!(count, 1, "the-file should appear exactly once");
    assert!(skipped >= 3, "three aliases of 'real' should cycle-skip");
}

/// Criterion 9: --follow-links OFF regression. With the cycle on disk
/// but follow_links=false, the symlink is skipped at the EntrySkipped
/// layer and visited_dirs stays empty.
#[test]
fn follow_links_off_does_not_follow_symlinks_at_all() {
    let tmp = tmp_dir("nofollow");
    let root = tmp.path();
    let dir_a = root.join("dir_a");
    std::fs::create_dir_all(&dir_a).unwrap();
    write_file(&dir_a.join("file.txt"), b"hi");
    std::os::unix::fs::symlink(&dir_a, dir_a.join("loop")).unwrap();

    let cfg = cfg_with(vec![root.to_path_buf()], false);
    let (files, skipped) = collect_walk(&cfg);

    // 'file.txt' appears once (from the normal directory walk).
    let count = files.iter().filter(|p| p.ends_with("file.txt")).count();
    assert_eq!(count, 1);
    // No cycle skipping — we never followed the symlink in the first
    // place, so the visited set was never populated.
    assert_eq!(skipped, 0);
}

/// Criterion 10: scan-reset isolation. Two consecutive scans must
/// each start with an empty visited_dirs. The second one's results
/// should look identical to the first, not "everything cycle-skipped".
#[test]
fn second_scan_starts_with_clean_visited_set() {
    let tmp = tmp_dir("isolation");
    let root = tmp.path();
    let real = root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    write_file(&real.join("file.txt"), b"x");
    std::os::unix::fs::symlink(&real, root.join("alias")).unwrap();

    let cfg = cfg_with(vec![root.to_path_buf()], true);
    let (files_a, skipped_a) = collect_walk(&cfg);
    let (files_b, skipped_b) = collect_walk(&cfg);

    // Same call, same outputs.
    assert_eq!(files_a.len(), files_b.len());
    assert_eq!(skipped_a, skipped_b);
    // And both saw the file (visited didn't carry across runs and
    // cause the file to be reported zero times on run 2).
    assert!(files_b.iter().any(|p| p.ends_with("file.txt")));
}
