//! Bridge between the real scan engine and the GUI's event channel.
//!
//! The engine runs on a dedicated worker thread; the UI thread drains
//! `EngineEvent`s from a bounded channel each frame. Two flavours:
//!
//! * [`spawn`] — legacy entry point with default settings, kept for
//!   the `--live` CLI flag.
//! * [`spawn_with_settings`] — preferred. Takes the full roots list
//!   (with reference flags), user settings, and a cancellation token.
//!   Emits incremental events so the funnel, drive scope, treemap,
//!   and log panel animate as the scan progresses.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rayon::prelude::*;

use globset::{Glob, GlobSetBuilder};
use parking_lot::Mutex;

use crate::cache::Cache;
use crate::cli::OutputFormat;
use crate::config::ScanConfig;
use crate::gui::checkpoint::{self, Checkpoint};
use crate::gui::diagnostics::{self, DiagnosticsLog, EngineCounters};
use crate::gui::events::{
    DriveInfo, DuplicateGroupSummary, EngineEvent, LogLevel, OverallStage, ReadSample, Stage,
};
use crate::gui::state::{RootEntry, ScanSettings};
use crate::inventory;
use crate::pipeline;

/// Legacy single-root scan with default settings. Used by the
/// `--live` CLI flag where the user explicitly passed paths.
pub fn spawn(tx: crate::gui::perf_channel::PerfTx, roots: Vec<PathBuf>) -> thread::JoinHandle<()> {
    let entries = roots
        .into_iter()
        .map(|p| RootEntry {
            path: p,
            is_reference: false,
        })
        .collect();
    spawn_with_settings(
        tx,
        entries,
        ScanSettings::default(),
        Arc::new(AtomicBool::new(false)),
        None,
        crate::cli::ScanMode::Exact,
        crate::cli::ImageSimilarityThresholdArg::Fixed(5),
        crate::cli::ImageHashAlgoArg::default(),
        5.0,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_with_settings(
    tx: crate::gui::perf_channel::PerfTx,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
    defender_rtp_pre: Option<bool>,
    scan_mode: crate::cli::ScanMode,
    image_similarity_threshold: crate::cli::ImageSimilarityThresholdArg,
    image_hash_algorithm: crate::cli::ImageHashAlgoArg,
    audio_similarity_threshold: f64,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("superdeduper-engine".into())
        .spawn(move || {
            if let Err(e) = run(
                tx.clone(),
                roots,
                settings,
                cancel,
                defender_rtp_pre,
                scan_mode,
                image_similarity_threshold,
                image_hash_algorithm,
                audio_similarity_threshold,
            ) {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Error,
                    message: format!("engine: {e}"),
                });
                let _ = tx.send(EngineEvent::Status(format!("Failed: {e}")));
            }
        })
        .expect("spawn engine thread")
}

