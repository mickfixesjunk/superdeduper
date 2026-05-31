//! A-ref-keeper — reference-drive keeper-invariant regression test.
//!
//! Bug context (Mick, 2026-05-30): a multi-root scan with one root
//! starred as the reference drive (R) was observed to produce
//! duplicate groups where a NON-reference root (A/B/C) landed at
//! files[0] and R was listed as a dupe. The GUI flow runs
//! `filter_reference_only` (which sorts R first) but then runs
//! `order_keeper_first` (which applied the Smart/Newest/Path-length
//! heuristic ignorant of the reference set) and silently demoted R.
//!
//! This test pins the invariant: across a multi-root corpus with one
//! reference root, EVERY duplicate group emitted by the engine must
//! place a reference-root file at files[0] when one is present.

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use superdeduper::gui::events::EngineEvent;
use superdeduper::gui::live;
use superdeduper::gui::state::{RootEntry, ScanSettings};

/// Build a 4-root corpus on disk: R (reference), A, B, C — each root
/// is its own directory under `base`, and each contains an identical
/// 8 KiB file (above the 4 KiB GUI `min_size_bytes` floor). Returns
/// (R, [A, B, C]).
///
/// The A/B/C paths are intentionally DEEPER than R's so the Smart
/// heuristic's depth bonus would otherwise prefer them — this is the
/// shape that surfaced the inversion. R also gets the OLDEST mtime
/// (an older backup-style layout) so Newest-strategy paths would
/// also pick a non-R sibling without the reference-priority fix.
fn build_multi_root_corpus(base: &std::path::Path) -> (PathBuf, [PathBuf; 3]) {
    let r_root = base.join("R");
    let a_root = base.join("A");
    let b_root = base.join("B");
    let c_root = base.join("C");
    // Deeper non-reference paths so Smart's `depth` bonus prefers them.
    let r_file = r_root.join("file.bin");
    let a_file = a_root.join("Documents").join("Projects").join("file.bin");
    let b_file = b_root
        .join("Documents")
        .join("Projects")
        .join("Sub")
        .join("file.bin");
    let c_file = c_root
        .join("Documents")
        .join("Projects")
        .join("Sub")
        .join("Nested")
        .join("file.bin");
    for parent in [&r_file, &a_file, &b_file, &c_file].iter().map(|p| p.parent().unwrap()) {
        std::fs::create_dir_all(parent).unwrap();
    }
    // 8 KiB of identical bytes — above ScanSettings::default().min_size_bytes (4 KiB).
    let mut blob = b"sdd-ref-keeper-".to_vec();
    blob.resize(8192, 0);
    // Write R first + oldest, then sleep so A/B/C are strictly newer
    // (compounds the bug — Newest-strategy would also pick a non-R sibling).
    std::fs::write(&r_file, &blob).unwrap();
    std::thread::sleep(Duration::from_millis(30));
    std::fs::write(&a_file, &blob).unwrap();
    std::fs::write(&b_file, &blob).unwrap();
    std::fs::write(&c_file, &blob).unwrap();
    (r_root, [a_root, b_root, c_root])
}

