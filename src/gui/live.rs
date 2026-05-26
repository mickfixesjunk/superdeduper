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
use std::time::Instant;

use crossbeam_channel::Sender;
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
pub fn spawn(tx: Sender<EngineEvent>, roots: Vec<PathBuf>) -> thread::JoinHandle<()> {
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
        5,
        crate::cli::ImageHashAlgoArg::default(),
        5.0,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_with_settings(
    tx: Sender<EngineEvent>,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
    defender_rtp_pre: Option<bool>,
    scan_mode: crate::cli::ScanMode,
    image_similarity_threshold: u32,
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
    tx: Sender<EngineEvent>,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
    _defender_rtp_pre: Option<bool>,
    scan_mode: crate::cli::ScanMode,
    image_similarity_threshold: u32,
    image_hash_algorithm: crate::cli::ImageHashAlgoArg,
    audio_similarity_threshold: f64,
) -> crate::Result<()> {
    let _scan_started_at = Instant::now();
    // Wall-clock start, separate from the Instant above (Instant is
    // monotonic + opaque; we need a UNIX timestamp the scan_history
    // persistence layer can sort + display). One reading, threaded
    // through to the ScanFinished hook below.
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
                "roots={} min_size={} format_aware={} use_cache={} paranoid={} threads={:?} \
                 hash_algo={} hash_impl={hash_impl}",
                roots.len(),
                settings.min_size_bytes,
                settings.use_format_aware,
                settings.use_cache,
                settings.paranoid,
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
    let reference_set: hashbrown::HashSet<PathBuf> = roots
        .iter()
        .filter(|r| r.is_reference)
        .map(|r| r.path.clone())
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
                        // may not be valid (e.g., min_size/paranoid
                        // changed → different group composition).
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
    let mut restored_dup_paths: hashbrown::HashSet<std::path::PathBuf> =
        hashbrown::HashSet::new();
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
    #[cfg(feature = "telemetry")]
    let easter_egg_hits: Vec<String> = {
        use crate::leaderboard::install;
        use crate::leaderboard::predicates::{evaluate_all, PredicateContext};
        let all_paths: Vec<&std::path::Path> = files.iter().map(|e| e.path.as_path()).collect();
        // FILETIME (100ns ticks since 1601-01-01) → Unix seconds.
        // Inverse of inventory::walk::filetime_ticks. `mtime == 0`
        // is the walker's "unknown" sentinel; surface as None so
        // mtime-dependent predicates short-circuit cleanly per-file.
        const UNIX_EPOCH_AS_FILETIME: i64 = 116_444_736_000_000_000;
        let mtimes_unix_secs: Vec<Option<i64>> = files
            .iter()
            .map(|e| {
                if e.mtime == 0 {
                    None
                } else {
                    Some((e.mtime - UNIX_EPOCH_AS_FILETIME) / 10_000_000)
                }
            })
            .collect();
        // Counters: best-effort load. If install.json is missing
        // or corrupt the counter-driven predicates (picky-eater /
        // verify-veteran) silently return None — they just won't
        // grant yet.
        let install_state = install::load().ok().flatten();
        let install_counters = install_state.as_ref().map(|s| &s.counters);
        let pred_ctx = PredicateContext {
            all_paths: &all_paths,
            mtimes_unix_secs: Some(&mtimes_unix_secs),
            install_counters,
            perceptual_mode_active: false,
        };
        evaluate_all(&pred_ctx)
    };

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

    // Smaller chunks → more frequent updates between chunks. We also
    // wire a per-file progress callback into the hasher so the UI
    // animates *within* a chunk, not just between them.
    let chunks = chunk_groups(laid, 32, 50);
    let total_chunks = chunks.len();
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
    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::Hashing,
        done: restored_skipped,
        total: total_to_hash.saturating_add(restored_skipped),
        eta_secs: None,
    });
    // #99 PR10 — frame-zero bar emit. Lets a paste-back of the
    // log capture the bar position BEFORE chunk 1 runs, so the
    // jump-vs-climb of the fast-forward is unambiguous.
    {
        let initial_adjusted_total = total_to_hash.saturating_add(restored_skipped);
        let initial_bar_pct = if initial_adjusted_total > 0 {
            (restored_skipped as f64 / initial_adjusted_total as f64) * 100.0
        } else {
            0.0
        };
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "Stage 4 bar frame-zero: bar {initial_bar_pct:.2}% ({restored_skipped}/{initial_adjusted_total} files) · total_to_hash={total_to_hash}, restored_dup_skip={restored_skipped}, total_chunks={total_chunks}"
            ),
        });
    }

    let mut total_bytes_read: u64 = 0;
    let mut total_dups: u64 = 0;
    let mut reclaimable: u64 = 0;
    let mut reclaimable_inode: u64 = 0;
    // #49 — per-SimilarityKind group counts; incremented every time
    // we bump `total_dups`. Persisted in the scan-history record at
    // ScanFinished so the History tab can show "32 perceptual + 30
    // byte-identical" rather than "62 groups total."
    let mut groups_by_similarity_kind: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut largest_group_bytes: u64 = 0;
    // G1.x esoteric metric: largest dup-group (by member count)
    // whose content is empty (size == 0). Used by backend to grant
    // "zero-byte hoarder". Updated on each group emission below.
    let mut zero_byte_group_max: u64 = 0;
    // G1.x esoteric metric: highest hardlink count observed in the
    // scan. Derived from `link_equivalent` groups — every path in
    // such a group is a confirmed alias of one inode, so the
    // group's member count is a tight lower bound on that inode's
    // `nlink`. Honest under-report: singletons + partial-hardlink
    // groups don't contribute (we don't have per-file nlink at
    // this layer; walker-side nlink capture is a future follow-up
    // that would let us cover those too).
    let mut max_hardlink_count_in_scan: u64 = 0;
    // G1.x esoteric metric: count of basenames that resolved to
    // ≥2 distinct content hashes across the scan ("name-twins").
    // Builds basename → {content_hash} as groups stream in; the
    // final count of entries with set size ≥ 2 is the metric.
    // Only sees basenames that appear in ≥1 dup group (singletons
    // bypass hashing entirely + we don't have hashes for them), so
    // the worst missed case is "same name in one dup group + one
    // singleton of different size" — fine for an esoteric metric.
    let mut basename_to_hashes: std::collections::HashMap<
        String,
        std::collections::HashSet<String>,
    > = std::collections::HashMap::new();
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

    for (i, chunk) in chunks.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            if let Some(p) = &checkpoint_path {
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
            let lcn_bytes = if progress_drive_is_hdd {
                total_bytes
            } else {
                hash_path_to_lcn(path)
            };

            let read_modulus = if progress_drive_is_hdd { 50 } else { 10 };
            if n.is_multiple_of(read_modulus) {
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
            if n_tier.is_multiple_of(100) {
                let stage = match tier {
                    0 => Stage::Tier0Format,
                    1 => Stage::Tier1Head,
                    2 => Stage::Tier2HeadMidTail,
                    _ => Stage::Tier3Full,
                };
                let _ = progress_tx.try_send(EngineEvent::StageTick {
                    stage,
                    delta: 100,
                    total: n_tier,
                });
            }

            // Headline OverallProgress + ETA: only Tier 1 advances.
            if counts_for_progress && n.is_multiple_of(100) {
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
                let adjusted_done = n.saturating_add(progress_restored_skipped);
                let adjusted_total = total_to_hash_inner
                    .saturating_add(progress_restored_skipped);
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

        let (dups, counters) = match pipeline::hash::run_cancellable(
            chunk,
            &cfg,
            cache.clone(),
            on_file,
            Arc::clone(&cancel),
        ) {
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
                if let Some(p) = &checkpoint_path {
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
                emit_paused(&tx);
                return Ok(());
            }
            Err(e) => return Err(e),
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

        for g in dups {
            if already_reported.contains(&g.content_hash) {
                continue; // carried over from a prior checkpoint
            }
            let visible_files = filter_reference_only(g.files, &reference_set);
            if visible_files.len() < 2 {
                continue;
            }
            confirmed += 1;
            let savings = g
                .size
                .saturating_mul(visible_files.len().saturating_sub(1) as u64);
            reclaimable = reclaimable.saturating_add(savings);
            // Inode-aware reclaim — for hardlinked corpora
            // (C:\Windows / WinSxS dominated), the path-aware
            // count above inflates because each WinSxS alias counts
            // as a "dup" even though all aliases share an inode.
            // Inode-aware is the TRUE freeable bytes; ships in the
            // leaderboard payload + clamped against bytes_scanned to
            // satisfy the backend's result_self_consistency check.
            // Hardlink-equivalent groups have nothing to reclaim
            // (already collapsed on disk) — skip them.
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
            reclaimable_inode = reclaimable_inode.saturating_add(group_reclaim);
            // `largest_single_group_bytes` per backend's sanity
            // check is the largest group's RECLAIM, not its total
            // bytes-on-disk. Backend rule: largest <= reclaim total.
            // Using inode-aware per-group reclaim guarantees that
            // because each per-group reclaim is a summand of the
            // total (and there are no negative summands).
            if group_reclaim > largest_group_bytes {
                largest_group_bytes = group_reclaim;
            }
            if g.size == 0 {
                // g.files was moved into filter_reference_only
                // above; use visible_files which carries the same
                // (or fewer) members.
                let members = visible_files.len() as u64;
                if members > zero_byte_group_max {
                    zero_byte_group_max = members;
                }
            }
            if g.link_equivalent {
                // Every visible path in a link-equivalent group
                // refers to one inode → the member count is a
                // confirmed lower bound on that inode's nlink.
                let aliases = visible_files.len() as u64;
                if aliases > max_hardlink_count_in_scan {
                    max_hardlink_count_in_scan = aliases;
                }
            }
            // Track basenames → content-hashes for the name-twins
            // metric. A path may have a non-unicode basename — skip
            // those (rare; just narrows what counts as a collision).
            for path in visible_files.iter() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    basename_to_hashes
                        .entry(name.to_string())
                        .or_default()
                        .insert(g.content_hash.clone());
                }
            }
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
            // Reorder so the smart-heuristic keeper lands at
            // index 0. The GUI's safe-rename / recycle / hardlink
            // flows all treat `files[0]` as the canonical keeper
            // — putting the best-scored file there is how we
            // make `KeepStrategy::Smart` the GUI default without
            // each downstream action having to know about it.
            let visible_files = order_keeper_first(visible_files, settings.keep_strategy);
            let summary = DuplicateGroupSummary {
                size: g.size,
                content_hash: g.content_hash,
                files: visible_files,
                link_equivalent: g.link_equivalent,
                unique_inodes: g.unique_inodes,
                similarity_kind: crate::pipeline::SimilarityKind::ByteIdentical,
            };
            checkpoint_state.record(&summary);
            let _ = tx.send(EngineEvent::DuplicateFound(summary));
        }

        // Periodic checkpoint flush so an unexpected crash doesn't
        // lose progress beyond the last chunk. JSON encode + atomic
        // rename is cheap relative to the hashing work.
        if let Some(p) = &checkpoint_path {
            let _ = checkpoint::save(p, &checkpoint_state);
        }

        tier3_done += 1;
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
        let _ = tx.try_send(EngineEvent::Status(format!(
            "Hashing chunk {}/{} · {} duplicate group(s) so far",
            i + 1,
            total_chunks,
            total_dups
        )));
        // #100 — periodic cache-stats emit during Stage 4 so a user
        // watching a resume run can see in real-time whether the
        // cache fast-forward is happening (vs waiting for scan-
        // finish). Fires every 10 chunks; cheap (atomic loads).
        if (i + 1).is_multiple_of(10) {
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
            let n_now = files_hashed.load(Ordering::Relaxed);
            let adjusted_done_now = n_now.saturating_add(restored_skipped);
            let adjusted_total_now = total_to_hash.saturating_add(restored_skipped);
            let bar_pct = if adjusted_total_now > 0 {
                (adjusted_done_now as f64 / adjusted_total_now as f64) * 100.0
            } else {
                0.0
            };
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "cache so far: {total_cache_hits} hit(s), {total_cache_drift_misses} drift miss(es), {} fresh hash(es) — {hit_rate_so_far:.1}% hit rate (chunk {}/{}) · bar {bar_pct:.2}% ({adjusted_done_now}/{adjusted_total_now} files, restored_dup_skip={restored_skipped})",
                    total_so_far.saturating_sub(total_cache_hits),
                    i + 1,
                    total_chunks,
                ),
            });
        }
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
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "cache: {} hits, {} writes, {} fresh hashes — {:.1}% hit rate",
            total_cache_hits,
            total_cache_writes,
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
            self, ResultSummary, RunShape, SubmissionInputs, FEATURE_BIT_ALLOW_RECALL_ON_READ,
            FEATURE_BIT_ALLOW_SYSTEM_PATHS, FEATURE_BIT_CACHE, FEATURE_BIT_EXCLUDE_GLOB,
            FEATURE_BIT_FOLLOW_LINKS, FEATURE_BIT_FORMAT_AWARE, FEATURE_BIT_INCLUDE_GLOB,
            FEATURE_BIT_PARANOID, FEATURE_BIT_REFERENCE_ROOTS,
        };
        // Discard the defender post probe; current backend schema
        // doesn't carry defender state. Keep the call commented in
        // case a future schema reinstates it. Sig param is
        // `_`-prefixed because it's unused in non-telemetry builds.
        let _ = _defender_rtp_pre;
        // Wall-clock as seconds (number) per schema. Same `_`-prefix
        // reasoning as the param above.
        let wall_clock_seconds = _scan_started_at.elapsed().as_secs_f64();
        let hash_algorithm = match settings.hash_algo {
            crate::pipeline::hash::HashAlgo::Blake3 => "blake3",
            crate::pipeline::hash::HashAlgo::River5 => "river5-aes-ni",
        }
        .to_string();
        // Scope heuristic from the root paths.
        let scope = classify_scope(&roots);
        // Corpus kind heuristic: "system" if any root looks like an
        // OS-system tree (C:\Windows, /System, /usr, etc.), else
        // "user-data".
        let corpus_kind = classify_corpus_kind(&roots);
        // Features bitmap built from the resolved settings.
        let mut features_bits: u64 = 0;
        if settings.use_cache {
            features_bits |= FEATURE_BIT_CACHE;
        }
        if settings.use_format_aware {
            features_bits |= FEATURE_BIT_FORMAT_AWARE;
        }
        if settings.paranoid {
            features_bits |= FEATURE_BIT_PARANOID;
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

        let inputs = SubmissionInputs {
            client_version: env!("CARGO_PKG_VERSION").to_string(),
            run_uuid: uuid::Uuid::new_v4().to_string(),
            scan_id: Some(scan_id_for_this_run.clone()),
            hardware: hardware::detect(),
            run_shape: RunShape {
                wall_clock_seconds,
                bytes_scanned: total_bytes_read,
                files_scanned: total_files,
                hash_algorithm,
                walker_variant: "hybrid".to_string(),
                scope,
                features_used_bitmap: features_bits,
                corpus_kind,
                cache_hit_ratio,
                easter_egg_hits,
                // Computed during dup-group emission above.
                zero_byte_group_max: if zero_byte_group_max > 0 {
                    Some(zero_byte_group_max)
                } else {
                    None
                },
                // Computed from `link_equivalent` group sizes during
                // dup-group emission above. Conservative lower bound
                // — see the comment on the declaration.
                max_hardlink_count_in_scan: if max_hardlink_count_in_scan > 0 {
                    Some(max_hardlink_count_in_scan)
                } else {
                    None
                },
                // Tally basenames whose path-resolution disagreed on
                // content (≥2 distinct hashes for the same name).
                name_collision_count: {
                    let n = basename_to_hashes
                        .values()
                        .filter(|hs| hs.len() >= 2)
                        .count() as u64;
                    if n > 0 {
                        Some(n)
                    } else {
                        None
                    }
                },
                // #89 — count of distinct network-share roots in
                // scope (UNC `\\server\share`, smb://, nfs://).
                // Counted at the requested-root level so the value
                // reflects user intent, not whether files were
                // actually read. Backend uses this for the latent
                // `multi-share-maestro` grant.
                share_count_in_scope: {
                    let n = count_distinct_share_roots(&root_paths);
                    if n > 0 {
                        Some(n)
                    } else {
                        None
                    }
                },
                // #89 — kept `None` per design's catalog-semantic
                // flag (rollback from initial `Some(true)`). The
                // `safety-first` achievement (catalog:932) describes
                // "Used --dry-run 25+ times before commit" and sits
                // on the skill/curation axis — it rewards deliberate
                // dry-run *intent*, not every-submission protocol
                // shape. Setting Some(true) per scan-finish would
                // collapse the metric into "scanned 25+ times" and
                // strip the curation semantic. Reactivate the field
                // when a real dry-run UX ships: a future GUI
                // "Preview without action" toggle, or CLI
                // `superdeduper scan --dry-run`.
                dry_run: None,
                // #89 — group-reviews happen AFTER scan-finish, so
                // the count is always 0 at initial submission time.
                // Plumbed as `None` (omitted from payload) until a
                // PATCH path updates it. See submission.rs comment
                // on the field for the deferred future-work note.
                groups_reviewed_count: None,
            },
            result_summary: ResultSummary {
                duplicate_groups: total_dups,
                // Use inode-aware reclaim (collapses hardlink
                // aliases) for the leaderboard payload. Clamp to
                // bytes_scanned just in case some weird edge case
                // still produces reclaim > scanned (e.g. a file
                // counted as both alias and unique somehow); backend
                // sanity-rejects on that.
                duplicate_bytes_reclaimable: reclaimable_inode.min(total_bytes_read),
                largest_single_group_bytes: largest_group_bytes.min(total_bytes_read),
                actions_taken_summary: std::collections::BTreeMap::new(),
                placeholder_skip_count: None,
                placeholder_skip_bytes: None,
            },
        };
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

        // #41 — build the FULL submittable payload (with the real
        // install_id from the active install state) + stash it for
        // the scan_history record-write site below. Resubmit replays
        // this verbatim so the signature stays valid against the
        // install_id captured at build time. If the install state
        // can't load (unregistered, missing file), we just leave
        // the History row without a payload — Resubmit stays
        // disabled, the user has a clear "register first" surface.
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
            let t_tier4 = std::time::Instant::now();
            let tier4_groups = crate::pipeline::image_hash::tier4::find_similar_groups(
                inv,
                algo,
                image_similarity_threshold,
            );
            let n_groups = tier4_groups.len();
            for g in tier4_groups {
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
                    files: g.files,
                    link_equivalent: g.link_equivalent,
                    unique_inodes: g.unique_inodes,
                    similarity_kind: g.similarity_kind,
                };
                let _ = tx.send(EngineEvent::DuplicateFound(summary));
            }
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Info,
                message: format!(
                    "Tier-4 perceptual ({}): {n_groups} group(s) within {image_similarity_threshold} bits ({} ms)",
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
            let tier4_groups = audio_tier4::find_similar_groups(inv, audio_similarity_threshold);
            let n_groups = tier4_groups.len();
            for g in tier4_groups {
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
                    files: g.files,
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
fn order_keeper_first(files: Vec<PathBuf>, strategy: crate::cli::KeepStrategy) -> Vec<PathBuf> {
    if files.len() < 2 {
        return files;
    }
    use crate::cli::KeepStrategy::*;
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
/// winapi_wrappers; failure or non-Windows defaults to "HDD" so the
/// scope renders in the conservative, calm pattern.
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
    #[cfg(not(windows))]
    {
        let _ = path;
        true
    }
}

/// Map a path to a stable, deterministic value in a fixed range so the
/// SSD drive scope renders the same "scattered cloud" the demo does.
/// Uses BLAKE3 truncated to 8 bytes — fast, no allocation beyond a
/// short slice, and stable across runs so the same file lands in the
/// same place on repeated scans.
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

fn emit_paused(tx: &Sender<EngineEvent>) {
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
    for r in reference_set {
        if path.starts_with(r) {
            return true;
        }
    }
    false
}

/// Pick chunk sizes so we get *both* enough chunks (for cross-chunk
/// updates) and reasonably small chunks (for cancellation
/// responsiveness). Target ≥ `min_chunks` chunks where possible, but
/// never put more than `max_chunk_size` groups in a single chunk.
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

/// Map roots → `run_shape.scope` enum:
/// * single drive-root (e.g. `C:\`) → `whole-volume`
/// * single non-root path → `subdirectory`
/// * multiple paths → `selection`
#[cfg(feature = "telemetry")]
fn classify_scope(roots: &[RootEntry]) -> String {
    if roots.len() > 1 {
        return "selection".to_string();
    }
    match roots.first() {
        Some(r) if is_drive_root(&r.path) => "whole-volume".to_string(),
        Some(_) => "subdirectory".to_string(),
        None => "subdirectory".to_string(),
    }
}

/// "system" if any root path looks like an OS-system tree;
/// otherwise "user-data". Conservative heuristic — the backend just
/// uses this for category bucketing.
#[cfg(feature = "telemetry")]
fn classify_corpus_kind(roots: &[RootEntry]) -> String {
    for r in roots {
        let s = r.path.to_string_lossy().to_ascii_lowercase();
        if s.contains("\\windows\\")
            || s.ends_with("\\windows")
            || s.contains("/system/")
            || s.starts_with("/system")
            || s.contains("\\program files")
            || s.starts_with("/usr/")
            || s.starts_with("/bin/")
            || s.starts_with("/sbin/")
        {
            return "system".to_string();
        }
    }
    "user-data".to_string()
}

#[cfg(feature = "telemetry")]
fn is_drive_root(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    // Windows: "C:\", "D:\", "\\?\C:\". Unix: "/".
    s == "/"
        || (s.len() == 3 && s.chars().nth(1) == Some(':') && s.ends_with('\\'))
        || (s.len() == 7 && s.starts_with("\\\\?\\") && s.ends_with('\\'))
}

#[cfg(feature = "telemetry")]
fn is_network_share_path(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    // Windows UNC `\\server\share\...` — leading `\\` but not the
    // verbatim-device form `\\?\` or `\\.\`. Also catches the
    // verbatim-UNC variant `\\?\UNC\server\share\...`.
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'\\' && bytes[1] == b'\\' {
        let prefix3 = bytes.get(2).copied();
        if prefix3 != Some(b'?') && prefix3 != Some(b'.') {
            return true;
        }
        if s.starts_with("\\\\?\\UNC\\") {
            return true;
        }
    }
    // Cross-platform URL forms surfaced by user-typed paths.
    s.starts_with("smb://") || s.starts_with("nfs://") || s.starts_with("cifs://")
}

#[cfg(feature = "telemetry")]
fn count_distinct_share_roots(paths: &[std::path::PathBuf]) -> u64 {
    use std::collections::HashSet;
    let mut shares: HashSet<String> = HashSet::new();
    for p in paths {
        if !is_network_share_path(p) {
            continue;
        }
        let s = p.to_string_lossy();
        // For `\\server\share\rest` (or verbatim-UNC equivalent),
        // group by `\\server\share` so multiple roots into the same
        // share count once. For URL forms, group by scheme+authority.
        let key = if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
            // `server\share\rest` → `server\share`
            let two: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
            format!("unc:{}", two.join("\\"))
        } else if let Some(rest) = s.strip_prefix("\\\\") {
            let two: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
            format!("unc:{}", two.join("\\"))
        } else if let Some(rest) = s.strip_prefix("smb://") {
            let auth = rest.split('/').next().unwrap_or("");
            format!("smb:{auth}")
        } else if let Some(rest) = s.strip_prefix("nfs://") {
            let auth = rest.split('/').next().unwrap_or("");
            format!("nfs:{auth}")
        } else if let Some(rest) = s.strip_prefix("cifs://") {
            let auth = rest.split('/').next().unwrap_or("");
            format!("cifs:{auth}")
        } else {
            s.to_string()
        };
        shares.insert(key);
    }
    shares.len() as u64
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
        paranoid: settings.paranoid,
        use_cache: settings.use_cache,
        use_format_aware: settings.use_format_aware,
        threads: settings.threads.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        }),
        io_threads: {
            // Explicit setting wins; otherwise oversubscribe to
            // CPU × 3 like the CLI default.
            let cpu = settings.threads.unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            });
            settings.io_threads.unwrap_or(cpu.saturating_mul(3).max(1))
        },
        output: None,
        follow_links: settings.follow_links,
        allow_system_paths: settings.allow_system_paths,
        // GUI settings don't surface the placeholder-policy knob yet;
        // tier guard defaults to conservative (refuse cloud recalls).
        // Phase 7 GUI counter exposes the bucket; a future iteration
        // can add the toggle if user feedback shows it's wanted.
        allow_recall_on_read: false,
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
    // classify_scope / classify_corpus_kind / is_drive_root —
    // the heuristics that produce run_shape.scope + corpus_kind on
    // the leaderboard payload. Wrong outputs land in the backend
    // and bucket users incorrectly; pin the obvious cases.
    // ============================================================

    #[cfg(feature = "telemetry")]
    fn root(path: &str) -> RootEntry {
        RootEntry {
            path: std::path::PathBuf::from(path),
            is_reference: false,
        }
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn classify_scope_whole_volume_for_single_drive_root() {
        assert_eq!(classify_scope(&[root(r"C:\")]), "whole-volume");
        assert_eq!(classify_scope(&[root("/")]), "whole-volume");
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn classify_scope_subdirectory_for_single_non_root() {
        assert_eq!(classify_scope(&[root(r"C:\Users\Mick")]), "subdirectory");
        assert_eq!(
            classify_scope(&[root("/home/neomatrix/Documents")]),
            "subdirectory"
        );
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn classify_scope_selection_for_multiple_roots() {
        assert_eq!(
            classify_scope(&[root(r"C:\Users\A"), root(r"D:\Backup")]),
            "selection"
        );
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn classify_corpus_kind_system_on_windows_system_paths() {
        assert_eq!(
            classify_corpus_kind(&[root(r"C:\Windows\System32")]),
            "system"
        );
        assert_eq!(
            classify_corpus_kind(&[root(r"C:\Program Files\Foo")]),
            "system"
        );
        assert_eq!(classify_corpus_kind(&[root("/usr/local/bin")]), "system");
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn classify_corpus_kind_user_data_on_user_paths() {
        assert_eq!(classify_corpus_kind(&[root(r"C:\Users\Mick")]), "user-data");
        assert_eq!(
            classify_corpus_kind(&[root("/home/neomatrix/Photos")]),
            "user-data"
        );
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn is_drive_root_recognises_windows_and_unix_roots() {
        assert!(is_drive_root(Path::new(r"C:\")));
        assert!(is_drive_root(Path::new(r"D:\")));
        assert!(is_drive_root(Path::new("/")));
        assert!(!is_drive_root(Path::new(r"C:\Users")));
        assert!(!is_drive_root(Path::new("/home")));
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn is_network_share_path_detects_unc_and_url_forms() {
        // UNC.
        assert!(is_network_share_path(Path::new(r"\\fileserver\public")));
        assert!(is_network_share_path(Path::new(r"\\fileserver\public\sub")));
        assert!(is_network_share_path(Path::new(r"\\?\UNC\fileserver\public\sub")));
        // URL forms.
        assert!(is_network_share_path(Path::new("smb://nas.local/photos")));
        assert!(is_network_share_path(Path::new("nfs://10.0.0.5/export")));
        assert!(is_network_share_path(Path::new("cifs://host/share")));
        // Verbatim-device forms must NOT count as shares.
        assert!(!is_network_share_path(Path::new(r"\\?\C:\Users")));
        assert!(!is_network_share_path(Path::new(r"\\.\PhysicalDrive0")));
        // Plain local paths.
        assert!(!is_network_share_path(Path::new(r"C:\Users\Mick")));
        assert!(!is_network_share_path(Path::new("/home/mick")));
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn count_distinct_share_roots_dedups_by_share() {
        let paths = vec![
            std::path::PathBuf::from(r"\\fileserver\public\a"),
            std::path::PathBuf::from(r"\\fileserver\public\b"),
            std::path::PathBuf::from(r"\\fileserver\private"),
            std::path::PathBuf::from(r"\\?\UNC\fileserver\private\sub"),
            std::path::PathBuf::from("smb://nas.local/photos"),
            std::path::PathBuf::from("smb://nas.local/videos"),
            std::path::PathBuf::from(r"C:\Users\Mick"),
        ];
        // Distinct shares: \\fileserver\public, \\fileserver\private
        // (UNC + verbatim-UNC collapse), smb://nas.local (one
        // authority, two paths). Local C:\ doesn't count.
        assert_eq!(count_distinct_share_roots(&paths), 3);
    }

    #[test]
    #[cfg(feature = "telemetry")]
    fn count_distinct_share_roots_zero_when_no_shares() {
        let paths = vec![
            std::path::PathBuf::from(r"C:\Users\Mick"),
            std::path::PathBuf::from("/home/mick"),
        ];
        assert_eq!(count_distinct_share_roots(&paths), 0);
    }
}