#[allow(clippy::too_many_arguments)]
fn run(
    tx: crate::gui::perf_channel::PerfTx,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
    _defender_rtp_pre: Option<bool>,
    scan_mode: crate::cli::ScanMode,
    image_similarity_threshold: crate::cli::ImageSimilarityThresholdArg,
    image_hash_algorithm: crate::cli::ImageHashAlgoArg,
    audio_similarity_threshold: f64,
) -> crate::Result<()> {
    let _scan_started_at = Instant::now();
    // Wall-clock start, separate from the Instant above (Instant is
    // monotonic + opaque; we need a UNIX timestamp the scan_history
    // persistence layer can sort + display). One reading, threaded
    // through to the ScanFinished hook below.
    let started_at_unix = crate::time::now_unix_secs();
    // Diagnostics report file — fresh per scan. Failure to open it
    // doesn't kill the scan; we just lose self-debug telemetry.
    let diag = DiagnosticsLog::open();
    let hash_impl: &str = match settings.hash_algo {
        crate::pipeline::hash::HashAlgo::Blake3 => "blake3 (Rust crate)",
        crate::pipeline::hash::HashAlgo::River5 => river5::impl_name(),
    };
    if let Some(d) = &diag {
        d.log(
            "SCAN-START",
            format_args!(
                "roots={} min_size={} format_aware={} use_cache={} threads={:?} \
                 hash_algo={} hash_impl={hash_impl}",
                roots.len(),
                settings.min_size_bytes,
                settings.use_format_aware,
                settings.use_cache,
                settings.threads,
                settings.hash_algo.tag(),
            ),
        );
        for r in &roots {
            d.log(
                "ROOT",
                format_args!(
                    "path={:?} reference={}",
                    r.path.display().to_string(),
                    r.is_reference
                ),
            );
        }
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: "Diagnostics report opened in ./diagnostics/".to_string(),
        });
    }
    // Surface the linked hash implementation so users running with
    // DDH-128 can tell at a glance whether they're on the stub or
    // the eventual AES-NI core.
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!("Hash impl: {hash_impl}"),
    });

    // #15 L2 — surface mount-info warnings per scan root on Linux.
    // Pool-dedup-capable filesystems, network mounts, and dm-mapped
    // volumes (LUKS) each have their own gotchas — log them once at
    // scan-start so they appear in the GUI Log panel before any
    // dup-find event lands.
    #[cfg(target_os = "linux")]
    {
        for root in &roots {
            if let Some(info) = crate::platform::linux::mount_info::for_path(&root.path) {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!("mount: {}", info.summary_line()),
                });
                for w in info.warnings() {
                    let _ = tx.send(EngineEvent::Log {
                        level: LogLevel::Warn,
                        message: w,
                    });
                }
            }
        }
    }

    let cfg = build_config(&roots, &settings)?;
    // PERF (#191 overnight push, 2026-05-31): pre-normalize reference roots
    // to their non-verbatim form ONCE at scan start. reference_belongs is
    // called on every GUI repaint frame × every group × every file; doing
    // strip_verbatim_prefix() on each reference root per-call was burning
    // CPU at 30fps. With pre-normalized entries here, reference_belongs
    // only normalizes the candidate path (one strip per call) + does a
    // straight starts_with against already-normalized references.
    let reference_set: hashbrown::HashSet<PathBuf> = roots
        .iter()
        .filter(|r| r.is_reference)
        .map(|r| strip_verbatim_prefix(&r.path).to_path_buf())
        .collect();
    let root_paths: Vec<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
    let checkpoint_path = checkpoint::default_checkpoint_path().ok();
    let mut checkpoint_state = Checkpoint::new(roots.clone(), settings.clone());

    // #64 Phase 1 — diagnostic instrumentation around the resume
    // load path. Every fail mode previously fell through silently
    // ("no checkpoint found" indistinguishable from "load failed"
    // from "settings drifted" from "roots drifted"). Now each
    // step logs WHY it took the branch it did, so when Mick reports
    // "resume restarted from 0%" we can read his log + see whether
    // the checkpoint loaded at all, or matched, or had real state.
    let prior: Option<Checkpoint> = match &checkpoint_path {
        None => {
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Warn,
                message: "resume diag: default_checkpoint_path() failed; cannot resume any state"
                    .into(),
            });
            None
        }
        Some(p) => match checkpoint::load(p) {
            Ok(Some(cp)) => {
                let size_hint = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "resume diag: checkpoint loaded from {} ({} bytes); prev_dups={}, saved_inventory={}",
                        p.display(),
                        size_hint,
                        cp.previous_duplicates.len(),
                        cp.saved_inventory.as_ref().map(|v| v.len()).unwrap_or(0),
                    ),
                });
                // #99 PR2 — Replace the pre-#99 binary
                // `roots_match && settings_match` filter with the
                // unified ResumeTier classification. Tier dictates
                // what gets carried forward: Full/Warm restore
                // everything; InventoryOnly keeps saved_inventory
                // only; Marker/Fresh skip the restore.
                let schema_state = crate::cache::default_cache_path()
                    .and_then(|p| crate::cache::schema_state(&p))
                    .unwrap_or(crate::cache::SchemaState::NoCache);
                let session_ctx = crate::gui::resume_tier::SessionContext {
                    roots: roots.clone(),
                    settings: settings.clone(),
                    schema_version_mismatch: schema_state.implies_cold_cache(),
                };
                let tier = crate::gui::resume_tier::classify_resume_tier(&cp, &session_ctx);
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "resume diag: classified tier = {tier:?}; cp.roots.len={}, current.roots.len={}; cp.prev_dups={}, cp.saved_inventory={}",
                        cp.roots.len(),
                        roots.len(),
                        cp.previous_duplicates.len(),
                        cp.saved_inventory.as_ref().map(|v| v.len()).unwrap_or(0),
                    ),
                });
                match tier {
                    crate::gui::resume_tier::ResumeTier::Full
                    | crate::gui::resume_tier::ResumeTier::Warm => Some(cp),
                    crate::gui::resume_tier::ResumeTier::InventoryOnly => {
                        // Reuse the walker output (file list) but
                        // drop previous_duplicates — they were
                        // computed under different settings and
                        // may not be valid (e.g., min_size changed
                        // → different group composition).
                        let _ = tx.send(EngineEvent::Log {
                            level: LogLevel::Info,
                            message: "resume: settings changed since pause — reusing file inventory, redoing analysis".into(),
                        });
                        let mut trimmed = cp;
                        trimmed.previous_duplicates.clear();
                        trimmed.completed_hashes.clear();
                        Some(trimmed)
                    }
                    crate::gui::resume_tier::ResumeTier::Marker => {
                        // Checkpoint exists but holds nothing
                        // usable — pre-inventory marker only.
                        // Walker starts fresh.
                        let _ = tx.send(EngineEvent::Log {
                            level: LogLevel::Info,
                            message: "resume: pre-inventory marker — walker starts fresh".into(),
                        });
                        None
                    }
                    crate::gui::resume_tier::ResumeTier::Fresh => {
                        let _ = tx.send(EngineEvent::Log {
                            level: LogLevel::Warn,
                            message: format!(
                                "resume diag: tier=Fresh (roots or settings drifted beyond reuse); \
                                 cp.prev_dups={}, cp.saved_inventory={} (NOT carried forward)",
                                cp.previous_duplicates.len(),
                                cp.saved_inventory.as_ref().map(|v| v.len()).unwrap_or(0),
                            ),
                        });
                        None
                    }
                }
            }
            Ok(None) => {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "resume diag: no checkpoint file at {} (fresh scan)",
                        p.display()
                    ),
                });
                None
            }
            Err(e) => {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Warn,
                    message: format!(
                        "resume diag: checkpoint load failed at {}: {e} (treating as fresh scan)",
                        p.display()
                    ),
                });
                None
            }
        },
    };
    // #108 — Snapshot cumulative scan-work counters from prior
    // *before* the `if let Some(prior) = prior` block below moves
    // `prior` into its own scope. Used both to seed
    // checkpoint_state.cumulative_bytes_scanned (so the
    // pre-/post-inventory marker saves preserve the count across
    // multiple pause/resume cycles) and to pre-bump the engine
    // local accumulators (total_bytes_read, total_dups,
    // reclaimable_inode) so the submission payload reflects
    // cumulative work, not just post-resume work.
    let prior_cumulative_bytes_scanned: u64 = prior
        .as_ref()
        .map(|p| p.cumulative_bytes_scanned)
        .unwrap_or(0);
    let prior_cumulative_dups: u64 = prior
        .as_ref()
        .map(|p| p.previous_duplicates.len() as u64)
        .unwrap_or(0);
    let prior_cumulative_reclaimable: u64 = prior
        .as_ref()
        .map(|p| {
            p.previous_duplicates
                .iter()
                .map(|g| g.unique_inodes.saturating_sub(1).saturating_mul(g.size))
                .sum()
        })
        .unwrap_or(0);
    // #108-extended — Web's sanity checks require ALL run_shape
    // totals (bytes, files, wall_clock) to be chain-cumulative
    // together, not just bytes. Otherwise cumulative-bytes /
    // per-spawn-wall computes as absurd throughput on resume + the
    // backend rejects with 422. Pre-seed the file count + wall
    // clock the same way as bytes.
    let prior_cumulative_files_scanned: u64 = prior
        .as_ref()
        .map(|p| p.cumulative_files_scanned)
        .unwrap_or(0);
    let prior_cumulative_wall_clock_seconds: u64 = prior
        .as_ref()
        .map(|p| p.cumulative_wall_clock_seconds)
        .unwrap_or(0);
    checkpoint_state.cumulative_bytes_scanned = prior_cumulative_bytes_scanned;
    checkpoint_state.cumulative_files_scanned = prior_cumulative_files_scanned;
    checkpoint_state.cumulative_wall_clock_seconds = prior_cumulative_wall_clock_seconds;

    // Inventory state carried over from a prior pause: lets us skip
    // Stage 1 entirely and jump straight to size-grouping. Empty
    // (None) ⇒ no saved inventory; do a fresh walk.
    let mut resumed_inventory: Option<Vec<crate::inventory::FileEntry>> = None;
    // #99 PR6 — paths of files that are already members of restored
    // dup groups. These don't need Stage 4 re-hashing: their
    // group membership is already proven and survives across the
    // resume via PR5's preserved state.duplicates. Stage 4 filters
    // them out so the user sees genuine fast-forward (progress
    // bar starts at the prior position) instead of pretend-resume
    // (bar restarts at 0 and burns CPU re-confirming what we
    // already know). Built INSIDE the `if let Some(prior)` arm so
    // it stays empty on a fresh scan with no prior state.
    let mut restored_dup_paths: hashbrown::HashSet<std::path::PathBuf> = hashbrown::HashSet::new();
    if let Some(prior) = prior {
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "Resuming from checkpoint — {} duplicate group(s) carried over",
                prior.previous_duplicates.len()
            ),
        });
        for g in &prior.previous_duplicates {
            checkpoint_state.record(g);
            for p in &g.files {
                restored_dup_paths.insert(p.clone());
            }
            let _ = tx.send(EngineEvent::DuplicateFound(g.clone()));
        }
        // If the prior pause happened after inventory completed, the
        // walker output is on disk — promote it back into a runtime
        // FileEntry list so size-grouping can pick up right away.
        if let Some(saved) = prior.saved_inventory.clone() {
            let mapped: Vec<crate::inventory::FileEntry> = saved
                .into_iter()
                .map(|s| crate::inventory::FileEntry {
                    path: s.path,
                    size: s.size,
                    mtime: s.mtime,
                    file_ref: s.file_ref,
                    parent_ref: s.parent_ref,
                    usn: s.usn,
                    attributes: s.attributes,
                    volume_guid: s.volume_guid,
                    // T2.1 phase 4 default — the saved-inventory format
                    // doesn't carry placeholder state yet (schema added
                    // for T2.1 phase 5). Resumed scans will re-rely on
                    // the tier-guard check against `attributes` instead.
                    placeholder: crate::inventory::PlaceholderState::default(),
                })
                .collect();
            // Cache lookups require volume_guid. If most resumed
            // entries have None here, the cache fast-forward path
            // can't fire — every file ends up re-hashed. Surface
            // the count so a future "resume restarted at 1%"
            // diagnosis is one log line away.
            let with_guid = mapped.iter().filter(|f| f.volume_guid.is_some()).count();
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "Resume · skipping Stage 1: reusing {} file(s) from saved inventory ({} with volume_guid)",
                    mapped.len(),
                    with_guid
                ),
            });
            resumed_inventory = Some(mapped);
            // Also propagate into the new checkpoint we're about to
            // build so a subsequent pause doesn't lose the list.
            checkpoint_state.saved_inventory = Some(saved_files_from_runtime(
                resumed_inventory.as_deref().unwrap_or(&[]),
            ));
        }
    }

    let _ = tx.send(EngineEvent::ScanStarted {
        at: Instant::now(),
        roots: root_paths.clone(),
    });
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!("starting scan over {} root(s)", root_paths.len()),
    });
    for r in &roots {
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "  · {}{}",
                if r.is_reference {
                    "★ reference  "
                } else {
                    "             "
                },
                r.path.display()
            ),
        });
    }
    // Detect each root's underlying storage device (HDD vs SSD) so
    // the drive scope renders the right pattern. Stored as a flat
    // `Vec<bool>` indexed by root order; the per-file callback later
    // uses it to pick HDD-style (cumulative) vs SSD-style (scattered
    // by path hash) LCN positions.
    let seek_penalties: Vec<bool> = root_paths
        .iter()
        .map(|p| detect_seek_penalty(p.as_path()))
        .collect();
    for (i, r) in root_paths.iter().enumerate() {
        let has_seek_penalty = seek_penalties.get(i).copied().unwrap_or(true);
        let model = if has_seek_penalty { "HDD" } else { "SSD" };
        // Surface the detection result in BOTH the GUI log and the
        // diagnostics file. If a drive that should be SSD is being
        // reported as HDD here, we know the IOCTL fell back to the
        // safe default and the drive scope will use the wrong pattern.
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "Detected drive at {}: {model} (seek_penalty={has_seek_penalty})",
                r.display(),
            ),
        });
        if let Some(d) = &diag {
            d.log(
                "DRIVE",
                format_args!(
                    "id={i} path={:?} model={model} seek_penalty={has_seek_penalty}",
                    r.display().to_string()
                ),
            );
        }
        // Resolve the volume GUID once per root so the UI can use it
        // as a stable key for persisted HDD/SSD overrides. Falls
        // back to an empty string on non-Windows or when the lookup
        // fails — overrides for that drive then just won't persist.
        let volume_guid = volume_guid_for(r.as_path()).unwrap_or_default();
        let _ = tx.send(EngineEvent::DriveDiscovered(DriveInfo {
            id: i as u32,
            model: format!("{model} · Root {}", i + 1),
            has_seek_penalty,
            capacity_bytes: 0,
            volume_label: r.to_string_lossy().into_owned(),
            volume_guid,
        }));
    }
    let seek_penalties = Arc::new(seek_penalties);

    // #61 — Write a "scan in flight" marker checkpoint to disk
    // BEFORE Stage 1 starts. Without this, a mid-inventory kill
    // leaves NOTHING on disk for the next launch's Resume modal
    // to detect — the engine silently re-walks from scratch and
    // the user has no idea the prior session ever existed. The
    // marker holds the current roots + settings + empty
    // saved_inventory + empty previous_duplicates. On the next
    // launch, `gui::app::SuperdeduperApp::new()` detects the
    // marker via `checkpoint::summary` and pops the existing
    // Resume / Start Fresh modal — the modal already renders
    // "inventory not yet saved" gracefully when has_saved_inventory
    // is false.
    //
    // User experience:
    //   * Resume → engine relaunches; marker found but has no
    //     saved_inventory → walker starts from scratch (no actual
    //     state to resume to mid-inventory; the marker only buys
    //     the modal surface). This is honest — incremental
    //     mid-walk checkpoint saves are a separate, larger fix
    //     (periodic saves during the walker callback).
    //   * Start Fresh → marker archived to .bak; fresh launch.
    //
    // Resume from a marker is identical in code path to Start
    // Fresh — both lead to "walker runs from scratch." The
    // difference is purely UX: the user knows the prior session
    // existed instead of being silently re-walked.
    //
    // Resume case (prior had real state): checkpoint_state was
    // already hydrated from prior.previous_duplicates +
    // prior.saved_inventory above. Re-saving here is idempotent
    // — overwrites the same content, doesn't lose anything.
    if let Some(p) = &checkpoint_path {
        if let Err(e) = checkpoint::save(p, &checkpoint_state) {
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Warn,
                message: format!(
                    "pre-inventory marker checkpoint save failed: {e} \
                     (mid-inventory kills won't surface a Resume modal)"
                ),
            });
        }
    }

    // ---------------- Stage 1: inventory ----------------
    // Resume fast-path: if we have saved_inventory from a prior
    // run, skip the walk entirely. Pre-this-block we always walked
    // even on resume and just discarded the result post-walk, which
    // made resumes pay full Stage 1 cost for no benefit.
    let mut files: Vec<crate::inventory::FileEntry> = Vec::new();
    let mut dirs_entered: u64 = 0;
    let mut dirs_denied: u64 = 0;
    let mut entries_skipped: u64 = 0;
    let mut skipped_below_min: u64 = 0;
    let walk_skipped = if let Some(saved) = resumed_inventory.take() {
        files = saved;
        let _ = tx.send(EngineEvent::Status(
            "Stage 1 — using saved inventory from prior run".into(),
        ));
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "Resume · skipping Stage 1 walk entirely: {} file(s) loaded from saved inventory",
                files.len()
            ),
        });
        true
    } else {
        let _ = tx.send(EngineEvent::Status("Stage 1 — scanning files".into()));
        let _ = tx.try_send(EngineEvent::OverallProgress {
            stage: OverallStage::Inventory,
            done: 0,
            total: 0,
            eta_secs: None,
        });
        false
    };
    if cancel.load(Ordering::Relaxed) {
        emit_paused(&tx);
        return Ok(());
    }
    if !walk_skipped {
        let inv_tx = tx.clone();
        let mut files_seen: u64 = 0;
        let mut last_emit = Instant::now();
        let inv_result = inventory::walk::enumerate_cancellable(&cfg, Some(&*cancel), |evt| {
            use crate::inventory::walk::WalkEvent;
            match evt {
                WalkEvent::Entered { path, depth } => {
                    dirs_entered += 1;
                    if last_emit.elapsed() > std::time::Duration::from_millis(250) {
                        last_emit = Instant::now();
                        let display = path.display().to_string();
                        let _ = inv_tx.try_send(EngineEvent::Status(format!(
                            "Walking: {} ({} dirs, {} files so far)",
                            truncate_tail(&display, 60),
                            dirs_entered,
                            files_seen,
                        )));
                        let _ = inv_tx.try_send(EngineEvent::StageTick {
                            stage: Stage::Inventory,
                            delta: 0,
                            total: files_seen,
                        });
                    }
                    let _ = depth;
                }
                WalkEvent::FileFound { size: _, .. } => {
                    files_seen += 1;
                    if files_seen.is_multiple_of(200) {
                        let _ = inv_tx.try_send(EngineEvent::StageTick {
                            stage: Stage::Inventory,
                            delta: 200,
                            total: files_seen,
                        });
                        let _ = inv_tx.try_send(EngineEvent::OverallProgress {
                            stage: OverallStage::Inventory,
                            done: files_seen,
                            total: 0,
                            eta_secs: None,
                        });
                    }
                }
                WalkEvent::DirError { path, message } => {
                    dirs_denied += 1;
                    let _ = inv_tx.send(EngineEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("dir {}: {}", path.display(), message),
                    });
                }
                WalkEvent::EntrySkipped { reason, .. } => {
                    entries_skipped += 1;
                    if reason == "below min-size" {
                        skipped_below_min += 1;
                    }
                }
                WalkEvent::SymlinkCycleSkipped { from, target } => {
                    // T1.7: surface in the log so users see WHICH
                    // alias triggered the cycle-skip. Doesn't count
                    // against entries_skipped — those are "files we
                    // declined" while this is "a dir we already
                    // enumerated via another path."
                    let _ = inv_tx.send(EngineEvent::Log {
                        level: LogLevel::Info,
                        message: format!(
                            "symlink cycle skipped: {} → {} (already enumerated)",
                            from.display(),
                            target.display()
                        ),
                    });
                }
            }
        });
        files = match inv_result {
            Ok(v) => v,
            Err(e) => {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Error,
                    message: format!("inventory failed: {e}"),
                });
                return Err(e);
            }
        };
    }
    // Volume-guid backfill: cache lookups require volume_guid. The
    // walker's Windows fast path (Block N) populates it via
    // FileIdBothDirectoryInfo, but the fallback path leaves it None.
    // Files in saved_inventory inherit whatever the original walk
    // recorded — None propagates indefinitely across resumes, so
    // Stage 4 silently re-hashes everything because cache_key()
    // short-circuits to None when volume_guid is missing.
    //
    // Resolve per-root once, stamp every file with None. Cheap: one
    // GetVolumePathNameW + GetVolumeNameForVolumeMountPointW pair
    // per unique root.
    #[cfg(windows)]
    {
        let mut root_guid_cache: hashbrown::HashMap<std::path::PathBuf, Option<String>> =
            hashbrown::HashMap::new();
        let mut stamped = 0u64;
        for f in files.iter_mut() {
            if f.volume_guid.is_some() {
                continue;
            }
            // Find which scan root contains this file. roots is
            // typically small (1-4 entries), linear scan is fine.
            let owner = root_paths.iter().find(|r| f.path.starts_with(r));
            if let Some(root) = owner {
                let guid = root_guid_cache
                    .entry(root.clone())
                    .or_insert_with(|| crate::winapi_wrappers::volume_for_path(root).ok());
                if let Some(g) = guid {
                    f.volume_guid = Some(g.clone());
                    stamped += 1;
                }
            }
        }
        if stamped > 0 {
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "post-walk volume_guid backfill: stamped {} file(s) so cache lookups can fire on resume",
                    stamped
                ),
            });
        }
    }
    let with_guid = files.iter().filter(|f| f.volume_guid.is_some()).count();
    let _ = tx.send(EngineEvent::Log {
        level: if with_guid == files.len() {
            LogLevel::Info
        } else {
            LogLevel::Warn
        },
        message: format!(
            "inventory volume_guid census: {}/{} files have volume_guid (cache fast-forward needs ALL of them)",
            with_guid,
            files.len()
        ),
    });
    // Persist the inventory NOW so a subsequent pause during hashing
    // doesn't have to re-walk on the next resume. Cheap: just clones
    // path + size + mtime; serialisation happens inside the next
    // checkpoint::save call below in the hashing loop.
    checkpoint_state.saved_inventory = Some(saved_files_from_runtime(&files));
    // G1: compute corpus_signature_hash from the inventory's file
    // sizes — path/content-free per leaderboard-spec §6. Two users
    // scanning the same canonical corpus produce the same hash;
    // useful for detecting "ran the official bench corpus" vs random
    // data. Held in `corpus_sig` for the ScanFinished payload build.
    #[cfg(feature = "telemetry")]
    let corpus_sig: String = {
        let sizes: Vec<u64> = files.iter().map(|f| f.size).collect();
        crate::leaderboard_corpus_sig(&sizes)
    };
    #[cfg(not(feature = "telemetry"))]
    let corpus_sig: String = String::new();
    let _ = &corpus_sig; // used below when telemetry feature is on
                         // Write the checkpoint to disk right now, BEFORE any hashing
                         // starts. Previously the first persist happened at the end of
                         // chunk 0, which meant a hard-kill mid-chunk-0 lost the entire
                         // walk and forced a fresh re-walk on the next launch. Saving
                         // here narrows the loss window to "during the walk itself" —
                         // everything after Stage 1 completes survives a process kill.
    if let Some(p) = &checkpoint_path {
        if let Err(e) = checkpoint::save(p, &checkpoint_state) {
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Warn,
                message: format!("post-walk checkpoint save failed: {e}"),
            });
        }
    }
    let total_files = files.len() as u64;
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::Inventory,
        delta: 0,
        total: total_files,
    });
    // Always emit a summary line so the user sees a definitive
    // "what happened during inventory" — regardless of whether we
    // found anything.
    let _ = tx.send(EngineEvent::Log {
        level: if total_files == 0 { LogLevel::Warn } else { LogLevel::Info },
        message: format!(
            "inventory done · {} files · {} dirs walked · {} dirs denied · {} entries skipped ({} below min-size)",
            total_files, dirs_entered, dirs_denied, entries_skipped, skipped_below_min,
        ),
    });
    if let Some(d) = &diag {
        d.log(
            "STAGE",
            format_args!(
                "inventory-done files={total_files} dirs={dirs_entered} \
                 denied={dirs_denied} skipped={entries_skipped} below_min={skipped_below_min}"
            ),
        );
    }
    if total_files == 0 {
        let mut hint = "Inventory returned 0 files.".to_string();
        if dirs_denied > 0 {
            hint.push_str(&format!(
                " {} director(ies) were permission-denied — try running superdeduper-gui as administrator.",
                dirs_denied
            ));
        }
        if skipped_below_min > 0 {
            hint.push_str(&format!(
                " {} file(s) were below the min-size filter ({} bytes) — drop it via ⚙ Settings.",
                skipped_below_min, cfg.min_size,
            ));
        }
        if dirs_denied == 0 && skipped_below_min == 0 && dirs_entered <= 1 {
            hint.push_str(" The root appears empty or the volume isn't accessible.");
        }
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Warn,
            message: hint,
        });
    }

    if cancel.load(Ordering::Relaxed) {
        emit_paused(&tx);
        return Ok(());
    }

    // G1.x: client-claimed predicate evaluation. Runs at end of
    // Stage 1 while `files` (the full post-walker inventory) is
    // still owned by this scope — Stage 2's group_by_size moves
    // it. Matched predicate IDs are stashed in `easter_egg_hits`
    // for inclusion in the leaderboard payload at end-of-scan.
    // Without the `telemetry` feature this is a no-op + the
    // payload code path is also gated off, so the Vec stays as
    // a zero-cost empty slot.
    // #149 — shared with the CLI scan path so neither can drift.
    #[cfg(feature = "telemetry")]
    let easter_egg_hits: Vec<String> =
        crate::leaderboard::predicates::compute_easter_egg_hits(&files);

    // ---------------- Stage 2: size grouping ----------------
    let _ = tx.send(EngineEvent::Status("Stage 2 — size grouping".into()));
    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::SizeGroup,
        done: 0,
        total: 0,
        eta_secs: None,
    });
    // #25 v3 wiring — clone the inventory before `group_by_size`
    // consumes it, but only when Tier-4 will actually run. Default
    // mode (Exact) skips the clone so the byte-identical path stays
    // zero-cost.
    #[cfg(feature = "similar-images")]
    let inventory_for_tier4 = if matches!(scan_mode, crate::cli::ScanMode::Image) {
        Some(files.clone())
    } else {
        None
    };
    // Audio mirror — same clone-only-when-needed shape.
    #[cfg(feature = "similar-audio")]
    let inventory_for_tier4_audio = if matches!(scan_mode, crate::cli::ScanMode::Audio) {
        Some(files.clone())
    } else {
        None
    };
    let mut size_groups = pipeline::grouping::group_by_size(files);
    // Resolve inode ids only on files that survived size grouping —
    // singletons can't be hardlinks within this scan and don't need
    // the per-file GetFileInformationByHandle. See the docs on
    // `pipeline::grouping::resolve_file_ids`.
    pipeline::grouping::resolve_file_ids(&mut size_groups);
    // #99 PR6 — pre-Stage-4 filter: drop files that are already
    // members of restored dup groups. Those files' membership is
    // already proven (the group sits in state.duplicates from
    // PR5's preservation) — re-hashing them would be wasted work.
    // After the filter, size classes that drop below 2 members
    // get pruned (a single-file class can't produce dups).
    //
    // Empty restored_dup_paths (fresh scan) is a no-op.
    let restored_skipped: u64 = if !restored_dup_paths.is_empty() {
        let mut skipped: u64 = 0;
        for g in size_groups.iter_mut() {
            let before = g.files.len();
            g.files.retain(|f| !restored_dup_paths.contains(&f.path));
            skipped = skipped.saturating_add((before - g.files.len()) as u64);
        }
        size_groups.retain(|g| g.files.len() >= 2);
        if skipped > 0 {
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "Resume fast-forward: skipping {skipped} file(s) already in restored dup groups (already-confirmed; no re-hash needed)"
                ),
            });
        }
        skipped
    } else {
        0
    };
    let size_candidates: u64 = size_groups.iter().map(|g| g.files.len() as u64).sum();
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::SizeGroup,
        delta: size_candidates,
        total: size_candidates,
    });
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "size grouping: {} candidate(s) in {} size class(es)",
            size_candidates,
            size_groups.len()
        ),
    });
    if let Some(d) = &diag {
        d.log(
            "STAGE",
            format_args!(
                "size-grouping-done candidates={size_candidates} classes={}",
                size_groups.len()
            ),
        );
    }

    if cancel.load(Ordering::Relaxed) {
        emit_paused(&tx);
        return Ok(());
    }

    // ---------------- Stage 3: layout ----------------
    let _ = tx.send(EngineEvent::Status("Stage 3 — layout".into()));
    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::Layout,
        done: 0,
        total: 0,
        eta_secs: None,
    });
    let laid = pipeline::layout::resolve(size_groups)?;
    let laid_count: u64 = laid.iter().map(|g| g.files.len() as u64).sum();
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::LayoutResolve,
        delta: laid_count,
        total: laid_count,
    });

    // ---------------- Stage 4: progressive hashing ----------------
    let cache = if cfg.use_cache {
        crate::cache::default_cache_path()
            .and_then(|p| Cache::open(&p))
            .ok()
            .map(|c| Arc::new(Mutex::new(c)))
    } else {
        None
    };
    // Surface cache state at Stage 4 start so users (and the
    // diagnostics log) can see whether the fast-forward path is
    // available on a resume. If the cache is None here, every file
    // will be re-hashed regardless of whether the rusqlite cache file
    // on disk has matching rows.
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: match (cfg.use_cache, cache.is_some()) {
            (true, true) => {
                "cache enabled — Stage 4 will fast-forward through already-hashed files".to_string()
            }
            (true, false) => {
                "cache requested but failed to open — Stage 4 will re-hash everything".to_string()
            }
            (false, _) => {
                "cache disabled in settings — Stage 4 will re-hash everything".to_string()
            }
        },
    });
    // #100 — surface cache state at Stage 4 start so resume-run
    // diagnostics don't have to wait for scan-finish summary.
    // PR8 — keep the diagnostic emit but DROP PR7's bar-credit
    // logic. PR7 used cache_rows as an initial bar offset, but:
    //   (a) it over-credits — the cache holds rows from earlier
    //       sessions of this corpus, not just the just-killed
    //       scan, so Mick's count showed ~10% higher than reality
    //   (b) the max() floor froze the bar for the cache-hit
    //       window, making it LOOK like nothing was happening
    // The accurate position comes from cache hits as they
    // actually fire during Stage 4 — bar climbs continuously,
    // no over-credit, no freeze. Trade-off accepted: bar starts
    // at restored_skipped (4%) and climbs through cache hits to
    // the prior position over ~minutes (SQLite mutex bottleneck
    // caps per-file lookup throughput). Pre-flight bulk cache
    // load is the v0.2.11 perf fix that makes this near-instant.
    if let Some(c) = &cache {
        if let Ok(cache_path) = crate::cache::default_cache_path() {
            if let Ok(stats) = c.lock().stats(&cache_path) {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "cache state at Stage 4 start: {} hash row(s), {} inventory row(s), {} bytes on disk",
                        stats.rows,
                        stats.snapshot_rows,
                        stats.bytes_on_disk
                    ),
                });
            }
        }
        // #99 PR9 — warm the in-memory mirror. Bulk-loads every
        // cache row for cfg.hash_algo into a HashMap on the Cache
        // struct. Subsequent lookup_detailed calls check the
        // HashMap first and skip SQLite — ~1000x per-call speedup
        // turns mutex-bound 300/sec lookups into ~100k/sec.
        // For Mick's killed-at-25% scenario: 126k rows warm in
        // ~100ms, then cache-hit phase completes in seconds.
        let warm_started = Instant::now();
        match c.lock().warm_in_place(cfg.hash_algo) {
            Ok(n) => {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "cache warmed: {n} row(s) loaded into in-memory mirror in {} ms (per-file lookup now lock-free-equivalent)",
                        warm_started.elapsed().as_millis()
                    ),
                });
            }
            Err(e) => {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Warn,
                    message: format!(
                        "cache warm-load failed: {e} — falling back to per-file SQLite lookup (slower path)"
                    ),
                });
            }
        }
    }
    let mut total_cache_hits: u64 = 0;
    // #99 PR3 — Tally of files whose cache row existed but
    // invalidated at lookup time (size / mtime / usn drift). Summed
    // across chunks; surfaced at scan-finish as the
    // "X files re-validated after FS changes" status line so resume
    // runs don't silently appear to restart-at-zero per #52.
    let mut total_cache_drift_misses: u64 = 0;
    let mut total_cache_writes: u64 = 0;
    // #106 PR2 — Surface Err returns from Cache::store so a stuck or
    // corrupt cache DB shows up in the scan-finish line instead of
    // silently swallowing writes.
    let mut total_cache_write_failures: u64 = 0;

    // #99 PR11 — Pre-flight predicted-hit count against the warm
    // map. Lets the initial Stage-4 OverallProgress emit JUMP the
    // bar to the pre-kill position immediately instead of climbing
    // up to it over the fast-forward window. PR7 attempted this
    // using raw cache_rows as credit, but over-counted because the
    // cache table can hold rows for files not in the current scan;
    // PR11 lookups each laid file against the warm map and counts
    // only true Hit outcomes (size+mtime+usn match), so the credit
    // matches the actual fast-forward exactly.
    let predicted_cache_hits: u64 = if let Some(c) = &cache {
        let preflight_started = Instant::now();
        let keys: Vec<crate::cache::CacheKey> = laid
            .iter()
            .flat_map(|g| g.files.iter())
            .filter_map(|f| crate::pipeline::hash::cache_key(f, cfg.hash_algo))
            .collect();
        let key_count = keys.len();
        let hits = c.lock().predict_hits(&keys);
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "Resume pre-flight: {hits} predicted cache hit(s) over {key_count} laid file(s) in {} ms — bar will be credited at frame-zero so it doesn't climb during fast-forward",
                preflight_started.elapsed().as_millis()
            ),
        });
        hits
    } else {
        0
    };

    // Smaller chunks → more frequent updates between chunks. We also
    // wire a per-file progress callback into the hasher so the UI
    // animates *within* a chunk, not just between them.
    //
    // A-perf-chunks-h_new Path B (testdesign + design 2026-06-06 00:18
    // PDT): SUPERDEDUPER_CHUNK_SIZE env-var override. sdd-testwin's
    // 90805d1 perf-channel matrix EMPIRICALLY RULED OUT channel
    // back-pressure as the 217s engine-in-GUI slowdown; new prime
    // suspect is chunked-par-iter scheduling overhead (~1000 chunks
    // x ~217 ms per-chunk fixed cost). Lets sdd-testwin sweep
    // chunk_size 50/100/250/500/1000 in a single matrix to find the
    // optimal point + map the per-chunk-overhead curve. Default
    // unchanged (50) so unset behavior is identical.
    let chunk_max = chunk_size_max();
    crate::log_info!("GUI scan: chunk_groups max_chunk_size={chunk_max} (default=500; SUPERDEDUPER_CHUNK_SIZE override)");
    let chunks = chunk_groups(laid, 32, chunk_max);
    let total_chunks = chunks.len();

    // #195 perf — build the stage-4 io thread pool ONCE here and
    // share it across every chunk via run_cancellable_with_pool.
    // Pre-fix, each chunk built its own pool (CreateThread x
    // cfg.io_threads + join on scope-exit). On Windows that's tens
    // of ms per chunk; with ~1000 chunks on Mick's C:\sdd-tests
    // corpus the per-chunk pool churn was eating a chunk of the
    // GUI-vs-CLI gap (CLI builds the pool once for the whole run).
    // Fall back to fresh-per-chunk if build fails so a broken pool
    // setup doesn't strand the scan.
    let shared_io_pool = match pipeline::hash::build_io_pool(&cfg) {
        Ok(p) => Some(p),
        Err(e) => {
            crate::log_warn!(
                "GUI scan: shared io_pool build failed ({e}); falling back to per-chunk pools"
            );
            None
        }
    };
    let total_to_hash = laid_count;
    let hashing_started = Instant::now();
    let _ = tx.send(EngineEvent::Status(format!(
        "Stage 4 — hashing {} chunk(s)…",
        total_chunks
    )));
    // #99 PR6+PR8 — bar reflects prior position via the
    // restored_skipped credit only (files in restored dup groups,
    // filtered out of size_groups above). PR7's cache_credit
    // addition was reverted — see the PR8 comment block on the
    // cache-stats emit above for the rationale. Bar climbs
    // continuously through cache hits from this initial position
    // as the per-file callback bumps `n` (cached + fresh alike).
    // #99 PR11 — initial done credits restored_skipped (PR6) +
    // predicted_cache_hits (PR11). On a fresh scan both are 0 and
    // the bar starts at the origin; on a resume, both jump the bar
    // straight to the pre-kill position before chunk 1 runs.
    let initial_done = restored_skipped.saturating_add(predicted_cache_hits);
    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::Hashing,
        done: initial_done,
        total: total_to_hash.saturating_add(restored_skipped),
        eta_secs: None,
    });
    // #99 PR10 — frame-zero bar emit. Lets a paste-back of the
    // log capture the bar position BEFORE chunk 1 runs, so the
    // jump-vs-climb of the fast-forward is unambiguous.
    {
        let initial_adjusted_total = total_to_hash.saturating_add(restored_skipped);
        let initial_bar_pct = if initial_adjusted_total > 0 {
            (initial_done as f64 / initial_adjusted_total as f64) * 100.0
        } else {
            0.0
        };
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "Stage 4 bar frame-zero: bar {initial_bar_pct:.2}% ({initial_done}/{initial_adjusted_total} files) · total_to_hash={total_to_hash}, restored_dup_skip={restored_skipped}, predicted_cache_hits={predicted_cache_hits}, total_chunks={total_chunks}"
            ),
        });
    }

    // #108 — Seed engine local accumulators from `prior` so the
    // submission payload reflects cumulative work across the resume
    // chain, not just post-resume work. Pre-#108 these all started
    // at 0 on every spawn including resumes, so a pause+resume
    // sequence under-reported `bytes_scanned`, `duplicate_groups`,
    // and `duplicate_bytes_reclaimable` to the leaderboard backend
    // — restored-dup-group files (PR6) were filtered before the
    // chunk loop on the engine side but counted in the user-visible
    // Groups tab (PR5). The asymmetry didn't trip the backend 422
    // (clamp at line ~1893 protects), it just under-credited
    // resume-cluster users on the leaderboard. Closes #108.
    let mut total_bytes_read: u64 = prior_cumulative_bytes_scanned;
    let mut total_dups: u64 = prior_cumulative_dups;
    let mut reclaimable: u64 = prior_cumulative_reclaimable;
    let mut reclaimable_inode: u64 = prior_cumulative_reclaimable;
    // #49 — per-SimilarityKind group counts; incremented every time
    // we bump `total_dups`. Persisted in the scan-history record at
    // ScanFinished so the History tab can show "32 perceptual + 30
    // byte-identical" rather than "62 groups total."
    let mut groups_by_similarity_kind: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut largest_group_bytes: u64 = 0;
    // #162 -- A-run-shape-esoterics-streaming: the 3 esoteric
    // run_shape metrics (zero_byte_group_max, max_hardlink_count_in_scan,
    // name_collision_count) are now accumulated through the shared
    // streaming type in `leaderboard::payload_meta` so the GUI emitter
    // and the CLI batch path (`payload_meta::run_shape_esoterics`)
    // share ONE algorithm. Previously the GUI had inline state for
    // each metric -- drift surface eliminated.
    let mut run_shape_esoterics_accum =
        crate::leaderboard::payload_meta::RunShapeEsotericsAccumulator::new();
    let mut tier3_done: u64 = 0;
    let mut confirmed: u64 = 0;
    let files_hashed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bytes_hashed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let hash_failures = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Per-tier accumulators rolled up across all chunks. Each
    // HashCounters returned from run_cancellable is chunk-local; we
    // sum them here so the post-scan diagnostics line carries the
    // whole-scan picture.
    let mut tier_micros_total: [u64; 4] = [0; 4];
    let mut tier_bytes_total: [u64; 4] = [0; 4];
    let mut tier_count_total: [u64; 4] = [0; 4];
    // T2.1 phase 7 — placeholder skip counters, summed across chunks.
    let mut placeholders_blocked_recall_total: u64 = 0;
    let mut placeholders_blocked_other_reparse_total: u64 = 0;
    // Per-tier attempt counters (one slot for Tier 0..3). Shared
    // ownership across rayon worker threads via Arc; updated
    // lock-free by the per-file callback.
    let tier_counts: [Arc<std::sync::atomic::AtomicU64>; 4] = [
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
    ];

    // Counters the diagnostics sampler reads every 10 seconds. They
    // share Arc<AtomicU64> ownership with the rayon callback so reads
    // are lock-free even under contention.
    let diag_counters = Arc::new(EngineCounters {
        files_hashed: Arc::clone(&files_hashed),
        bytes_hashed: Arc::clone(&bytes_hashed),
        hash_failures: Arc::clone(&hash_failures),
        tier_counts: [
            Arc::clone(&tier_counts[0]),
            Arc::clone(&tier_counts[1]),
            Arc::clone(&tier_counts[2]),
            Arc::clone(&tier_counts[3]),
        ],
        ..EngineCounters::default()
    });
    diag_counters
        .candidates
        .store(total_to_hash, Ordering::Relaxed);
    diag_counters.stage.store(4, Ordering::Relaxed);
    let sampler_stop = Arc::new(AtomicBool::new(false));
    let sampler_handle = diag.clone().map(|log| {
        diagnostics::spawn_state_sampler(
            log,
            Arc::clone(&diag_counters),
            std::time::Duration::from_secs(10),
            Arc::clone(&sampler_stop),
        )
    });
    if let Some(d) = &diag {
        d.log(
            "STAGE",
            format_args!("hashing-start chunks={total_chunks} candidates={total_to_hash}"),
        );
    }

    let already_reported: hashbrown::HashSet<String> =
        checkpoint_state.completed_hashes.iter().cloned().collect();

    // A-perf-pc-decouple (v0.3.40, reduced-scope per design 08:35 PDT
    // ratify): spawn a single runner thread that aggregates dup-group
    // summaries from the chunk loop and emits batched
    // EngineEvent::DuplicatesFoundBatch at ~100ms cadence. Closes the
    // 258ms/chunk emit cost sdd-testwin measured: each per-group
    // tx.send(EngineEvent::DuplicateFound(summary)) inside the loop
    // hit the GUI state lock + UI render + accesskit tree update
    // synchronously; on Mick C:\sdd-tests with ~30K dup groups across
    // ~600 chunks that summed to ~154s of post-chunk emit work. The
    // runner pops summaries from an unbounded crossbeam channel into
    // a Vec<DuplicateGroupSummary> batch and ships ONE event per
    // 100ms-or-200-groups trigger; the GUI's UiState::apply arm for
    // DuplicatesFoundBatch (added in 2344711) does the equivalent
    // dedup + push under a single lock acquire.
    //
    // Runner intentionally does NOT poll cancel; it consumes everything
    // until the chunk loop drops its sum_tx Sender and the channel
    // disconnects, so partial-scan progress is preserved for the
    // checkpoint semantics on cancel paths (per testdesign 08:13 PDT
    // note 2 in the Phase 3 plan LGTM).
    let (sum_tx, sum_rx) =
        crossbeam_channel::unbounded::<crate::gui::events::DuplicateGroupSummary>();
    let mut sum_tx_opt: Option<crossbeam_channel::Sender<crate::gui::events::DuplicateGroupSummary>> =
        Some(sum_tx);
    let runner_tx = tx.clone();
    let mut runner_handle: Option<thread::JoinHandle<()>> = Some(thread::spawn(move || {
        let mut batch: Vec<crate::gui::events::DuplicateGroupSummary> = Vec::with_capacity(256);
        let mut last_emit = Instant::now();
        let mut batches_emitted: u64 = 0;
        let mut groups_emitted: u64 = 0;
        let runner_started = Instant::now();
        loop {
            let recv = sum_rx.recv_timeout(Duration::from_millis(100));
            let disconnected = matches!(
                recv,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected)
            );
            if let Ok(summary) = recv {
                batch.push(summary);
            }
            let now = Instant::now();
            let should_flush = !batch.is_empty()
                && (disconnected
                    || batch.len() >= 200
                    || now.duration_since(last_emit) >= Duration::from_millis(100));
            if should_flush {
                let drained = std::mem::take(&mut batch);
                groups_emitted = groups_emitted.saturating_add(drained.len() as u64);
                batches_emitted = batches_emitted.saturating_add(1);
                let _ = runner_tx.send(EngineEvent::DuplicatesFoundBatch(drained));
                batch = Vec::with_capacity(256);
                last_emit = now;
            }
            if disconnected {
                break;
            }
        }
        // perf-streaming emit (testdesign 08:13 PDT plan note 1): single
        // line scan-finish target so sdd-testwin's hermetic matrix can
        // verify engine throughput in GUI mode approaches CLI engine
        // wall post-fix. Same SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1
        // gate as perf-chunks / perf-channel for consistency.
        if crate::gui::app::perf_instrument_update_enabled() && groups_emitted > 0 {
            crate::log_info!(
                "perf-streaming: runner_wall_ms={:.3} batches={} groups_emitted={}",
                runner_started.elapsed().as_secs_f64() * 1000.0,
                batches_emitted,
                groups_emitted,
            );
        }
    }));

    // A-perf-chunks-h_new (testdesign ASK 4, 2026-06-06 00:11 PDT):
    // accumulate per-chunk wall + setup/hash/emit phases so the
    // scan-finish emit decomposes the 217s GUI-vs-CLI throughput
    // gap into chunk-loop-overhead vs hash-work vs emit. CLI runs ONE
    // chunk all-at-once; GUI runs ~1000 chunks; if chunk_setup +
    // chunk_emit aggregate is large, per-chunk fixed cost is load-
    // bearing. p50/p99 give the per-chunk distribution shape.
    let chunk_loop_started = std::time::Instant::now();
    let mut chunk_walls_ns: Vec<u128> = Vec::with_capacity(total_chunks);
    let mut chunk_setup_ns_total: u128 = 0;
    let mut chunk_hash_ns_total: u128 = 0;
    let mut chunk_emit_ns_total: u128 = 0;

    // v0.3.41 chunk_emit decomp (design 2026-06-06 spec §3.1 6 named
    // buckets). Cached env-var read; perf_chunk_emit_enabled() is a
    // OnceLock-cached single bool branch. When OFF, every per-bucket
    // accumulator stays zero + the emit line is skipped + the
    // Instant::now() / elapsed calls are short-circuited via the gated
    // helper closures below.
    let perf_chunk_emit = crate::gui::app::perf_instrument_chunk_emit_enabled();

    // v0.3.41 Phase 4 fix: hand the periodic checkpoint::save off to a
    // background worker. The 22:55 PDT matrix verdict showed
    // checkpoint::save dominated chunk_emit at 99.974% (21.9s of
    // 22.4s); moving it to a bg thread eliminates ~22s from the chunk
    // loop's critical path on Mick-corpus.
    //
    // Single-slot replace semantics (see SaveWorker docs): chunk loop
    // calls enqueue() with a fresh snapshot; bg thread saves the
    // latest. Older still-pending snapshots are dropped (we only need
    // crash-recovery from the most-recent state). The cancellation /
    // Interrupted paths SHUTDOWN the worker before doing the sync
    // cumulative-aware save -- that preserves the durability semantic
    // (pause-time saves are still sync + carry cumulative_*).
    let mut saver: Option<checkpoint::SaveWorker> = checkpoint_path
        .as_ref()
        .map(|p| checkpoint::SaveWorker::spawn(p.clone()));

    for (i, chunk) in chunks.into_iter().enumerate() {
        let chunk_t_start = std::time::Instant::now();
        // v0.3.41 chunk_emit decomp: per-chunk u64 accumulators (us).
        // Reset every chunk; emit one line per chunk when
        // perf_chunk_emit is ON (spec §3.1).
        let mut checkpoint_save_us: u64 = 0;
        let mut checkpoint_record_us: u64 = 0;
        let mut tx_send_dup_us: u64 = 0;
        let mut tx_try_send_us: u64 = 0;
        let mut tx_send_log_us: u64 = 0;
        let mut cache_stats_us: u64 = 0;
        let mut group_count: u64 = 0;
        if cancel.load(Ordering::Relaxed) {
            // v0.3.41 Phase 4: shutdown bg saver BEFORE the sync save
            // below so the bg-thread doesn't race the cumulative-aware
            // sync save on the same path. Drops any stale pending
            // snapshot; the upcoming sync save carries the latest
            // (post-cumulative) state.
            if let Some(mut s) = saver.take() {
                s.shutdown();
            }
            if let Some(p) = &checkpoint_path {
                checkpoint_state.cumulative_bytes_scanned = total_bytes_read;
                // #108-extended — preserve files + wall_clock
                // cumulatively across resume chains too, so the
                // backend's throughput + IOPS sanity checks
                // (which divide by wall_clock) don't trip when
                // bytes is cumulative but the others are per-spawn.
                checkpoint_state.cumulative_files_scanned = total_files;
                checkpoint_state.cumulative_wall_clock_seconds =
                    _scan_started_at.elapsed().as_secs() + prior_cumulative_wall_clock_seconds;
                if let Err(e) = checkpoint::save(p, &checkpoint_state) {
                    let _ = tx.send(EngineEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("checkpoint save failed: {e}"),
                    });
                }
            }
            sampler_stop.store(true, Ordering::Relaxed);
            if let Some(d) = &diag {
                let n = files_hashed.load(Ordering::Relaxed);
                let f = hash_failures.load(Ordering::Relaxed);
                d.log(
                    "SCAN-PAUSED",
                    format_args!(
                        "n_hashed={n} hash_failures={f} dups={total_dups} reclaimable={reclaimable}"
                    ),
                );
                d.finalize(format_args!(
                    "paused at chunk {}/{} · {total_dups} dup group(s)",
                    i + 1,
                    total_chunks
                ));
            }
            // A-perf-pc-decouple: cancel-from-top runner shutdown so
            // any in-flight batch ships + the thread exits cleanly
            // before run() returns.
            drop(sum_tx_opt.take());
            if let Some(handle) = runner_handle.take() {
                let _ = handle.join();
            }
            emit_paused(&tx);
            return Ok(());
        }
        let progress_tx = tx.clone();
        let progress_files = Arc::clone(&files_hashed);
        let progress_bytes = Arc::clone(&bytes_hashed);
        let progress_failures = Arc::clone(&hash_failures);
        let progress_drive = (i as u32) % roots.len().max(1) as u32;
        let progress_drive_is_hdd = seek_penalties
            .get(progress_drive as usize)
            .copied()
            .unwrap_or(true);
        let total_to_hash_inner = total_to_hash;
        let hashing_started_inner = hashing_started;
        // #99 PR6 — capture the resume skip count so the in-loop
        // OverallProgress emit offsets `done` + `total` by it.
        let progress_restored_skipped = restored_skipped;
        // #99 PR11 — capture the pre-flight predicted-hit count so
        // the in-loop emit can floor `done` at the pre-credited
        // position. Bar stays at the credit during fast-forward
        // (n < predicted), then climbs normally once disk-bound
        // work begins (n > predicted).
        let progress_predicted_cache_hits = predicted_cache_hits;
        let progress_diag = diag.clone();
        let progress_tier_counts = [
            Arc::clone(&tier_counts[0]),
            Arc::clone(&tier_counts[1]),
            Arc::clone(&tier_counts[2]),
            Arc::clone(&tier_counts[3]),
        ];
        let on_file: pipeline::hash::FileProgress = Arc::new(move |path, tier, outcome| {
            // Bump the per-tier attempt counter (success+cache+fail).
            // The funnel reads these so each tier shows its own
            // narrowing count instead of all rolling into Tier 3.
            let tier_idx = (tier as usize).min(3);
            let n_tier = progress_tier_counts[tier_idx].fetch_add(1, Ordering::Relaxed) + 1;

            // Headline progress only counts Tier 1 (which fires
            // for every candidate file exactly once). Counting
            // every tier invocation would let `n` overshoot
            // `total` (a single file goes through T0+T1+T2+T3).
            let counts_for_progress = tier == 1;
            let n = if counts_for_progress {
                progress_files.fetch_add(1, Ordering::Relaxed) + 1
            } else {
                progress_files.load(Ordering::Relaxed)
            };
            // Credit both fresh-hashed AND cache-hit bytes — the
            // user-visible throughput graph reflects "scan progress
            // speed" (cache fast-forward + real hashing both count
            // as work done), and the leaderboard payload's
            // bytes_scanned needs the cache-hit count too or it
            // undercounts vs reclaimable_bytes and trips the
            // backend's result_self_consistency sanity check.
            let bytes_added = match &outcome {
                pipeline::hash::ProgressOutcome::Hashed { bytes }
                | pipeline::hash::ProgressOutcome::Cached { bytes } => *bytes,
                _ => 0,
            };
            let total_bytes = progress_bytes
                .fetch_add(bytes_added, Ordering::Relaxed)
                .saturating_add(bytes_added);

            // Surface unreadable files in the Log tab. Without
            // this they were dropped silently and the progress
            // bar's "done" count never reached the total.
            if let pipeline::hash::ProgressOutcome::Failed { error } = &outcome {
                let f = progress_failures.fetch_add(1, Ordering::Relaxed) + 1;
                if f <= 50 {
                    let _ = progress_tx.try_send(EngineEvent::Log {
                        level: LogLevel::Warn,
                        message: format!("hash failed · {} · {error}", display_path(path),),
                    });
                } else if f == 51 {
                    let _ = progress_tx.try_send(EngineEvent::Log {
                        level: LogLevel::Warn,
                        message: "…suppressing further per-file hash failures (see counter)".into(),
                    });
                }
                if let Some(d) = &progress_diag {
                    d.log_hash_failure(path, error);
                }
            }

            // Drive-scope dot positioning. On SSDs we use a stable
            // path-hash for Y (scattered "TV snow"); on HDDs we
            // use the cumulative-bytes climb (clean diagonal).
            //
            // 2026-06-02 perf cleanup (design iterate-freely 17:15
            // PDT; pre-empting hypothesis 2/3/5 investigation):
            // hash_path_to_lcn(path) is a BLAKE3 hash on the path
            // string -- ~1-2us per call. Pre-fix it was computed on
            // EVERY callback (per-file-per-tier) but only USED inside
            // the modulus-gated try_send (every 10th call on SSD;
            // every 50th on HDD). 90-98% of the BLAKE3 work was
            // discarded. Moved inside the modulus-gated branch.
            // Per-file cost change for the dominant SSD case on a
            // 312K-file scan: ~280K wasted BLAKE3 calls -> 0.
            // SUPERDEDUPER_THROTTLE_STATE_EMIT_DURING_SCAN (Cand 1
            // experiment, see state_emit_throttle_mult fn): multiplies
            // modulus 10x when env var is set so events drop 10x.
            let read_modulus = (if progress_drive_is_hdd { 50 } else { 10 }) * state_emit_throttle_mult();
            if n.is_multiple_of(read_modulus) {
                let lcn_bytes = if progress_drive_is_hdd {
                    total_bytes
                } else {
                    hash_path_to_lcn(path)
                };
                let _ = progress_tx.try_send(EngineEvent::Read(ReadSample {
                    drive: progress_drive,
                    lcn_bytes,
                    bytes: bytes_added,
                    latency_us: 1,
                    at: Instant::now(),
                }));
            }
            // Per-tier funnel tick — uses the tier-local counter
            // and the matching Stage enum so the funnel rows
            // narrow as you'd expect: T0 ≥ T1 ≥ T2 ≥ T3.
            let stage_modulus = 100 * state_emit_throttle_mult();
            if n_tier.is_multiple_of(stage_modulus) {
                let stage = match tier {
                    0 => Stage::Tier0Format,
                    1 => Stage::Tier1Head,
                    2 => Stage::Tier2HeadMidTail,
                    _ => Stage::Tier3Full,
                };
                let _ = progress_tx.try_send(EngineEvent::StageTick {
                    stage,
                    delta: stage_modulus,
                    total: n_tier,
                });
            }

            // Headline OverallProgress + ETA: only Tier 1 advances.
            let progress_modulus = 100 * state_emit_throttle_mult();
            if counts_for_progress && n.is_multiple_of(progress_modulus) {
                let elapsed = hashing_started_inner.elapsed().as_secs_f32();
                // #99 PR6+PR8 — bar math:
                //   done = restored_skipped + n
                //   total = total_to_hash + restored_skipped
                // restored_skipped credits files in already-
                // confirmed dup groups (PR6); n bumps for every
                // Tier 1 callback (cache hits + fresh hashes
                // alike), so the bar climbs continuously from
                // restored_skipped through the cache-hit window
                // (visibly) and into the fresh-hash window.
                // #99 PR11 — floor `done` at the pre-credited
                // position. Without the max(), `done` would dip
                // back to (n + restored) at chunk 1 (n=0), undoing
                // the frame-zero jump and producing a visible
                // bar-drop. With it, the bar stays at the credit
                // until actual n catches up, then climbs naturally.
                let adjusted_done = n
                    .saturating_add(progress_restored_skipped)
                    .max(progress_restored_skipped.saturating_add(progress_predicted_cache_hits));
                let adjusted_total = total_to_hash_inner.saturating_add(progress_restored_skipped);
                let frac = if total_to_hash_inner > 0 {
                    (n as f32 / total_to_hash_inner as f32).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let eta = if frac > 0.001 {
                    Some((elapsed * (1.0 - frac) / frac).max(0.0))
                } else {
                    None
                };
                let _ = progress_tx.try_send(EngineEvent::OverallProgress {
                    stage: OverallStage::Hashing,
                    done: adjusted_done,
                    total: adjusted_total,
                    eta_secs: eta,
                });
            }
        });

        let chunk_t_pre_hash = std::time::Instant::now();
        let chunk_result = if let Some(pool) = shared_io_pool.as_ref() {
            pipeline::hash::run_cancellable_with_pool(
                chunk,
                &cfg,
                cache.clone(),
                on_file,
                Arc::clone(&cancel),
                pool,
            )
        } else {
            pipeline::hash::run_cancellable(
                chunk,
                &cfg,
                cache.clone(),
                on_file,
                Arc::clone(&cancel),
            )
        };
        let chunk_t_post_hash = std::time::Instant::now();
        let (dups, counters) = match chunk_result {
            Ok(v) => v,
            Err(crate::Error::Io(e)) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Cancellation came from the per-file Tier 3 streaming
                // path — it surfaces as Interrupted before the
                // chunks-loop top can see the cancel atomic. Save the
                // checkpoint with whatever progress we have, then
                // emit_paused and exit cleanly. Without this, the
                // outer `?` would propagate the cancel as a scan
                // failure and the checkpoint would never write — the
                // exact bug that breaks resume after a mid-hash cancel.
                //
                // v0.3.41 Phase 4: same shutdown-before-sync-save
                // discipline as the top-of-loop cancel above so the bg
                // thread doesn't race the cumulative-aware save.
                if let Some(mut s) = saver.take() {
                    s.shutdown();
                }
                if let Some(p) = &checkpoint_path {
                    checkpoint_state.cumulative_bytes_scanned = total_bytes_read;
                    // #108-extended — preserve files + wall_clock
                    // cumulatively across resume chains too, so the
                    // backend's throughput + IOPS sanity checks
                    // (which divide by wall_clock) don't trip when
                    // bytes is cumulative but the others are per-spawn.
                    checkpoint_state.cumulative_files_scanned = total_files;
                    checkpoint_state.cumulative_wall_clock_seconds =
                        _scan_started_at.elapsed().as_secs() + prior_cumulative_wall_clock_seconds;
                    if let Err(e) = checkpoint::save(p, &checkpoint_state) {
                        let _ = tx.send(EngineEvent::Log {
                            level: LogLevel::Warn,
                            message: format!("checkpoint save failed: {e}"),
                        });
                    }
                }
                sampler_stop.store(true, Ordering::Relaxed);
                if let Some(d) = &diag {
                    let n = files_hashed.load(Ordering::Relaxed);
                    let f = hash_failures.load(Ordering::Relaxed);
                    d.log(
                        "SCAN-PAUSED",
                        format_args!(
                            "n_hashed={n} hash_failures={f} dups={total_dups} \
                             reclaimable={reclaimable} reason=mid-chunk-cancel"
                        ),
                    );
                    d.finalize(format_args!(
                        "paused mid-chunk at {}/{} · {total_dups} dup group(s)",
                        i + 1,
                        total_chunks
                    ));
                }
                // A-perf-pc-decouple: mid-chunk cancel runner shutdown
                // mirrors the top-of-loop path; ensures any final batch
                // ships + the thread exits before run() returns.
                drop(sum_tx_opt.take());
                if let Some(handle) = runner_handle.take() {
                    let _ = handle.join();
                }
                emit_paused(&tx);
                return Ok(());
            }
            Err(e) => {
                drop(sum_tx_opt.take());
                if let Some(handle) = runner_handle.take() {
                    let _ = handle.join();
                }
                // v0.3.41 Phase 4: shutdown bg saver on error exit too
                // so the bg thread doesn't outlive run() (Drop would
                // catch this too via SaveWorker::drop, but explicit
                // shutdown here surfaces any panic visible to the
                // caller).
                if let Some(mut s) = saver.take() {
                    s.shutdown();
                }
                return Err(e);
            }
        };
        let chunk_bytes = counters.bytes_read.load(Ordering::Relaxed);
        total_bytes_read = total_bytes_read.saturating_add(chunk_bytes);
        total_cache_hits =
            total_cache_hits.saturating_add(counters.cache_hits.load(Ordering::Relaxed));
        total_cache_writes =
            total_cache_writes.saturating_add(counters.cache_writes.load(Ordering::Relaxed));
        // #99 PR3 — Sum the per-file drift counter across chunks so
        // the scan-finish summary can surface re-validation count.
        total_cache_drift_misses = total_cache_drift_misses
            .saturating_add(counters.cache_drift_misses.load(Ordering::Relaxed));
        // #106 PR2 — Sum cache_write_failures across chunks for the
        // scan-finish counters line.
        total_cache_write_failures = total_cache_write_failures
            .saturating_add(counters.cache_write_failures.load(Ordering::Relaxed));
        for i in 0..4 {
            tier_micros_total[i] = tier_micros_total[i]
                .saturating_add(counters.tier_micros[i].load(Ordering::Relaxed));
            tier_bytes_total[i] =
                tier_bytes_total[i].saturating_add(counters.tier_bytes[i].load(Ordering::Relaxed));
            tier_count_total[i] =
                tier_count_total[i].saturating_add(counters.tier_count[i].load(Ordering::Relaxed));
        }
        placeholders_blocked_recall_total = placeholders_blocked_recall_total
            .saturating_add(counters.placeholders_blocked_recall.load(Ordering::Relaxed));
        placeholders_blocked_other_reparse_total = placeholders_blocked_other_reparse_total
            .saturating_add(
                counters
                    .placeholders_blocked_other_reparse
                    .load(Ordering::Relaxed),
            );

        // v0.3.40 A-perf-pc-decouple Phase 4 (Mick GO 2026-06-06 ship-
        // today): parallelize the per-group processing across rayon's
        // global pool. sdd-testwin's v0.3.40 reduced-scope (aa140a8)
        // matrix showed chunk_emit_ms_total ~24s on Mick-corpus = the
        // bottleneck is the SERIAL for-g-in-dups loop, where each
        // iteration's order_keeper_first calls file_mtime() (stat
        // syscall per file). ~30K groups x ~800us serial = 24s wall.
        //
        // Splitting via par_iter lets the stat-bound work run across
        // the global rayon pool (CPU-sized); the cheap serial fold
        // afterwards updates aggregates (atomics + BTreeMap entry +
        // checkpoint::record + sum_tx send) in a few ms total. With
        // 8 logical cores parallelizing the stat work, projected wall
        // for the per-chunk emit block drops from ~24s to ~3-5s on
        // Mick-corpus = engine wall approaches CLI parity per design's
        // <=1.10x criteria (lock-in 2026-06-06 12:28 PDT).
        struct ProcessedGroup {
            summary: DuplicateGroupSummary,
            savings: u64,
            group_reclaim: u64,
        }
        let keep_strategy = settings.keep_strategy;
        let reference_set_ref = &reference_set;
        let already_reported_ref = &already_reported;
        let processed: Vec<ProcessedGroup> = dups
            .into_par_iter()
            .filter_map(|g| {
                if already_reported_ref.contains(&g.content_hash) {
                    return None;
                }
                let visible_files = filter_reference_only(g.files, reference_set_ref);
                if visible_files.len() < 2 {
                    return None;
                }
                let savings = g
                    .size
                    .saturating_mul(visible_files.len().saturating_sub(1) as u64);
                let unique_inodes = if g.unique_inodes == 0 {
                    visible_files.len() as u64
                } else {
                    g.unique_inodes
                };
                let group_reclaim = if !g.link_equivalent && unique_inodes > 1 {
                    g.size.saturating_mul(unique_inodes.saturating_sub(1))
                } else {
                    0
                };
                let ordered_files =
                    order_keeper_first(visible_files, keep_strategy, reference_set_ref);
                Some(ProcessedGroup {
                    summary: DuplicateGroupSummary {
                        size: g.size,
                        content_hash: g.content_hash,
                        files: ordered_files,
                        link_equivalent: g.link_equivalent,
                        unique_inodes: g.unique_inodes,
                        similarity_kind: crate::pipeline::SimilarityKind::ByteIdentical,
                    },
                    savings,
                    group_reclaim,
                })
            })
            .collect();

        for out in processed {
            let g_size = out.summary.size;
            let g_link_equivalent = out.summary.link_equivalent;
            confirmed += 1;
            reclaimable = reclaimable.saturating_add(out.savings);
            reclaimable_inode = reclaimable_inode.saturating_add(out.group_reclaim);
            // `largest_single_group_bytes` per backend's sanity
            // check is the largest group's RECLAIM, not its total
            // bytes-on-disk. Backend rule: largest <= reclaim total.
            if out.group_reclaim > largest_group_bytes {
                largest_group_bytes = out.group_reclaim;
            }
            // #162 -- A-run-shape-esoterics-streaming: feed the group
            // through the shared accumulator. Same algorithm the CLI
            // uses via `payload_meta::run_shape_esoterics`.
            run_shape_esoterics_accum.add_group(
                g_size,
                &out.summary.content_hash,
                g_link_equivalent,
                out.summary.files.iter(),
            );
            total_dups += 1;
            *groups_by_similarity_kind
                .entry("byte-identical".to_string())
                .or_insert(0) += 1;
            // Keep the diagnostics counters in sync so the 10s
            // sampler thread sees fresh values without us holding
            // a lock.
            diag_counters
                .confirmed_dups
                .store(confirmed, Ordering::Relaxed);
            diag_counters
                .reclaimable_bytes
                .store(reclaimable, Ordering::Relaxed);
            // v0.3.41 chunk_emit decomp: bucket #2 checkpoint_record_us
            // (sum across this chunk's groups). Spec §3.1.
            let rec_started = if perf_chunk_emit {
                Some(std::time::Instant::now())
            } else {
                None
            };
            checkpoint_state.record(&out.summary);
            if let Some(s) = rec_started {
                checkpoint_record_us =
                    checkpoint_record_us.saturating_add(s.elapsed().as_micros() as u64);
            }
            // A-perf-pc-decouple (v0.3.40): runner thread batches +
            // emits DuplicatesFoundBatch instead of per-group blocking
            // send. Send to sum_tx is cheap (unbounded channel push;
            // no GUI lock + no UI re-render until batched flush).
            //
            // v0.3.41 chunk_emit decomp: bucket #3 tx_send_dup_us. The
            // PR #170 spec hypothesis §2.3 was the blocking
            // tx.send(DuplicateFound) per group; v0.3.40 already moved
            // to the batched sum_tx_opt unbounded channel, but the
            // bucket name + measurement window are preserved for
            // continuity with the spec interpretation tree §4.2.
            let send_started = if perf_chunk_emit {
                Some(std::time::Instant::now())
            } else {
                None
            };
            if let Some(sender) = sum_tx_opt.as_ref() {
                let _ = sender.send(out.summary);
            }
            if let Some(s) = send_started {
                tx_send_dup_us =
                    tx_send_dup_us.saturating_add(s.elapsed().as_micros() as u64);
            }
            group_count = group_count.saturating_add(1);
        }

        // Periodic checkpoint flush so an unexpected crash doesn't
        // lose progress beyond the last chunk.
        //
        // v0.3.41 Phase 4: handoff to bg thread. enqueue() replaces
        // the bg-thread's pending snapshot with the current state
        // (single-slot semantics; older still-pending snapshots are
        // dropped since they're superseded). Clone cost is small
        // relative to the 255ms JSON-encode + tempfile write that
        // checkpoint::save used to do inline.
        //
        // v0.3.41 chunk_emit decomp: bucket #1 checkpoint_save_us now
        // measures the enqueue path (clone + mutex briefly + notify),
        // not the JSON encode + disk write. Spec §3.1 / §2.1
        // dominant-bucket hypothesis = pre-#162 era assumed
        // cheap-relative-to-hashing; bg-thread offload restores that
        // invariant by moving the disk-bound work off the chunk loop.
        let save_started = if perf_chunk_emit {
            Some(std::time::Instant::now())
        } else {
            None
        };
        if let Some(s) = saver.as_ref() {
            s.enqueue(checkpoint_state.clone());
        }
        if let Some(s) = save_started {
            checkpoint_save_us = s.elapsed().as_micros() as u64;
        }

        tier3_done += 1;
        // v0.3.41 chunk_emit decomp: bucket #4 tx_try_send_us. Per spec
        // §3.1 + testdesign gap #1: timing window wraps the FULL
        // Status-emission lifecycle including the preceding format!()
        // build (so format-allocation cost is captured in the same
        // bucket as the try_sends -- avoids invisible residual). Spec
        // also calls for summing the StageTick try_sends here.
        let try_send_started = if perf_chunk_emit {
            Some(std::time::Instant::now())
        } else {
            None
        };
        // Cross-chunk ticks: try_send so we never queue behind the
        // per-file progress samples that share this channel.
        let _ = tx.try_send(EngineEvent::StageTick {
            stage: Stage::Tier3Full,
            delta: 1,
            total: tier3_done,
        });
        let _ = tx.try_send(EngineEvent::StageTick {
            stage: Stage::Confirmed,
            delta: 0,
            total: confirmed,
        });
        // #104 Gap 3 — surface cache-hit progress in the live Status
        // bar (not just the every-10-chunk Log line) so a user
        // watching a paused-then-resumed scan can see fast-forward
        // happening in real-time. Only included when the cache is
        // doing meaningful work (resume situation); fresh scans
        // would just show "0 cached" noise otherwise.
        let fresh_so_far = tier_count_total[0]
            .saturating_add(tier_count_total[1])
            .saturating_add(tier_count_total[2])
            .saturating_add(tier_count_total[3]);
        let cache_suffix = if total_cache_hits > 0 || predicted_cache_hits > 0 {
            format!(" · {total_cache_hits} cached / {fresh_so_far} fresh")
        } else {
            String::new()
        };
        let _ = tx.try_send(EngineEvent::Status(format!(
            "Hashing chunk {}/{} · {} duplicate group(s) so far{}",
            i + 1,
            total_chunks,
            total_dups,
            cache_suffix
        )));
        if let Some(s) = try_send_started {
            tx_try_send_us = s.elapsed().as_micros() as u64;
        }
        // #100 — periodic cache-stats emit during Stage 4 so a user
        // watching a resume run can see in real-time whether the
        // cache fast-forward is happening (vs waiting for scan-
        // finish). Fires every 10 chunks; cheap (atomic loads).
        //
        // v0.3.41 chunk_emit decomp: bucket #6 cache_stats_us (the
        // compute: hit-rate + bar-position + format!) + bucket #5
        // tx_send_log_us (the blocking tx.send for the Log event).
        // Spec §3.1 + testdesign gap #2 (explicit named bucket;
        // estimated ~9% of chunk_emit_ms on Mick-corpus; would surface
        // as residual + trip the 5% sum-conservation threshold if not
        // separately accounted).
        if (i + 1).is_multiple_of(10) {
            let cache_started = if perf_chunk_emit {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let total_so_far = total_cache_hits.saturating_add(
                tier_count_total[0]
                    .saturating_add(tier_count_total[1])
                    .saturating_add(tier_count_total[2])
                    .saturating_add(tier_count_total[3]),
            );
            let hit_rate_so_far = if total_so_far > 0 {
                (total_cache_hits as f64 / total_so_far as f64) * 100.0
            } else {
                0.0
            };
            // #99 PR10 — surface the bar position alongside chunk
            // position so a paste-back of the log lets us correlate
            // what the user sees in the GUI against engine state.
            // PR11 — same floor as the in-loop emit so the log %
            // matches the GUI %.
            let n_now = files_hashed.load(Ordering::Relaxed);
            let adjusted_done_now = n_now
                .saturating_add(restored_skipped)
                .max(restored_skipped.saturating_add(predicted_cache_hits));
            let adjusted_total_now = total_to_hash.saturating_add(restored_skipped);
            let bar_pct = if adjusted_total_now > 0 {
                (adjusted_done_now as f64 / adjusted_total_now as f64) * 100.0
            } else {
                0.0
            };
            // v0.3.41: do the format!() BEFORE timing the tx.send so
            // cache_stats_us captures the formatting cost + the
            // tx_send_log_us bucket captures just the channel send.
            // (Bumping format!() into cache_stats_us is intentional --
            // it's part of the cache-stats compute surface.)
            let log_message = format!(
                "cache so far: {total_cache_hits} hit(s), {total_cache_drift_misses} drift miss(es), {} fresh hash(es) — {hit_rate_so_far:.1}% hit rate (chunk {}/{}) · bar {bar_pct:.2}% ({adjusted_done_now}/{adjusted_total_now} files, restored_dup_skip={restored_skipped}, predicted_cache_hits={predicted_cache_hits})",
                total_so_far.saturating_sub(total_cache_hits),
                i + 1,
                total_chunks,
            );
            if let Some(s) = cache_started {
                cache_stats_us = s.elapsed().as_micros() as u64;
            }
            let log_started = if perf_chunk_emit {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: log_message,
            });
            if let Some(s) = log_started {
                tx_send_log_us = s.elapsed().as_micros() as u64;
            }
        }

        // A-perf-chunks-h_new: accumulate per-chunk phase durations.
        // Mid-chunk cancel paths return early via `return Ok(());`
        // above and never reach this; partial accumulators just
        // never emit -- acceptable since cancellation during a 4-min
        // scan is rare and partial data isn't decision-useful.
        let chunk_t_end = std::time::Instant::now();
        chunk_walls_ns.push(chunk_t_end.duration_since(chunk_t_start).as_nanos());
        chunk_setup_ns_total = chunk_setup_ns_total
            .saturating_add(chunk_t_pre_hash.duration_since(chunk_t_start).as_nanos());
        chunk_hash_ns_total = chunk_hash_ns_total
            .saturating_add(chunk_t_post_hash.duration_since(chunk_t_pre_hash).as_nanos());
        chunk_emit_ns_total = chunk_emit_ns_total
            .saturating_add(chunk_t_end.duration_since(chunk_t_post_hash).as_nanos());

        // v0.3.41 chunk_emit decomp: per-chunk emit line, gated.
        // Format per spec §3.1:
        //   perf-chunk-emit: chunk_idx checkpoint_save_us checkpoint_record_us
        //                    tx_send_dup_us tx_try_send_us tx_send_log_us
        //                    cache_stats_us group_count
        //
        // sdd-testwin matrix harvest aggregates these via the generic
        // (.*perf-.*:.*) regex; aggregation across chunks happens
        // offline per spec §3.2 sum-conservation invariant.
        if perf_chunk_emit {
            crate::log_info!(
                "perf-chunk-emit: chunk_idx={} checkpoint_save_us={} checkpoint_record_us={} tx_send_dup_us={} tx_try_send_us={} tx_send_log_us={} cache_stats_us={} group_count={}",
                i,
                checkpoint_save_us,
                checkpoint_record_us,
                tx_send_dup_us,
                tx_try_send_us,
                tx_send_log_us,
                cache_stats_us,
                group_count,
            );
        }
    }

    // A-perf-pc-decouple (v0.3.40): chunk loop done -- drop sum_tx so
    // the runner thread's recv_timeout sees Disconnected on its next
    // tick, flushes its final batch (if any) via DuplicatesFoundBatch,
    // emits the perf-streaming summary line, and exits. Join is
    // best-effort: a runner panic shouldn't break scan-finish, but the
    // join must complete before ScanFinished so the GUI's terminal
    // event lands after any final batch. Early-return paths upstream
    // (cancellation / Interrupted) shut down the runner via the same
    // take()-and-drop pattern before returning.
    drop(sum_tx_opt.take());
    if let Some(handle) = runner_handle.take() {
        if let Err(e) = handle.join() {
            crate::log_warn!("dup-runner panicked: {e:?}");
        }
    }

    // v0.3.41 Phase 4: drain + join the bg checkpoint-save worker.
    // The worker may have a final pending snapshot from the last
    // chunk's enqueue; shutdown() drains it before joining so the
    // very-latest crash-recovery state lands on disk before
    // checkpoint::delete() removes it on clean scan-finish below.
    // (Save-then-delete is harmless; the delete supersedes the save
    // for the clean-exit path.)
    if let Some(mut s) = saver.take() {
        s.shutdown();
    }

    // A-perf-chunks-h_new (testdesign ASK 4, 2026-06-06 00:11 PDT):
    // emit the per-chunk phase decomposition for the just-completed
    // chunk loop. Gated by SUPERDEDUPER_PERF_INSTRUMENT_UPDATE=1 so
    // sdd-testwin's hermetic harness picks it up automatically. One
    // line at scan-finish (not per-frame) -- grep target for the
    // Mick-corpus 217s engine-in-GUI slowdown root-cause analysis.
    if crate::gui::app::perf_instrument_update_enabled() && !chunk_walls_ns.is_empty() {
        let chunk_loop_total_ms = chunk_loop_started.elapsed().as_secs_f64() * 1000.0;
        chunk_walls_ns.sort_unstable();
        let n = chunk_walls_ns.len();
        let p50 = chunk_walls_ns[n / 2];
        let p99 = chunk_walls_ns[(n.saturating_sub(1) * 99) / 100];
        crate::log_info!(
            "perf-chunks: chunks_total={} chunk_setup_ms_total={:.3} chunk_wall_p50_ms={:.3} chunk_wall_p99_ms={:.3} chunk_hash_ms_total={:.3} chunk_emit_ms_total={:.3} chunk_loop_total_ms={:.3}",
            n,
            chunk_setup_ns_total as f64 / 1_000_000.0,
            p50 as f64 / 1_000_000.0,
            p99 as f64 / 1_000_000.0,
            chunk_hash_ns_total as f64 / 1_000_000.0,
            chunk_emit_ns_total as f64 / 1_000_000.0,
            chunk_loop_total_ms,
        );
    }

    // Scan finished cleanly — the checkpoint has served its purpose.
    if let Some(p) = &checkpoint_path {
        let _ = checkpoint::delete(p);
    }

    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::Finishing,
        done: files_hashed.load(Ordering::Relaxed).max(total_to_hash),
        total: total_to_hash.max(1),
        eta_secs: Some(0.0),
    });
    // Emit log lines BEFORE ScanFinished so listeners that break
    // on ScanFinished (UI, tests) still see them. Order matters:
    // ScanFinished is the terminal signal.
    // Use inode-aware reclaim here so this user-visible line agrees
    // with the header tile (which also uses inode-aware via
    // gui::state::inode_aware_savings). The diagnostic line below
    // prints both flavors for debugging hardlink-heavy corpora.
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "scan complete: {} group(s), {} reclaimable",
            total_dups,
            crate::gui::theme::humansize(reclaimable_inode)
        ),
    });
    // #81 — Surface exclusion stats so the user sees the safe-
    // defaults filter actually fired. Only emit when something was
    // excluded; a "0 excluded" line on a clean corpus is noise.
    let excl = cfg.exclusion_counters.snapshot();
    if excl.excluded_files > 0 {
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "exclusions: skipped {} file(s) / {} (per safe-defaults filter)",
                excl.excluded_files,
                crate::gui::theme::humansize(excl.excluded_bytes),
            ),
        });
    }
    // Cache stats so a resumed scan that *should* have fast-
    // forwarded but didn't is obvious from the log. Hit rate near
    // zero on a resume is the smoking gun for a cache-key mismatch.
    let total_hash_ops = total_cache_hits.saturating_add(
        tier_count_total[0]
            .saturating_add(tier_count_total[1])
            .saturating_add(tier_count_total[2])
            .saturating_add(tier_count_total[3]),
    );
    let hit_rate = if total_hash_ops > 0 {
        (total_cache_hits as f64 / total_hash_ops as f64) * 100.0
    } else {
        0.0
    };
    // #106 PR2 — Counters line: hits / drift-misses / writes /
    // write-failures consolidated, plus fresh-hashes + hit-rate as the
    // headline metrics. Pre-#106 the line silently lost the Err path
    // of `Cache::store` and only reported writes that succeeded.
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "cache: {} hits, {} drift-misses, {} writes, {} write-failures, {} fresh hashes — {:.1}% hit rate",
            total_cache_hits,
            total_cache_drift_misses,
            total_cache_writes,
            total_cache_write_failures,
            total_hash_ops.saturating_sub(total_cache_hits),
            hit_rate
        ),
    });
    // #99 PR3 — Per testdesign B2-USN spec. When a resumed scan
    // finds cache rows that drifted (size/mtime/usn changed since
    // the prior hash), surface the count so the user understands
    // why progress visibly restarted from low cache-hit ratio
    // instead of the expected fast-forward. Closes #52: the
    // restart-at-zero symptom now has an explicit "X files
    // re-validated after FS changes" log line.
    if total_cache_drift_misses > 0 {
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "cache: {total_cache_drift_misses} file(s) re-validated after FS changes \
                 (cache row existed but size/mtime/usn drifted — file modified between scans)"
            ),
        });
    }
    // Diagnostic: surface both reclaim flavors + bytes_read at
    // scan-end so a "reclaim > read" report (hardlink-heavy corpora,
    // partial-hardlink groups with stale unique_inodes, etc.) is
    // one log-line away. The leaderboard payload clamps reclaim
    // against bytes_read; this line shows the raw figures.
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "reclaim: path-aware={}, inode-aware={}, bytes-read={} — {} groups",
            crate::gui::theme::humansize(reclaimable),
            crate::gui::theme::humansize(reclaimable_inode),
            crate::gui::theme::humansize(total_bytes_read),
            total_dups,
        ),
    });
    // #82 — Hoist scan_id generation above the SubmissionInputs build
    // so it can be threaded into BOTH the inputs (for #82's
    // submission_id-back-onto-ScanRecord join) AND the
    // record_completed call below. Pre-#82 the id was generated only
    // at record_completed; now we share one id across both call
    // sites in this fn.
    let scan_id_for_this_run = crate::scan_history::new_scan_id();
    // #41 v3 — scan_history v2 persists the canonical payload so the
    // History tab's Resubmit button has something ready-to-POST. Built
    // inside the telemetry cfg block below (alongside the existing
    // pending-slot wiring) + threaded out via this Option so the
    // scan_history record-write site can attach it. Telemetry-off
    // builds leave this None; the row still writes, just without a
    // payload (Resubmit button stays disabled).
    #[cfg(feature = "telemetry")]
    let mut submission_payload_for_history: Option<(serde_json::Value, String)> = None;

    // G1: build the leaderboard payload from this scan's results
    // and log its size. We don't auto-submit — that's the GUI
    // "Submit run" button's job. This step proves the integration
    // works end-to-end: hardware detect + scan totals + corpus
    // signature → canonical-JSON + HMAC sign-ready payload.
    #[cfg(feature = "telemetry")]
    {
        use crate::leaderboard::hardware;
        use crate::leaderboard::hmac_signer;
        use crate::leaderboard::submission::{
            self, FEATURE_BIT_ALLOW_RECALL_ON_READ,
            FEATURE_BIT_ALLOW_SYSTEM_PATHS, FEATURE_BIT_CACHE, FEATURE_BIT_EXCLUDE_GLOB,
            FEATURE_BIT_FOLLOW_LINKS, FEATURE_BIT_FORMAT_AWARE, FEATURE_BIT_INCLUDE_GLOB,
            FEATURE_BIT_REFERENCE_ROOTS,
        };
        // Discard the defender post probe; current backend schema
        // doesn't carry defender state. Keep the call commented in
        // case a future schema reinstates it. Sig param is
        // `_`-prefixed because it's unused in non-telemetry builds.
        let _ = _defender_rtp_pre;
        // Wall-clock as seconds (number) per schema. Same `_`-prefix
        // reasoning as the param above.
        //
        // #108-extended — payload's wall_clock_seconds must be
        // chain-cumulative so the backend's throughput sanity check
        // (bytes_scanned / wall_clock_seconds ≤ disk_class_ceiling)
        // doesn't 422-reject the payload on resume chains. bytes is
        // cumulative via 51e4e1c's pre-bump; wall_clock here adds
        // the prior cumulative on top of this spawn's elapsed.
        let wall_clock_seconds =
            _scan_started_at.elapsed().as_secs_f64() + prior_cumulative_wall_clock_seconds as f64;
        let hash_algorithm = match settings.hash_algo {
            crate::pipeline::hash::HashAlgo::Blake3 => "blake3",
            crate::pipeline::hash::HashAlgo::River5 => "river5-aes-ni",
        }
        .to_string();
        // Scope + corpus kind heuristics via the shared
        // `payload_meta` module (#142 — moved out of gui::live so
        // CLI's run_scan can compute identical values for its
        // submission payload build).
        let root_paths_only: Vec<std::path::PathBuf> =
            roots.iter().map(|r| r.path.clone()).collect();
        let scope = crate::leaderboard::payload_meta::classify_scope(&root_paths_only);
        let corpus_kind =
            crate::leaderboard::payload_meta::classify_corpus_kind(&root_paths_only);
        // Features bitmap built from the resolved settings.
        let mut features_bits: u64 = 0;
        if settings.use_cache {
            features_bits |= FEATURE_BIT_CACHE;
        }
        if settings.use_format_aware {
            features_bits |= FEATURE_BIT_FORMAT_AWARE;
        }
        if cfg.follow_links {
            features_bits |= FEATURE_BIT_FOLLOW_LINKS;
        }
        if cfg.allow_system_paths {
            features_bits |= FEATURE_BIT_ALLOW_SYSTEM_PATHS;
        }
        if cfg.allow_recall_on_read {
            features_bits |= FEATURE_BIT_ALLOW_RECALL_ON_READ;
        }
        if !cfg.reference_roots.is_empty() {
            features_bits |= FEATURE_BIT_REFERENCE_ROOTS;
        }
        if cfg.include.is_some() {
            features_bits |= FEATURE_BIT_INCLUDE_GLOB;
        }
        if cfg.exclude.is_some() {
            features_bits |= FEATURE_BIT_EXCLUDE_GLOB;
        }
        // Cache hit ratio: tier-totals tracked above; ratio of
        // cache_hits to total hash ops attempted.
        let cache_hit_ratio = if total_hash_ops > 0 {
            Some(total_cache_hits as f64 / total_hash_ops as f64)
        } else {
            None
        };

        // #162 -- A-run-shape-esoterics-streaming: finalize the
        // shared accumulator now that the dup-group emission loop
        // is done. Same triple shape as the CLI batch path:
        // (zero_byte_group_max, max_hardlink_count_in_scan,
        // name_collision_count).
        let (zero_byte_group_max, max_hardlink_count_in_scan, name_collision_count) =
            run_shape_esoterics_accum.finalize();

        // Codex-review item 2 (v0.3.25): the field-name boilerplate
        // is consolidated in payload_meta::build_scan_submission_inputs.
        // The GUI passes the streaming-accumulator outputs + the
        // count_distinct_share_roots reading via root_paths; constant
        // fields (walker_variant, dry_run, bench, lane, etc.) come from
        // the helper.
        let inputs = crate::leaderboard::payload_meta::build_scan_submission_inputs(
            crate::leaderboard::payload_meta::ScanSubmissionArgs {
                scan_id: scan_id_for_this_run.clone(),
                // #88 Phase 1 — pass the first scan root so filesystem
                // detection has a real path to probe instead of falling
                // back to the platform default. Unlocks pathfinder-refs
                // + network-pioneer signal classes.
                hardware: hardware::detect_with_root_hint(roots.first().map(|r| r.path.as_path())),
                wall_clock_seconds,
                bytes_scanned: total_bytes_read,
                files_scanned: total_files,
                hash_algorithm,
                scope,
                features_used_bitmap: features_bits,
                corpus_kind,
                cache_hit_ratio,
                easter_egg_hits,
                // #162 -- A-run-shape-esoterics-streaming: all 3
                // metrics come from the shared accumulator that fed
                // every emitted group above. Single source of truth
                // with the CLI batch path (`run_shape_esoterics`);
                // drift is no longer physically representable.
                zero_byte_group_max,
                max_hardlink_count_in_scan,
                name_collision_count,
                // #89 — count of distinct network-share roots in
                // scope (UNC `\\server\share`, smb://, nfs://).
                // Counted at the requested-root level so the value
                // reflects user intent, not whether files were
                // actually read. Backend uses this for the latent
                // `multi-share-maestro` grant.
                share_count_in_scope: {
                    let n = crate::leaderboard::payload_meta::count_distinct_share_roots(
                        &root_paths,
                    );
                    if n > 0 {
                        Some(n)
                    } else {
                        None
                    }
                },
                // Use inode-aware reclaim (collapses hardlink
                // aliases) for the leaderboard payload. Clamp to
                // bytes_scanned just in case some weird edge case
                // still produces reclaim > scanned (e.g. a file
                // counted as both alias and unique somehow); backend
                // sanity-rejects on that.
                duplicate_groups: total_dups,
                duplicate_bytes_reclaimable: reclaimable_inode.min(total_bytes_read),
                largest_single_group_bytes: largest_group_bytes.min(total_bytes_read),
                // #142 follow-up — populate placeholder_skip_count
                // from the running counters so the GUI submission
                // matches the CLI's. Pre-fix the field always
                // shipped None despite the data being available;
                // wire parity restored. `placeholder_skip_bytes`
                // stays None until the tier guard threads the
                // per-placeholder byte total (separate follow-up).
                placeholder_skip_count: {
                    let n = placeholders_blocked_recall_total
                        .saturating_add(placeholders_blocked_other_reparse_total);
                    if n > 0 {
                        Some(n)
                    } else {
                        None
                    }
                },
            },
        );
        // Diagnostic-only payload preview. install_id is empty string
        // here because the engine doesn't load the install state at
        // scan-end — the real submission flow re-loads it just-in-time
        // so the most recent value (post-reset, post-register) is used.
        // The byte-count printed below is approximate; the actual
        // submit-time body will include install_id.
        let payload = submission::build_payload(&inputs, "");
        let body = hmac_signer::canonical_body(&payload);

        // #41 — build the FULL submittable payload (with the real
        // install_id from the active install state) + stash it for
        // the scan_history record-write site below. Resubmit replays
        // this verbatim so the signature stays valid against the
        // install_id captured at build time. If the install state
        // can't load (unregistered, missing file), we leave the
        // History row without a payload — Resubmit stays disabled
        // + the user has a clear "register first" surface.
        if let Ok(Some(install_state)) = crate::leaderboard::install::load() {
            let full_payload = submission::build_payload(&inputs, &install_state.install_id);
            submission_payload_for_history = Some((full_payload, install_state.install_id));
        }

        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "leaderboard payload ready: {} bytes (run_uuid={}, hw={}/{}c)",
                body.len(),
                inputs.run_uuid.split('-').next().unwrap_or(""),
                inputs.hardware.cpu_model_string,
                inputs.hardware.cpu_threads,
            ),
        });
        // Stash the inputs in the engine→GUI handoff slot so the
        // post-scan "Submit run" button has something to send. The
        // global slot is overwritten on every scan-end — only the
        // freshest run is submittable from the UI.
        submission::store_pending(inputs);
        // A new run replaces any previous outcome's display state.
        submission::clear_last_outcome();
    }
    // #25 v3 GUI Tier-4 wiring. If user picked `--mode image`,
    // run perceptual-similarity grouping against the inventory
    // we cloned pre-`group_by_size`. Emit each tier4 group as a
    // DuplicateGroupSummary event so the GUI's groups table
    // surfaces them alongside byte-identical groups. Also update
    // the running totals so the ScanFinished payload (+ leaderboard
    // submission downstream) reflect the combined counts.
    #[cfg(feature = "similar-images")]
    if matches!(scan_mode, crate::cli::ScanMode::Image) {
        if let Some(inv) = inventory_for_tier4.as_deref() {
            let algo: crate::pipeline::image_hash::Algorithm = image_hash_algorithm.into();
            // E3 (#78): resolve auto-threshold from the count of
            // image-extension files in the inventory. Same shape as
            // main.rs's CLI scan-path resolution; comment there has
            // the n-vs-decoded-n discussion.
            let n_images = inv
                .iter()
                .filter(|f| crate::pipeline::image_hash::tier4::is_image_file(&f.path))
                .count() as u64;
            let resolved_threshold = image_similarity_threshold.resolve(
                crate::pipeline::image_hash::tier4::DEFAULT_THRESHOLD,
                n_images,
            );
            let t_tier4 = std::time::Instant::now();
            let tier4_groups = crate::pipeline::image_hash::tier4::find_similar_groups(
                inv,
                algo,
                resolved_threshold,
            );
            let n_groups = tier4_groups.len();
            for g in tier4_groups {
                // A-ref-keeper — reference-drive invariant for Tier-4
                // perceptual-image groups. Without this pair, a
                // perceptual match where R contains the centroid would
                // emit files[0]=non-reference and Mick would see R
                // demoted to dupe in the GUI. Drop the group entirely
                // if every member is under a reference root (nothing
                // to dedupe).
                let visible_files = filter_reference_only(g.files, &reference_set);
                if visible_files.len() < 2 {
                    continue;
                }
                let visible_files =
                    order_keeper_first(visible_files, settings.keep_strategy, &reference_set);
                total_dups += 1;
                *groups_by_similarity_kind
                    .entry("perceptual-image".to_string())
                    .or_insert(0) += 1;
                // Tier-4 group reclaim = (unique_inodes - 1) * size, same
                // shape as the byte-identical accumulator. Saturating
                // arithmetic so a pathological count can't overflow.
                let group_reclaim = g.unique_inodes.saturating_sub(1).saturating_mul(g.size);
                reclaimable_inode = reclaimable_inode.saturating_add(group_reclaim);
                reclaimable = reclaimable.saturating_add(group_reclaim);
                let summary = DuplicateGroupSummary {
                    size: g.size,
                    content_hash: g.content_hash,
                    files: visible_files,
                    link_equivalent: g.link_equivalent,
                    unique_inodes: g.unique_inodes,
                    similarity_kind: g.similarity_kind,
                };
                let _ = tx.send(EngineEvent::DuplicateFound(summary));
            }
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "Tier-4 perceptual ({}): {n_groups} group(s) within {resolved_threshold} bits ({} ms; n_images={n_images}, requested={image_similarity_threshold})",
                    algo.as_slug(),
                    t_tier4.elapsed().as_millis()
                ),
            });
        }
    }
    #[cfg(not(feature = "similar-images"))]
    let _ = (scan_mode, image_similarity_threshold, image_hash_algorithm);

    // #26 T1.3 GUI Tier-4 wiring. Audio analog of the image branch
    // above; runs when user picked `--mode audio`. Threshold comes
    // from the new --audio-similarity-threshold flag (GH #53)
    // threaded through ScanSettings on the CLI side; GUI hardcodes
    // czkawka's calibrated 5.0 bits/chunk default at the
    // app::launch_scan call site until the Settings widget for both
    // image + audio thresholds lands.
    #[cfg(feature = "similar-audio")]
    if matches!(scan_mode, crate::cli::ScanMode::Audio) {
        if let Some(inv) = inventory_for_tier4_audio.as_deref() {
            use crate::pipeline::audio_hash::tier4 as audio_tier4;
            let t_tier4 = std::time::Instant::now();
            let tier4_result = audio_tier4::find_similar_groups(inv, audio_similarity_threshold);
            let n_groups = tier4_result.groups.len();
            let short_skipped = tier4_result.short_skipped_count;
            for g in tier4_result.groups {
                // A-ref-keeper — reference-drive invariant for Tier-4
                // perceptual-audio groups. Mirrors the byte-identical +
                // perceptual-image filter pair above so a star-marked
                // root is honoured across all similarity tiers.
                let visible_files = filter_reference_only(g.files, &reference_set);
                if visible_files.len() < 2 {
                    continue;
                }
                let visible_files =
                    order_keeper_first(visible_files, settings.keep_strategy, &reference_set);
                total_dups += 1;
                *groups_by_similarity_kind
                    .entry("perceptual-audio".to_string())
                    .or_insert(0) += 1;
                let group_reclaim = g.unique_inodes.saturating_sub(1).saturating_mul(g.size);
                reclaimable_inode = reclaimable_inode.saturating_add(group_reclaim);
                reclaimable = reclaimable.saturating_add(group_reclaim);
                let summary = DuplicateGroupSummary {
                    size: g.size,
                    content_hash: g.content_hash,
                    files: visible_files,
                    link_equivalent: g.link_equivalent,
                    unique_inodes: g.unique_inodes,
                    similarity_kind: g.similarity_kind,
                };
                let _ = tx.send(EngineEvent::DuplicateFound(summary));
            }
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "Tier-4 acoustic: {n_groups} group(s) within {} bits/chunk avg ({} ms)",
                    audio_similarity_threshold,
                    t_tier4.elapsed().as_millis()
                ),
            });
            // #102 — surface <30s perceptual-skip count so users
            // understand why short voice memos / sound effects didn't
            // cluster perceptually. Byte-identical matching still ran
            // in Tier 0-3 — #103 confirmed those files aren't lost.
            if short_skipped > 0 {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Info,
                    message: format!(
                        "{short_skipped} audio file(s) too short for perceptual matching (<30s); processed via byte-identical tier only"
                    ),
                });
            }
        }
    }
    #[cfg(not(feature = "similar-audio"))]
    let _ = audio_similarity_threshold;

    // Use inode-aware reclaim — this is what overwrites
    // state.totals.reclaimable_bytes at scan-end (per state.rs's
    // ScanFinished handler). The path-aware `reclaimable` is kept
    // for engine-internal diagnostics; the user-facing total must
    // be inode-aware (true freeable bytes) so the header agrees
    // with the inline EngineEvent::DuplicateFound accumulation
    // and the leaderboard payload.
    let _ = tx.send(EngineEvent::ScanFinished {
        at: Instant::now(),
        total_files,
        total_bytes_read,
        duplicates: total_dups,
        reclaimable_bytes: reclaimable_inode,
    });

    // #38 v1: persist this scan to local history. Failure to write
    // doesn't kill the scan — log + continue. Resubmit + restore
    // are v2 territory; v1 just leaves a row the History panel can
    // surface to the user.
    {
        let channel_slug = crate::channel::active_channel().as_slug();
        let root_strings: Vec<String> = roots
            .iter()
            .map(|r| r.path.to_string_lossy().into_owned())
            .collect();
        #[cfg_attr(not(feature = "telemetry"), allow(unused_mut))]
        let mut record = crate::scan_history::ScanRecord::new_finished(
            scan_id_for_this_run.clone(),
            started_at_unix,
            channel_slug,
            root_strings,
            total_files,
            total_bytes_read,
            total_dups,
            reclaimable_inode,
            groups_by_similarity_kind.clone(),
        );
        // #41 — attach the canonical submission payload (built in
        // the telemetry block above) if it's available. The row
        // still persists when telemetry is off or install state
        // can't load — Resubmit stays disabled rather than the
        // row being absent from History.
        #[cfg(feature = "telemetry")]
        if let Some((payload, install_id)) = submission_payload_for_history.take() {
            record = record.with_submission_payload(payload, install_id);
        }
        if let Err(e) = crate::scan_history::record_completed(&record) {
            tracing::warn!(error = %e, "scan_history: record_completed failed (non-fatal)");
        }
    }
    // T2.1 phase 7 surface: tell the user how many files the tier
    // guard skipped, broken out by class. Silent when the corpus
    // had no placeholders (typical for non-OneDrive / non-WSL roots),
    // shown prominently otherwise so dropped dup-group counts
    // make sense at a glance.
    let placeholders_total =
        placeholders_blocked_recall_total.saturating_add(placeholders_blocked_other_reparse_total);
    if placeholders_total > 0 {
        // Only suggest the recall flag when there's actually a recall
        // placeholder to unlock — otherwise the hint misleads.
        let hint = if placeholders_blocked_recall_total > 0 {
            " (rerun with --allow-recall-on-read to include cloud stubs)"
        } else {
            ""
        };
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Warn,
            message: format!(
                "skipped {} placeholder file(s): {} cloud-recall, {} other reparse{}",
                placeholders_total,
                placeholders_blocked_recall_total,
                placeholders_blocked_other_reparse_total,
                hint,
            ),
        });
    }
    // Stop the diagnostics sampler and write the final summary.
    sampler_stop.store(true, Ordering::Relaxed);
    if let Some(h) = sampler_handle {
        let _ = h.join();
    }
    if let Some(d) = &diag {
        let n_final = files_hashed.load(Ordering::Relaxed);
        let f_final = hash_failures.load(Ordering::Relaxed);
        d.log(
            "SCAN-COMPLETE",
            format_args!(
                "files_inventory={total_files} candidates={total_to_hash} \
                 n_hashed={n_final} hash_failures={f_final} \
                 confirmed_dups={total_dups} reclaimable={reclaimable} \
                 bytes_read={total_bytes_read} hash_algo={}",
                cfg.hash_algo.tag()
            ),
        );
        // Per-tier wallclock breakdown — the single most useful line
        // when diagnosing "algo X is slower than algo Y". CPU-summed
        // microseconds + bytes hashed at each tier so MB/s/thread is
        // a one-line derivation.
        for (i, name) in ["t0_fmt", "t1_head", "t2_hmt", "t3_full"]
            .iter()
            .enumerate()
        {
            if tier_count_total[i] == 0 && tier_micros_total[i] == 0 {
                continue;
            }
            let micros = tier_micros_total[i];
            let bytes = tier_bytes_total[i];
            let count = tier_count_total[i];
            // Bytes per microsecond happens to read as MB/s.
            let mbps = if micros == 0 {
                0.0
            } else {
                bytes as f64 / micros as f64
            };
            d.log(
                "TIER-TIMING",
                format_args!(
                    "tier={name} algo={} files={count} bytes={bytes} cpu_us={micros} thru_mb_per_s={mbps:.0}",
                    cfg.hash_algo.tag()
                ),
            );
        }
        d.finalize(format_args!(
            "completed · {total_dups} dup group(s) · {reclaimable} bytes reclaimable · algo={}",
            cfg.hash_algo.tag()
        ));
    }
    Ok(())
}

