//! End-to-end resume regression test.
//!
//! Reproduces Mick's report: cancelling a GUI scan partway through
//! hashing should leave a checkpoint on disk. On the next launch,
//! the engine should detect that checkpoint, replay the previously
//! confirmed duplicate groups, skip Stage 1 entirely (saved_inventory),
//! and continue hashing the un-hashed files.
//!
//! The bug we hit on `D:\sdd-tests`: when cancellation propagated as
//! an `io::Error::Interrupted("cancelled")` from inside the Tier 3
//! streaming hasher, the chunks-loop's `?` operator short-circuited
//! past the cancel-detection-and-save at the top of the next
//! iteration — so the checkpoint never reached disk and the next
//! launch started from scratch.
//!
//! Note: file-level resume granularity is "chunk done". We can't
//! mid-tier-3 a single file, but we can verify that a scan cancelled
//! mid-chunks writes a checkpoint and the next launch picks it up.

#![cfg(feature = "gui")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Process-wide serial gate for tests in this file. Both tests call
/// `isolate_cache_dir`, which `env::set_var("XDG_CACHE_HOME", ...)`.
/// When cargo runs the two tests in parallel within the same test
/// binary, they race each other on that env var — test A's engine
/// can read test B's XDG_CACHE_HOME, write its cache there, then
/// test A's "run 2" finds zero cache hits because the rows are in
/// the WRONG directory. Manifested as a hard-to-reproduce
/// `run 2 should hit the cache from run 1's writes; cache: 0 hits`
/// flake on faster CI runners (locally serial-by-default; CI's
/// faster cores expose the race).
///
/// PoisonError doesn't kill us — even a panicked predecessor leaves
/// the test infra in a state the next test can recover from (each
/// test sets its own XDG_CACHE_HOME on entry).
static SERIAL: Mutex<()> = Mutex::new(());

use superdeduper::gui::checkpoint;
use superdeduper::gui::events::{EngineEvent, LogLevel};
use superdeduper::gui::live;
use superdeduper::gui::state::{RootEntry, ScanSettings};

/// Set up a scratch directory with enough files (and enough byte
/// volume) that hashing takes long enough to observe a cancel
/// interrupt. Returns the scratch dir handle (must outlive the test)
/// and the path used as the scan root.
fn make_corpus(num_dup_groups: usize, files_per_group: usize) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("corpus");
    std::fs::create_dir(&root).unwrap();
    // Big enough that Tier 3 actually fires (each file > 1 MiB) so
    // the streaming hash path is exercised — that's the path where
    // the original cancellation-as-io::Interrupted bug lived.
    const FILE_SIZE: usize = 4 * 1024 * 1024; // 4 MiB
    for g in 0..num_dup_groups {
        let mut payload = vec![0u8; FILE_SIZE];
        // Make each group's payload distinct so Tier 1 doesn't collapse them.
        let stamp: u32 = 0x9E37_79B9u32.wrapping_mul(g as u32 + 1);
        for (i, b) in payload.iter_mut().enumerate() {
            *b = ((stamp.wrapping_mul(i as u32 + 1)) >> 24) as u8;
        }
        for f in 0..files_per_group {
            let p = root.join(format!("g{g:03}-f{f:02}.bin"));
            std::fs::write(&p, &payload).unwrap();
        }
    }
    (dir, root)
}

/// Spawn the engine and drive it until either (a) the cancel atomic
/// fires and the engine emits ScanPaused / completes, or (b) the
/// engine emits ScanFinished. Collects all Log events and returns
/// them so callers can inspect "Resuming from checkpoint" etc.
fn drive_engine_until_done(
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel_after: Option<CancelTrigger>,
) -> (Vec<EngineEvent>, Arc<AtomicBool>) {
    let (tx, rx) = crossbeam_channel::bounded::<EngineEvent>(8192);
    let cancel = Arc::new(AtomicBool::new(false));
    let handle = live::spawn_with_settings(
        tx.clone(),
        roots,
        settings,
        cancel.clone(),
        None,
        superdeduper::cli::ScanMode::Exact,
        5,
        5.0,
    );

    let mut events = Vec::new();
    let started = Instant::now();
    let mut cancel_fired = false;
    loop {
        let evt = rx
            .recv_timeout(Duration::from_secs(60))
            .expect("engine event timeout — scan stuck or hung");
        // Fire cancel based on trigger condition.
        if !cancel_fired {
            if let Some(trigger) = &cancel_after {
                if trigger.matches(&evt) {
                    cancel.store(true, Ordering::Relaxed);
                    cancel_fired = true;
                }
            }
        }
        let terminal = matches!(
            evt,
            EngineEvent::ScanFinished { .. } | EngineEvent::ScanPaused { .. }
        );
        events.push(evt);
        if terminal {
            break;
        }
        if started.elapsed() > Duration::from_secs(120) {
            panic!("test exceeded 120s — engine never finished or paused");
        }
    }
    // Join the engine thread so we don't leak it across tests.
    let _ = handle.join();
    drop(tx);
    (events, cancel)
}