/// Drive a hermetic engine run with the supplied roots + settings and
/// collect every `DuplicateFound` event until `ScanFinished` lands (or
/// the deadline elapses). XDG_DATA_HOME points at `cache_dir` so the
/// cache + scan_history are isolated from the host's real state.
fn run_engine_collect_groups(
    cache_dir: &std::path::Path,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
) -> Vec<superdeduper::gui::events::DuplicateGroupSummary> {
    // Cache + scan_history live under XDG_DATA_HOME — keep them in tempdir.
    // Tests in this file are serialized via `env_lock()` so the env mutation
    // is safe.
    unsafe {
        std::env::set_var("XDG_DATA_HOME", cache_dir);
        std::env::set_var("XDG_CACHE_HOME", cache_dir);
    }
    let (tx, rx) = crossbeam_channel::unbounded();
    let cancel = Arc::new(AtomicBool::new(false));
    let join = live::spawn_with_settings(
        tx,
        roots,
        settings,
        cancel.clone(),
        None,
        superdeduper::cli::ScanMode::Exact,
        superdeduper::cli::ImageSimilarityThresholdArg::Fixed(5),
        superdeduper::cli::ImageHashAlgoArg::default(),
        5.0,
    );
    let mut groups = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if Instant::now() >= deadline {
            cancel.store(true, std::sync::atomic::Ordering::SeqCst);
            panic!("engine did not finish within 60s — groups so far: {}", groups.len());
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(EngineEvent::DuplicateFound(g)) => groups.push(g),
            Ok(EngineEvent::ScanFinished { .. }) => break,
            Ok(_) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    let _ = join.join();
    groups
}

/// Process-wide lock — these tests mutate XDG env, must serialize.
fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[test]
fn reference_root_is_always_keeper_across_smart_strategy() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).unwrap();
    let (r_root, [a_root, b_root, c_root]) = build_multi_root_corpus(&corpus_root);

    // Roots in NON-reference-first order so order alone can't save us —
    // A is listed before R. Without the reference-priority short-circuit,
    // `order_keeper_first` would (with Smart strategy on these
    // increasingly-deep paths) put C or B at files[0] and R as a dupe.
    let roots = vec![
        RootEntry { path: a_root.clone(), is_reference: false },
        RootEntry { path: b_root.clone(), is_reference: false },
        RootEntry { path: c_root.clone(), is_reference: false },
        RootEntry { path: r_root.clone(), is_reference: true },
    ];

    let settings = ScanSettings {
        use_cache: false, // Hermetic — no cache reads/writes between runs.
        threads: Some(2),
        io_threads: Some(2),
        ..ScanSettings::default()
    };

    let cache_dir = tmp.path().join("xdg");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let groups = run_engine_collect_groups(&cache_dir, roots, settings);

    assert!(
        !groups.is_empty(),
        "expected at least one duplicate group from the 4-root identical-content corpus"
    );
    for (i, g) in groups.iter().enumerate() {
        assert!(
            g.files.len() >= 2,
            "group {i} has <2 files: {:?}",
            g.files
        );
        assert!(
            g.files[0].starts_with(&r_root),
            "A-ref-keeper invariant violated: group {i} files[0]={} is NOT under reference root {} \
             (full group: {:?})",
            g.files[0].display(),
            r_root.display(),
            g.files,
        );
    }
}

#[test]
fn reference_root_invariant_holds_when_added_last_to_roots_list() {
    // Same shape as above but with reference root added FIRST in the
    // roots list — should still hold (regression against the
    // "files[0] = first walker emission" failure mode where adding R
    // first masked the bug).
    let _g = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).unwrap();
    let (r_root, [a_root, b_root, c_root]) = build_multi_root_corpus(&corpus_root);

    let roots = vec![
        RootEntry { path: r_root.clone(), is_reference: true },
        RootEntry { path: a_root.clone(), is_reference: false },
        RootEntry { path: b_root.clone(), is_reference: false },
        RootEntry { path: c_root.clone(), is_reference: false },
    ];

    let settings = ScanSettings {
        use_cache: false,
        threads: Some(2),
        io_threads: Some(2),
        ..ScanSettings::default()
    };

    let cache_dir = tmp.path().join("xdg");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let groups = run_engine_collect_groups(&cache_dir, roots, settings);

    assert!(!groups.is_empty(), "expected at least one duplicate group");
    for (i, g) in groups.iter().enumerate() {
        assert!(
            g.files[0].starts_with(&r_root),
            "group {i} files[0]={} not under R={}",
            g.files[0].display(),
            r_root.display(),
        );
    }
}

#[test]
fn no_reference_root_falls_through_to_smart_heuristic() {
    // Negative control — when NO root is marked reference, the engine
    // must still emit duplicate groups (the fix didn't accidentally
    // gate emission on reference-set presence). The keeper here is
    // chosen by the Smart heuristic alone; we only assert the groups
    // exist + are non-empty, not which file wins.
    let _g = env_lock();
    let tmp = tempfile::tempdir().expect("tempdir");
    let corpus_root = tmp.path().join("corpus");
    std::fs::create_dir_all(&corpus_root).unwrap();
    let (_r_root, [a_root, b_root, c_root]) = build_multi_root_corpus(&corpus_root);

    let roots = vec![
        RootEntry { path: a_root.clone(), is_reference: false },
        RootEntry { path: b_root.clone(), is_reference: false },
        RootEntry { path: c_root.clone(), is_reference: false },
    ];

    let settings = ScanSettings {
        use_cache: false,
        threads: Some(2),
        io_threads: Some(2),
        ..ScanSettings::default()
    };

    let cache_dir = tmp.path().join("xdg");
    std::fs::create_dir_all(&cache_dir).unwrap();
    let groups = run_engine_collect_groups(&cache_dir, roots, settings);

    assert!(!groups.is_empty(), "duplicate group emission must still work with no reference root");
    for g in &groups {
        assert!(g.files.len() >= 2, "groups should have >=2 files: {:?}", g.files);
    }
}