fn truncate_tail(s: &str, n: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= n {
        return s.to_string();
    }
    let tail: String = chars[chars.len() - (n - 1)..].iter().collect();
    format!("…{tail}")
}

/// Volume GUID for the path's underlying volume, as the stable key
/// for persisted HDD/SSD render overrides. Returns `None` on
/// non-Windows or when the Win32 lookup fails — callers store the
/// empty string in that case and the override system skips
/// persistence for that drive.
/// Reorder a duplicate group's `files` list so the smart-heuristic
/// keeper lands at index 0. The rest stay in their original order
/// (we only swap the chosen keeper to position 0). This makes
/// `KeepStrategy::Smart` the implicit GUI default without changing
/// any downstream action handlers — safe-rename, recycle and
/// hardlink all treat files[0] as canonical.
///
/// A-ref-keeper — reference-drive invariant (hard rule, beats every
/// strategy): if any file is under a reference root, that file MUST
/// be the keeper. Matches CLI `dedupe::pick_keeper` early-return
/// (dedupe.rs:753-758) and preserves the index-0 placement that
/// `filter_reference_only` establishes for byte-identical groups —
/// without this short-circuit, the Smart heuristic silently demotes a
/// reference file to a dupe when a non-reference sibling scores
/// higher (deeper path, newer mtime, etc.), producing the
/// "A as keeper, R as dupe" inversion Mick observed.
fn order_keeper_first(
    files: Vec<PathBuf>,
    strategy: crate::cli::KeepStrategy,
    reference_set: &hashbrown::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    if files.len() < 2 {
        return files;
    }
    use crate::cli::KeepStrategy::*;
    // Reference-priority short-circuit — applies regardless of
    // strategy (including First/Interactive, so a GUI default with no
    // explicit Smart still honours the star marker).
    if !reference_set.is_empty() {
        if let Some(i) = files.iter().position(|p| reference_belongs(p, reference_set)) {
            if i == 0 {
                return files;
            }
            let mut reordered = files;
            reordered.swap(0, i);
            return reordered;
        }
    }
    // `First` is a no-op — the engine's natural order already wins.
    if matches!(strategy, First | Interactive) {
        return files;
    }
    let mtimes: Vec<Option<std::time::SystemTime>> =
        files.iter().map(|p| crate::keep::file_mtime(p)).collect();
    let keeper_idx = match strategy {
        Smart | InReference => crate::keep::pick_keeper(&files, &mtimes),
        Oldest => mtimes
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.map(|m| (i, m)))
            .min_by_key(|(_, m)| *m)
            .map(|(i, _)| i)
            .unwrap_or(0),
        Newest => mtimes
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.map(|m| (i, m)))
            .max_by_key(|(_, m)| *m)
            .map(|(i, _)| i)
            .unwrap_or(0),
        ShortestPath => files
            .iter()
            .enumerate()
            .min_by_key(|(_, p)| p.as_os_str().len())
            .map(|(i, _)| i)
            .unwrap_or(0),
        LongestPath => files
            .iter()
            .enumerate()
            .max_by_key(|(_, p)| p.as_os_str().len())
            .map(|(i, _)| i)
            .unwrap_or(0),
        First | Interactive => 0, // already handled above
    };
    if keeper_idx == 0 {
        return files;
    }
    let mut reordered = files;
    reordered.swap(0, keeper_idx);
    reordered
}