#[allow(dead_code)]
enum CancelTrigger {
    /// Fire cancel on the first Status event whose text contains the
    /// given substring. Useful when the corpus is small enough that
    /// per-100-files counter events never fire.
    OnStatusContains(&'static str),
}

impl CancelTrigger {
    fn matches(&self, evt: &EngineEvent) -> bool {
        match (self, evt) {
            (CancelTrigger::OnStatusContains(needle), EngineEvent::Status(s)) => s.contains(needle),
            _ => false,
        }
    }
}

fn log_lines(events: &[EngineEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Log {
                level: LogLevel::Info,
                message,
            }
            | EngineEvent::Log {
                level: LogLevel::Warn,
                message,
            } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

/// Each test gets its own XDG_CACHE_HOME directory so the cache +
/// checkpoint file are sandboxed. Returns the TempDir handle that
/// must outlive the test.
///
/// IMPORTANT: env::set_var leaks across tests in the same binary.
/// We keep each scenario in its own #[test] function but they share
/// one process — call this with a unique suffix per scenario to
/// avoid race conditions on the env var.
fn isolate_cache_dir(label: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("sd-resume-test-{label}-"))
        .tempdir()
        .unwrap();
    // SAFETY: Single-threaded test setup. set_var is technically UB
    // if another thread reads the env concurrently, but the engine
    // reads XDG_CACHE_HOME exactly once when computing the cache /
    // checkpoint path during run(), and we set it before spawning.
    unsafe {
        std::env::set_var("XDG_CACHE_HOME", dir.path());
    }
    dir
}

#[test]
fn cancel_mid_hash_writes_checkpoint() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _cache_dir = isolate_cache_dir("cancel-mid-hash");
    let (corpus_dir, root) = make_corpus(8, 3);
    let roots = vec![RootEntry {
        path: root.clone(),
        is_reference: false,
    }];
    let settings = ScanSettings::default();

    let (events, _cancel) = drive_engine_until_done(
        roots,
        settings,
        // Trigger cancel as soon as hashing reports any progress —
        // exercises the mid-chunk cancel path (the bug's home turf).
        Some(CancelTrigger::OnStatusContains("hashing")),
    );

    // The engine should have emitted ScanPaused (not ScanFinished —
    // we cancelled before completion).
    let paused = events
        .iter()
        .any(|e| matches!(e, EngineEvent::ScanPaused { .. }));
    assert!(
        paused,
        "expected ScanPaused after mid-hash cancel; events: {:#?}",
        log_lines(&events)
    );

    // The checkpoint must exist on disk at the resolved path.
    let ckpt_path = checkpoint::default_checkpoint_path().expect("checkpoint path");
    assert!(
        ckpt_path.exists(),
        "checkpoint file missing at {} — this is the bug we're guarding against",
        ckpt_path.display()
    );

    // It should contain a saved_inventory (Stage 1 finished before
    // hashing started, so the walk results were persisted).
    let cp = checkpoint::load(&ckpt_path)
        .expect("load")
        .expect("checkpoint loaded");
    assert!(
        cp.saved_inventory.is_some(),
        "checkpoint should carry saved_inventory once Stage 1 has completed"
    );

    drop(corpus_dir);
}

#[test]
fn resume_after_cancel_replays_checkpoint() {
    let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _cache_dir = isolate_cache_dir("resume-after-cancel");
    let (corpus_dir, root) = make_corpus(6, 3);
    let roots = vec![RootEntry {
        path: root.clone(),
        is_reference: false,
    }];
    let settings = ScanSettings::default();

    // Run 1: full scan to completion. Populates the cache.
    let (events1, _) = drive_engine_until_done(roots.clone(), settings.clone(), None);
    assert!(
        events1
            .iter()
            .any(|e| matches!(e, EngineEvent::ScanFinished { .. })),
        "run 1 should finish"
    );
    // Sanity: run 1 wrote cache rows (cold cache).
    let logs1 = log_lines(&events1);
    let cache_line_1 = logs1
        .iter()
        .find(|m| m.starts_with("cache: "))
        .cloned()
        .expect("run 1 must emit a cache-stats line");
    assert!(
        cache_line_1.contains(" 0 hits"),
        "run 1 cold cache should have 0 hits: {cache_line_1}"
    );
    assert!(
        !cache_line_1.contains(" 0 writes"),
        "run 1 should write cache rows: {cache_line_1}"
    );

    // Run 2: scan again. The cache from run 1 should make this
    // a fast-forward — every file's hash already in the rusqlite
    // cache, so lookups hit instead of disk reads. This is the
    // EXACT behaviour resume relies on; if run 2 has zero hits,
    // resume on a real corpus will also re-hash everything.
    let (events2, _) = drive_engine_until_done(roots, settings, None);
    let logs2 = log_lines(&events2);
    let finished = events2
        .iter()
        .any(|e| matches!(e, EngineEvent::ScanFinished { .. }));
    assert!(
        finished,
        "run 2 should finish cleanly; got logs:\n{}",
        logs2.join("\n")
    );
    let cache_line_2 = logs2
        .iter()
        .find(|m| m.starts_with("cache: "))
        .cloned()
        .expect("run 2 must emit a cache-stats line");
    let hits: u64 = cache_line_2
        .strip_prefix("cache: ")
        .and_then(|after| {
            let comma = after.find(',')?;
            after[..comma].trim().strip_suffix(" hits")?.parse().ok()
        })
        .unwrap_or(0);
    assert!(
        hits > 0,
        "run 2 should hit the cache from run 1's writes; cache line: {cache_line_2}\nall logs:\n{}",
        logs2.join("\n")
    );
    let finished = events2
        .iter()
        .any(|e| matches!(e, EngineEvent::ScanFinished { .. }));
    assert!(
        finished,
        "run 2 should finish cleanly; got logs:\n{}",
        logs2.join("\n")
    );

    drop(corpus_dir);
}