pub fn volume_guid_for(path: &std::path::Path) -> Option<String> {
    #[cfg(windows)]
    {
        crate::winapi_wrappers::volume_for_path(path).ok()
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        None
    }
}

/// Best-effort detection of whether a path's underlying device has a
/// seek penalty (HDD). Windows: IOCTL_STORAGE_QUERY_PROPERTY via
/// winapi_wrappers. macOS (#158): parse `diskutil info <path>` for
/// "Solid State: Yes/No"; falls back to false (SSD assumed) since
/// every modern Mac ships with flash storage. Other Unix: defaults to
/// "HDD" so the scope renders in the conservative pattern.
fn detect_seek_penalty(path: &std::path::Path) -> bool {
    #[cfg(windows)]
    {
        if let Ok(vol) = crate::winapi_wrappers::volume_for_path(path) {
            if let Ok(info) = crate::winapi_wrappers::query_storage_device(&vol) {
                return info.has_seek_penalty;
            }
        }
        true
    }
    #[cfg(target_os = "macos")]
    {
        // #158 -- A-macos-ssd. Apple Silicon internal storage is
        // always flash + Intel Mac SSDs have been standard since ~2018;
        // the prior non-Windows arm returned true unconditionally
        // which mis-rendered M3 + every recent Mac as HDD. Probe
        // diskutil first (catches external rotational disks), then
        // fall back to SSD-assumed.
        if let Some(has_penalty) = macos_seek_penalty_via_diskutil(path) {
            return has_penalty;
        }
        false
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        let _ = path;
        true
    }
}

/// #158 -- A-macos-ssd. Run `diskutil info <path>` and parse the
/// "Solid State: Yes|No" line into a seek-penalty bool. Returns
/// `None` when diskutil is unavailable / errors / its output doesn't
/// include the line (network volumes, sparse images), so the caller
/// can fall back to a sane default. Output snippet we parse:
///
/// ```text
/// Device Identifier:        disk3s1s1
/// ...
/// Solid State:              Yes
/// ...
/// ```
///
/// Lifted out for unit testing without invoking the subprocess --
/// `parse_diskutil_solid_state` takes the captured stdout directly.
#[cfg(target_os = "macos")]
fn macos_seek_penalty_via_diskutil(path: &std::path::Path) -> Option<bool> {
    let out = std::process::Command::new("/usr/sbin/diskutil")
        .args(["info", &path.to_string_lossy()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&out.stdout).ok()?;
    parse_diskutil_solid_state(text).map(|solid_state| !solid_state)
}

/// Parse `diskutil info` output: returns `Some(true)` if "Solid
/// State: Yes" appears (SSD), `Some(false)` if "Solid State: No"
/// (HDD), `None` if the line is absent (network volumes, sparse
/// images, exotic mounts).
///
/// `#[cfg(any(target_os = "macos", test))]` -- compiled on macOS for
/// production AND on every host under test so the parser contract
/// stays pinned cross-platform without dead-code warnings on Linux
/// production builds.
#[cfg(any(target_os = "macos", test))]
fn parse_diskutil_solid_state(text: &str) -> Option<bool> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Solid State:") {
            let value = rest.trim();
            // Match case-insensitively + accept the canonical Yes/No
            // diskutil emits. Anything else -> unknown.
            if value.eq_ignore_ascii_case("Yes") {
                return Some(true);
            }
            if value.eq_ignore_ascii_case("No") {
                return Some(false);
            }
            return None;
        }
    }
    None
}

/// Map a path to a stable, deterministic value in a fixed range so the
/// SSD drive scope renders the same "scattered cloud" the demo does.
/// Uses BLAKE3 truncated to 8 bytes — fast, no allocation beyond a
/// short slice, and stable across runs so the same file lands in the
/// same place on repeated scans.
/// EXPERIMENTAL GATE -- SUPERDEDUPER_THROTTLE_STATE_EMIT_DURING_SCAN
/// (Mick GO state-emit-throttling experiment via design 2026-06-05 00:00 PDT).
/// Tests Cand 1 hypothesis: per-file scan-progress event volume drives GUI
/// per-step cost (drain_events processing + downstream state mutation +
/// render churn).
///
/// When the env var is SET (any value, matches the SUPERDEDUPER_IOTHREADS_PARKED
/// + SUPERDEDUPER_SKIP_ACCESSKIT_DURING_SCAN pattern), the on_file callback's
/// existing modulus gates are MULTIPLIED 10x:
///   - Read events:           every 10/50 -> every 100/500
///   - StageTick funnel:      every 100   -> every 1000
///   - OverallProgress + ETA: every 100   -> every 1000
///
/// Diagnostic-only. Default behavior (env var unset) is UNCHANGED. Cached
/// via OnceLock so env::var_os fires ONCE per process.
fn state_emit_throttle_mult() -> u64 {
    use std::sync::OnceLock;
    static FLAG: OnceLock<u64> = OnceLock::new();
    *FLAG.get_or_init(|| {
        if std::env::var_os("SUPERDEDUPER_THROTTLE_STATE_EMIT_DURING_SCAN").is_some() {
            10
        } else {
            1
        }
    })
}

fn hash_path_to_lcn(path: &std::path::Path) -> u64 {
    let bytes = path.as_os_str().to_string_lossy();
    let h = blake3::hash(bytes.as_bytes());
    let arr = h.as_bytes();
    // Treat the first 8 bytes as a u64; modulo into a "synthetic
    // address space" of 4 TB so the trace looks like a real SSD's.
    let v = u64::from_le_bytes([
        arr[0], arr[1], arr[2], arr[3], arr[4], arr[5], arr[6], arr[7],
    ]);
    v % (4 * 1024 * 1024 * 1024 * 1024u64) // 4 TiB-ish
}

fn emit_paused(tx: &crate::gui::perf_channel::PerfTx) {
    let _ = tx.send(EngineEvent::ScanPaused {
        at: Instant::now(),
        checkpoint_id: "ad-hoc".into(),
    });
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Warn,
        message: "Scan paused/cancelled by user.".into(),
    });
}

/// Reorder a group's files so reference paths come first (becoming
/// the keepers in `groups_table::show`). Returns the new ordering.
/// If the group contains ONLY reference files, returns an empty Vec
/// so the caller can drop it (nothing to dedupe).
fn filter_reference_only(
    mut files: Vec<PathBuf>,
    reference_set: &hashbrown::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    files.sort_by_key(|p| {
        !reference_belongs(p, reference_set) // false (reference) sorts before true (non)
    });
    let any_non_reference = files.iter().any(|p| !reference_belongs(p, reference_set));
    if !any_non_reference {
        return Vec::new();
    }
    files
}

fn reference_belongs(path: &std::path::Path, reference_set: &hashbrown::HashSet<PathBuf>) -> bool {
    // KEEPER-PRESERVATION FIX (Mick 2026-05-31 v0.3.5 4-folder run): the
    // walker (src/inventory/walk.rs `to_verbatim`) converts root paths to
    // their Windows verbatim form (`\\?\E:\DROPBOX\...`) before walking, and
    // emitted file paths INTENTIONALLY keep that prefix. `reference_set`
    // however is built from raw user-input root paths (no verbatim prefix).
    // A naked `path.starts_with(r)` byte-compare then fails because
    // `\\?\E:\DROPBOX\Dropbox\...` does not start with `E:\DROPBOX`, so the
    // reference-priority short-circuit in `order_keeper_first` silently
    // falls through to the Smart heuristic — which then picks a non-
    // reference file as keeper when its path scores higher.
    //
    // PERF (#191 overnight push, 2026-05-31): `reference_set` entries are
    // now pre-normalized at scan-start (see L180) so this hot-path only
    // normalizes the candidate path. Earlier symmetric `strip_verbatim_prefix(r)`
    // call per iteration was burning CPU at 30fps × N groups × M files;
    // pre-normalization moves it to once-per-scan-start.
    let path_normal = strip_verbatim_prefix(path);
    reference_set
        .iter()
        .any(|r_normalized| path_normal.starts_with(r_normalized))
}

/// Strip the Windows verbatim path prefix `\\?\` if present. No-op on
/// every other input shape (Unix paths, drive-relative, UNC `\\server\…`).
/// Returns a `&Path` borrowed from the input so callers don't allocate
/// when the prefix is absent.
fn strip_verbatim_prefix(p: &std::path::Path) -> &std::path::Path {
    // Match by the raw byte form (OsStr) — `Path::starts_with` is
    // component-based and would treat `\\?\C:` as one component, which is
    // fine but slightly less direct than a literal prefix strip.
    let bytes = p.as_os_str();
    // OsStr doesn't expose a portable strip_prefix; route through Path
    // component matching which IS portable. The `\\?\` prefix shows up as
    // a single Prefix(VerbatimDisk) / Prefix(Verbatim) / Prefix(VerbatimUNC)
    // component on Windows; Path::components iterates past it transparently
    // BUT changes how starts_with matches against a non-verbatim reference.
    //
    // The simplest cross-platform correct strip: check if the string
    // representation starts with `\\?\` and slice. We have to go through
    // a Cow-like lossy round-trip because OsStr isn't slicable directly;
    // use as_encoded_bytes (stable since 1.74) for a zero-alloc slice.
    let raw = bytes.as_encoded_bytes();
    const VERBATIM: &[u8] = br"\\?\";
    if raw.starts_with(VERBATIM) {
        // SAFETY: the verbatim prefix is ASCII (4 bytes); slicing past it
        // remains valid UTF-8/WTF-8 by construction (the encoding is
        // backwards-compatible at ASCII boundaries). `from_encoded_bytes_unchecked`
        // requires the slice be a valid OsStr encoding -- ASCII-prefix
        // slicing preserves that for both Unix and Windows native encodings.
        let stripped = unsafe { std::ffi::OsStr::from_encoded_bytes_unchecked(&raw[VERBATIM.len()..]) };
        std::path::Path::new(stripped)
    } else {
        p
    }
}

/// Pick chunk sizes so we get *both* enough chunks (for cross-chunk
/// updates) and reasonably small chunks (for cancellation
/// responsiveness). Target ≥ `min_chunks` chunks where possible, but
/// never put more than `max_chunk_size` groups in a single chunk.
/// A-perf-chunks-h_new ship default: chunk_groups max_chunk_size.
/// Default 500 per sdd-testwin sweep matrix knee-point (2026-06-06
/// 01:05 PDT): chunked-par-iter overhead at ~258 ms per chunk emit
/// dominates 217s engine-in-GUI slowdown; cs=500 lands at 78.64s on
/// Mick-corpus C:\sdd-tests (vs cs=50 278.21s = -72% wall), cs=1000
/// shows slight regression (84.62s) so 500 is the right knee.
/// SUPERDEDUPER_CHUNK_SIZE env-var override stays so future sweeps
/// can re-characterize after the v0.3.40+ per-chunk-emit fix lands.
/// Cached via OnceLock; env::var fires once per process. Min 1.
fn chunk_size_max() -> usize {
    use std::sync::OnceLock;
    static CHUNK_SIZE: OnceLock<usize> = OnceLock::new();
    *CHUNK_SIZE.get_or_init(|| {
        std::env::var("SUPERDEDUPER_CHUNK_SIZE")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .map(|n| n.max(1))
            .unwrap_or(500)
    })
}

fn chunk_groups(
    laid: Vec<pipeline::layout::LaidOutGroup>,
    min_chunks: usize,
    max_chunk_size: usize,
) -> Vec<Vec<pipeline::layout::LaidOutGroup>> {
    if laid.is_empty() {
        return Vec::new();
    }
    let raw = (laid.len() / min_chunks.max(1)).max(1);
    let chunk_size = raw.min(max_chunk_size.max(1));
    let mut chunks = Vec::with_capacity(laid.len() / chunk_size + 1);
    let mut current: Vec<pipeline::layout::LaidOutGroup> = Vec::with_capacity(chunk_size);
    for g in laid {
        if current.len() >= chunk_size {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(g);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// User-facing path display that strips the Windows verbatim-path
/// prefix (`\\?\`). The engine uses verbatim paths internally to
/// bypass MAX_PATH; surfacing them to the user as `\\?\C:\foo\bar`
/// is jarring. Keeps UNC shares (`\\?\UNC\server\share`) intact —
/// only the local-drive verbatim form is normalized.
fn display_path(p: &std::path::Path) -> String {
    crate::path_display::for_user_display(p)
}


fn build_config(roots: &[RootEntry], settings: &ScanSettings) -> crate::Result<ScanConfig> {
    let include = if settings.include_glob.is_empty() {
        None
    } else {
        let mut b = GlobSetBuilder::new();
        b.add(
            Glob::new(&settings.include_glob).map_err(|e| crate::Error::BadGlob {
                pattern: settings.include_glob.clone(),
                source: e,
            })?,
        );
        Some(b.build().map_err(|e| crate::Error::BadGlob {
            pattern: settings.include_glob.clone(),
            source: e,
        })?)
    };
    let exclude = if settings.exclude_glob.is_empty() {
        None
    } else {
        let mut b = GlobSetBuilder::new();
        b.add(
            Glob::new(&settings.exclude_glob).map_err(|e| crate::Error::BadGlob {
                pattern: settings.exclude_glob.clone(),
                source: e,
            })?,
        );
        Some(b.build().map_err(|e| crate::Error::BadGlob {
            pattern: settings.exclude_glob.clone(),
            source: e,
        })?)
    };

    Ok(ScanConfig {
        roots: roots.iter().map(|r| r.path.clone()).collect(),
        reference_roots: roots
            .iter()
            .filter(|r| r.is_reference)
            .map(|r| r.path.clone())
            .collect(),
        min_size: settings.min_size_bytes,
        max_size: settings.max_size_bytes,
        // GUI scans use the engine default Tier 1 read size; the
        // experimental --tier1-bytes flag is CLI-only for now.
        tier1_bytes: crate::pipeline::hash::TIER1_BYTES,
        include,
        exclude,
        format: OutputFormat::Json,
        use_cache: settings.use_cache,
        use_format_aware: settings.use_format_aware,
        threads: settings.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        }),
        io_threads: {
            // 2026-06-02 P1 fix (design URGENT 22:05Z, Mick C:\sdd-tests
            // 60x GUI-vs-CLI gap): explicit setting wins; otherwise
            // delegate to crate::config::default_io_threads which runs
            // the v0.3.31 startup probe + (α) per-disk-class fallback.
            //
            // Pre-fix the GUI hardcoded cpu × 3 here (= 96 threads on a
            // 9950X3D2) which bypassed the workload-aware default
            // entirely. On a real NVMe with 312K-file corpus that
            // produced 60x slower wall-time than the CLI (CLI's
            // ScanConfig::from_args hits the probe; GUI didn't).
            //
            // 2026-06-02 v0.3.34 diagnostic (design 23:38Z): v0.3.33
            // ship reduced the gap from 5min to 3min, not all the way
            // to CLI's ~5s. Emit a tracing::info! that pins which path
            // ran -- user override vs default-via-probe vs default-via-
            // (α)-fallback. Mick can read this from the engine log to
            // confirm the new code path is engaging.
            let cpu = settings.threads.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            });
            let (chosen, source) = match settings.io_threads {
                Some(n) => (n, "user-explicit"),
                None => {
                    let first_root = roots.first().map(|r| &r.path);
                    let n = crate::config::default_io_threads(cpu, first_root);
                    (n, "default-via-default_io_threads")
                }
            };
            // 2026-06-02: GUI binary does NOT install a tracing
            // subscriber (per src/bin/superdeduper_gui.rs); tracing::info!
            // would be a no-op + never reach Mick's engine log file. Use
            // the superdeduper-log macro instead -- writes to both stderr
            // AND the persistent engine log at
            // <data_dir>/log/superdeduper.<unix>.<pid>.log per
            // [[feedback_persist_engine_log]] (Mick directive 2026-05-29).
            crate::log_info!(
                "GUI scan: io-threads selected io_threads={} source={} cpu_threads={}",
                chosen,
                source,
                cpu,
            );
            chosen
        },
        output: None,
        follow_links: settings.follow_links,
        allow_system_paths: settings.allow_system_paths,
        // GUI never sets force_mft; it's an A/B knob exposed via
        // the CLI for the v0.3.14 inventory matrix only.
        force_mft: false,
        parallel_roots: false,
        // GUI settings don't surface the placeholder-policy knob yet;
        // tier guard defaults to conservative (refuse cloud recalls).
        // Phase 7 GUI counter exposes the bucket; a future iteration
        // can add the toggle if user feedback shows it's wanted.
        allow_recall_on_read: false,
        // 2026-06-02 R2 engine-ask: cold-enforced is a CLI-only
        // measurement flag; GUI scans use the OS page cache as normal.
        cold_enforced: false,
        hash_algo: settings.hash_algo,
        // #81 — Compile the user's ExclusionConfig (master toggle +
        // active preset packs + custom rules) into the runtime
        // ExclusionPolicy that the walker consults per file. Falls
        // back to disabled() on compile-failure so a malformed user
        // glob doesn't sink the entire scan — the matcher error
        // surfaces in the log instead. (New installs ship with
        // safe-defaults ON via ExclusionConfig::default().)
        exclusion_policy: match crate::exclusions::ExclusionPolicy::compile(
            &settings.exclusion_config,
            &crate::exclusions::presets::BuiltinPresets,
        ) {
            Ok(policy) => policy,
            Err(e) => {
                eprintln!(
                    "exclusions: compile failed ({e}); scanning without exclusions for this run. \
                     Fix the offending pattern in Settings → Exclusions."
                );
                crate::exclusions::ExclusionPolicy::disabled()
            }
        },
        exclusion_counters: crate::exclusions::ExclusionCounters::new(),
    })
}

/// Compress a runtime inventory into the lightweight `SavedFileEntry`
/// form the checkpoint persists. One allocation per file; the on-disk
/// size for 50 000 files is ~5 MiB of JSON, well within "save in a
/// few hundred ms" territory.
fn saved_files_from_runtime(
    files: &[crate::inventory::FileEntry],
) -> Vec<crate::gui::checkpoint::SavedFileEntry> {
    files
        .iter()
        .map(|f| crate::gui::checkpoint::SavedFileEntry {
            path: f.path.clone(),
            size: f.size,
            mtime: f.mtime,
            file_ref: f.file_ref,
            parent_ref: f.parent_ref,
            usn: f.usn,
            attributes: f.attributes,
            volume_guid: f.volume_guid.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // ============================================================
    // display_path — verbatim-prefix stripping for Log tab paths.
    // ============================================================

    #[test]
    fn display_path_strips_verbatim_drive_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Windows\System32\foo.dll")),
            r"C:\Windows\System32\foo.dll"
        );
    }

    #[test]
    fn display_path_rewrites_verbatim_unc() {
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\fs\share\thing")),
            r"\\fs\share\thing"
        );
    }

    #[test]
    fn display_path_passes_through_normal_paths() {
        assert_eq!(
            display_path(Path::new(r"C:\Users\Mick\file")),
            r"C:\Users\Mick\file"
        );
        assert_eq!(
            display_path(Path::new("/home/neomatrix/file")),
            "/home/neomatrix/file"
        );
    }

    // ============================================================
    // A-ref-keeper — reference-drive invariant in order_keeper_first.
    // Regression tests for the "A as keeper, R as dupe" inversion: a
    // file under a reference root must land at files[0] regardless of
    // any Smart heuristic that would otherwise prefer a sibling.
    // ============================================================

    use crate::cli::KeepStrategy;
    use std::path::PathBuf;

    fn ref_set(roots: &[&str]) -> hashbrown::HashSet<PathBuf> {
        // Mirror production construction (live.rs:180) which pre-normalizes
        // via strip_verbatim_prefix so reference_belongs's hot path only
        // strips the candidate side. Production code is the truth; tests
        // should construct the set the same way.
        roots
            .iter()
            .map(|s| strip_verbatim_prefix(std::path::Path::new(s)).to_path_buf())
            .collect()
    }

    // ============================================================
    // #158 -- A-macos-ssd. Tests for the diskutil-output parser.
    // Cross-platform unit tests (no diskutil subprocess) so the
    // parser contract is pinned on every host even though the
    // production code path only runs on macOS.
    // ============================================================

    #[test]
    fn parse_diskutil_solid_state_yes_means_ssd() {
        let sample = "\
Device Identifier:        disk3s1s1
Device Node:              /dev/disk3s1s1
Whole:                    No
Solid State:              Yes
SMART Status:             Verified
";
        // SSD -> Solid State: Yes -> Some(true).
        assert_eq!(super::parse_diskutil_solid_state(sample), Some(true));
    }

    #[test]
    fn parse_diskutil_solid_state_no_means_hdd() {
        let sample = "Solid State:              No\n";
        assert_eq!(super::parse_diskutil_solid_state(sample), Some(false));
    }

    #[test]
    fn parse_diskutil_solid_state_absent_returns_none() {
        // Network volume / sparse image output -- no "Solid State"
        // line at all. Caller falls back to its platform default
        // (SSD-assumed on macOS).
        let sample = "\
Device Identifier:        smb://server/share
File System Personality:  smbfs
";
        assert_eq!(super::parse_diskutil_solid_state(sample), None);
    }

    #[test]
    fn parse_diskutil_solid_state_unknown_value_returns_none() {
        // Defensive: diskutil could in principle emit "Unknown" or
        // some new vocabulary in a future macOS release. Don't guess.
        let sample = "Solid State:              Unknown\n";
        assert_eq!(super::parse_diskutil_solid_state(sample), None);
    }

    #[test]
    fn parse_diskutil_solid_state_is_case_insensitive() {
        let yes_lower = "Solid State: yes\n";
        let no_upper = "Solid State: NO\n";
        assert_eq!(super::parse_diskutil_solid_state(yes_lower), Some(true));
        assert_eq!(super::parse_diskutil_solid_state(no_upper), Some(false));
    }

    #[test]
    fn parse_diskutil_solid_state_tolerates_indented_lines() {
        // Some diskutil output variants indent the value lines.
        let sample = "    Solid State:              Yes\n";
        assert_eq!(super::parse_diskutil_solid_state(sample), Some(true));
    }

    #[test]
    fn order_keeper_first_promotes_reference_over_smart_heuristic() {
        // R lives at a shallow path; A lives at a deeper, "more
        // organised" path that the Smart heuristic would otherwise
        // prefer (depth bonus). Without the reference-priority
        // short-circuit, A wins and R gets demoted — the exact
        // inversion Mick reported.
        let r_file = PathBuf::from("/mnt/R/file.bin");
        let a_file = PathBuf::from("/mnt/A/sub/sub/sub/file.bin");
        let files = vec![a_file.clone(), r_file.clone()];
        let refs = ref_set(&["/mnt/R"]);
        let ordered = order_keeper_first(files, KeepStrategy::Smart, &refs);
        assert_eq!(
            ordered[0], r_file,
            "reference-root file must land at index 0 regardless of heuristic"
        );
    }

    #[test]
    fn order_keeper_first_is_noop_when_reference_already_first() {
        // filter_reference_only puts R at index 0 first; the heuristic
        // must NOT swap R out for a higher-scoring sibling.
        let r_file = PathBuf::from("/mnt/R/file.bin");
        let a_file = PathBuf::from("/mnt/A/sub/sub/sub/file.bin");
        let files = vec![r_file.clone(), a_file.clone()];
        let refs = ref_set(&["/mnt/R"]);
        let ordered = order_keeper_first(files, KeepStrategy::Smart, &refs);
        assert_eq!(ordered[0], r_file);
        assert_eq!(ordered[1], a_file);
    }

    #[test]
    fn order_keeper_first_no_reference_falls_through_to_strategy() {
        // Empty reference set → existing Smart behaviour preserved.
        let recycle_path = PathBuf::from("/mnt/A/$Recycle.Bin/foo.txt");
        let canonical = PathBuf::from("/mnt/A/Users/me/Documents/Projects/foo.txt");
        let files = vec![recycle_path.clone(), canonical.clone()];
        let refs = ref_set(&[]);
        let ordered = order_keeper_first(files, KeepStrategy::Smart, &refs);
        assert_eq!(
            ordered[0], canonical,
            "with no reference set, Smart should still beat the recycle-bin path"
        );
    }

    #[test]
    fn order_keeper_first_reference_priority_honoured_by_first_strategy() {
        // Even KeepStrategy::First — which is normally a no-op — must
        // promote a reference file. Otherwise a GUI default with no
        // explicit Smart silently violates the star invariant.
        let r_file = PathBuf::from("/mnt/R/file.bin");
        let a_file = PathBuf::from("/mnt/A/file.bin");
        let files = vec![a_file.clone(), r_file.clone()];
        let refs = ref_set(&["/mnt/R"]);
        let ordered = order_keeper_first(files, KeepStrategy::First, &refs);
        assert_eq!(ordered[0], r_file);
    }

    #[test]
    fn order_keeper_first_picks_first_reference_when_multiple_present() {
        // Two reference roots both contain a member — the first one
        // encountered in `files` wins, matching CLI parity
        // (dedupe::pick_keeper returns the first index whose
        // canonical_key is in the reference set).
        let r1_file = PathBuf::from("/mnt/R1/file.bin");
        let r2_file = PathBuf::from("/mnt/R2/file.bin");
        let a_file = PathBuf::from("/mnt/A/file.bin");
        // R2 appears first in `files` order — R2 wins.
        let files = vec![a_file.clone(), r2_file.clone(), r1_file.clone()];
        let refs = ref_set(&["/mnt/R1", "/mnt/R2"]);
        let ordered = order_keeper_first(files, KeepStrategy::Smart, &refs);
        assert_eq!(ordered[0], r2_file);
    }

    /// REGRESSION GUARD for Mick's 2026-05-31 v0.3.5 4-folder bug: when
    /// the walker emits file paths with the Windows `\\?\` verbatim
    /// prefix (per src/inventory/walk.rs `to_verbatim`) but the
    /// reference set is built from raw user-input root paths (no
    /// verbatim prefix), a naked `starts_with` byte-compare fails — so
    /// the reference-priority short-circuit silently falls through and
    /// the Smart heuristic picks a non-reference keeper. The fix
    /// (strip_verbatim_prefix on both sides of the compare) is what
    /// this test pins down.
    ///
    /// Windows-only because the verbatim prefix is a Windows path
    /// concept and `Path::starts_with` component semantics differ on
    /// Linux (treats backslash as a regular character, not a
    /// separator) — running this test against Linux's Path parser
    /// would test a different code path than what fires in prod.
    #[test]
    #[cfg(target_os = "windows")]
    fn order_keeper_first_promotes_verbatim_prefixed_reference_files() {
        // Mick's bug shape: 3 files with identical digest, 2 are under
        // a reference root, walker has slapped `\\?\` on every emitted
        // path. The reference set still carries the un-prefixed root.
        let a_file = PathBuf::from(r"\\?\E:\SEAGATE-2TB\DropBox\Dropbox\file.mp4");
        let b_file = PathBuf::from(r"\\?\E:\DROPBOX\Dropbox\file.mp4");
        let c_file = PathBuf::from(r"\\?\E:\DROPBOX\Dropbox\backups\file.mp4");
        let files = vec![a_file.clone(), b_file.clone(), c_file.clone()];
        let refs = ref_set(&[r"E:\DROPBOX"]);

        let ordered = order_keeper_first(files, KeepStrategy::Smart, &refs);

        // Reference-rooted file wins regardless of Smart's path-depth
        // heuristic preference for the SEAGATE path. Either B or C is
        // acceptable per the tightened invariant ('keeper from the
        // reference partition; non-reference files MUST be dupes').
        assert!(
            ordered[0] == b_file || ordered[0] == c_file,
            "keeper must come from the reference partition (E:\\DROPBOX); got {:?}",
            ordered[0]
        );
        // Belt-and-suspenders: the SEAGATE non-reference file MUST NOT
        // be the keeper.
        assert_ne!(
            ordered[0], a_file,
            "non-reference SEAGATE file landed as keeper — invariant violation",
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn reference_belongs_matches_across_verbatim_asymmetry() {
        // Direct unit on the helper that did the wrong thing in v0.3.5.
        // All four combinations of verbatim-prefix presence/absence on
        // path vs reference must match (assuming the underlying paths
        // refer to the same root). The walker's emission shape is the
        // first two rows; the user-input root shape is the last two.
        let refs_plain = ref_set(&[r"E:\DROPBOX"]);
        let refs_verbatim = ref_set(&[r"\\?\E:\DROPBOX"]);

        let plain_file = PathBuf::from(r"E:\DROPBOX\sub\file.bin");
        let verbatim_file = PathBuf::from(r"\\?\E:\DROPBOX\sub\file.bin");

        assert!(reference_belongs(&plain_file, &refs_plain), "plain+plain");
        assert!(reference_belongs(&verbatim_file, &refs_plain), "verbatim+plain (the bug shape)");
        assert!(reference_belongs(&plain_file, &refs_verbatim), "plain+verbatim");
        assert!(reference_belongs(&verbatim_file, &refs_verbatim), "verbatim+verbatim");

        // Non-reference paths still NOT under any reference root.
        let unrelated = PathBuf::from(r"\\?\E:\SEAGATE-2TB\Dropbox\file.bin");
        assert!(!reference_belongs(&unrelated, &refs_plain));
        assert!(!reference_belongs(&unrelated, &refs_verbatim));
    }

    #[test]
    fn order_keeper_first_reference_beats_newest_mtime_strategy_too() {
        // Newest-strategy + reference set: reference still wins even
        // if a non-reference sibling has a much newer mtime. Touches
        // the filesystem because Newest reads mtime; build the
        // physical files inline.
        let dir = std::env::temp_dir().join(format!(
            "sdd-order-keeper-newest-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let r_root = dir.join("R");
        let a_root = dir.join("A");
        std::fs::create_dir_all(&r_root).unwrap();
        std::fs::create_dir_all(&a_root).unwrap();
        let r_file = r_root.join("file.bin");
        let a_file = a_root.join("file.bin");
        std::fs::write(&r_file, b"x").unwrap();
        // Sleep so A's mtime is strictly later than R's.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&a_file, b"x").unwrap();
        let files = vec![a_file.clone(), r_file.clone()];
        let refs: hashbrown::HashSet<PathBuf> =
            std::iter::once(r_root.clone()).collect();
        let ordered = order_keeper_first(files, KeepStrategy::Newest, &refs);
        assert_eq!(
            ordered[0], r_file,
            "reference must beat Newest even when A is strictly newer"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
