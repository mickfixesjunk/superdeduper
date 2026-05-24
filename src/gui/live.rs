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
    )
}

pub fn spawn_with_settings(
    tx: Sender<EngineEvent>,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
    defender_rtp_pre: Option<bool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("superdeduper-engine".into())
        .spawn(move || {
            if let Err(e) = run(tx.clone(), roots, settings, cancel, defender_rtp_pre) {
                let _ = tx.send(EngineEvent::Log {
                    level: LogLevel::Error,
                    message: format!("engine: {e}"),
                });
                let _ = tx.send(EngineEvent::Status(format!("Failed: {e}")));
            }
        })
        .expect("spawn engine thread")
}

fn run(
    tx: Sender<EngineEvent>,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
    defender_rtp_pre: Option<bool>,
) -> crate::Result<()> {
    let scan_started_at = Instant::now();
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
    let cfg = build_config(&roots, &settings)?;
    let reference_set: hashbrown::HashSet<PathBuf> = roots
        .iter()
        .filter(|r| r.is_reference)
        .map(|r| r.path.clone())
        .collect();
    let root_paths: Vec<PathBuf> = roots.iter().map(|r| r.path.clone()).collect();
    let checkpoint_path = checkpoint::default_checkpoint_path().ok();
    let mut checkpoint_state = Checkpoint::new(roots.clone(), settings.clone());
    // If a checkpoint already exists from a prior interrupted scan
    // against THESE EXACT roots and settings, fold its previous
    // duplicates in so we don't re-report them or lose them.
    let prior = checkpoint_path
        .as_ref()
        .and_then(|p| checkpoint::load(p).ok().flatten())
        .filter(|cp| cp.roots == roots && cp.settings == settings);
    // Inventory state carried over from a prior pause: lets us skip
    // Stage 1 entirely and jump straight to size-grouping. Empty
    // (None) ⇒ no saved inventory; do a fresh walk.
    let mut resumed_inventory: Option<Vec<crate::inventory::FileEntry>> = None;
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
        let inv_result =
            inventory::walk::enumerate_cancellable(&cfg, Some(&*cancel), |evt| {
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
                    .or_insert_with(|| {
                        crate::winapi_wrappers::volume_for_path(root).ok()
                    });
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

    // ---------------- Stage 2: size grouping ----------------
    let _ = tx.send(EngineEvent::Status("Stage 2 — size grouping".into()));
    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::SizeGroup,
        done: 0,
        total: 0,
        eta_secs: None,
    });
    let mut size_groups = pipeline::grouping::group_by_size(files);
    // Resolve inode ids only on files that survived size grouping —
    // singletons can't be hardlinks within this scan and don't need
    // the per-file GetFileInformationByHandle. See the docs on
    // `pipeline::grouping::resolve_file_ids`.
    pipeline::grouping::resolve_file_ids(&mut size_groups);
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
            (true, true) => "cache enabled — Stage 4 will fast-forward through already-hashed files".to_string(),
            (true, false) => "cache requested but failed to open — Stage 4 will re-hash everything".to_string(),
            (false, _) => "cache disabled in settings — Stage 4 will re-hash everything".to_string(),
        },
    });
    let mut total_cache_hits: u64 = 0;
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
    let _ = tx.try_send(EngineEvent::OverallProgress {
        stage: OverallStage::Hashing,
        done: 0,
        total: total_to_hash,
        eta_secs: None,
    });

    let mut total_bytes_read: u64 = 0;
    let mut total_dups: u64 = 0;
    let mut reclaimable: u64 = 0;
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
            let bytes_added = match &outcome {
                pipeline::hash::ProgressOutcome::Hashed { bytes } => *bytes,
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
                        message: format!("hash failed · {} · {error}", path.display()),
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
                    done: n,
                    total: total_to_hash_inner,
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
        total_cache_hits = total_cache_hits
            .saturating_add(counters.cache_hits.load(Ordering::Relaxed));
        total_cache_writes = total_cache_writes
            .saturating_add(counters.cache_writes.load(Ordering::Relaxed));
        for i in 0..4 {
            tier_micros_total[i] = tier_micros_total[i]
                .saturating_add(counters.tier_micros[i].load(Ordering::Relaxed));
            tier_bytes_total[i] =
                tier_bytes_total[i].saturating_add(counters.tier_bytes[i].load(Ordering::Relaxed));
            tier_count_total[i] =
                tier_count_total[i].saturating_add(counters.tier_count[i].load(Ordering::Relaxed));
        }
        placeholders_blocked_recall_total = placeholders_blocked_recall_total.saturating_add(
            counters
                .placeholders_blocked_recall
                .load(Ordering::Relaxed),
        );
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
            total_dups += 1;
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
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "scan complete: {} group(s), {} reclaimable",
            total_dups,
            crate::gui::theme::humansize(reclaimable)
        ),
    });
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
    // G1: build the leaderboard payload from this scan's results
    // and log its size. We don't auto-submit — that's the GUI
    // "Submit run" button's job. This step proves the integration
    // works end-to-end: hardware detect + scan totals + corpus
    // signature → canonical-JSON + HMAC sign-ready payload.
    #[cfg(feature = "telemetry")]
    {
        use crate::leaderboard::hardware;
        use crate::leaderboard::hmac_signer;
        use crate::leaderboard::submission;
        let wall_ms = scan_started_at
            .elapsed()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let defender_post = crate::diagnose::probe_defender().rtp_enabled;
        let inputs = submission::SubmissionInputs {
            run_uuid: uuid::Uuid::new_v4().to_string(),
            sd_version: env!("CARGO_PKG_VERSION").to_string(),
            hardware: hardware::detect(),
            scan: submission::ScanResults {
                files_scanned: total_files,
                bytes_scanned: total_bytes_read,
                wall_clock_ms: wall_ms,
                duplicate_groups: total_dups,
                reclaimable_inode_bytes: reclaimable,
                hash_algo: settings.hash_algo.tag().to_string(),
                defender_rtp_state_pre: defender_rtp_pre,
                defender_rtp_state_post: defender_post,
                corpus_signature_hash: corpus_sig.clone(),
            },
        };
        let payload = submission::build_payload(&inputs);
        let body = hmac_signer::canonical_body(&payload);
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!(
                "leaderboard payload ready: {} bytes (run_uuid={}, hw={}/{}c, corpus_sig={})",
                body.len(),
                inputs.run_uuid.split('-').next().unwrap_or(""),
                inputs.hardware.cpu_model_string,
                inputs.hardware.cpu_threads,
                corpus_sig.split(':').nth(1).map(|h| &h[..8]).unwrap_or(""),
            ),
        });
    }
    let _ = tx.send(EngineEvent::ScanFinished {
        at: Instant::now(),
        total_files,
        total_bytes_read,
        duplicates: total_dups,
        reclaimable_bytes: reclaimable,
    });
    // T2.1 phase 7 surface: tell the user how many files the tier
    // guard skipped, broken out by class. Silent when the corpus
    // had no placeholders (typical for non-OneDrive / non-WSL roots),
    // shown prominently otherwise so dropped dup-group counts
    // make sense at a glance.
    let placeholders_total = placeholders_blocked_recall_total
        .saturating_add(placeholders_blocked_other_reparse_total);
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
fn order_keeper_first(
    files: Vec<PathBuf>,
    strategy: crate::cli::KeepStrategy,
) -> Vec<PathBuf> {
    if files.len() < 2 {
        return files;
    }
    use crate::cli::KeepStrategy::*;
    // `First` is a no-op — the engine's natural order already wins.
    if matches!(strategy, First | Interactive) {
        return files;
    }
    let mtimes: Vec<Option<std::time::SystemTime>> = files
        .iter()
        .map(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok())
        .collect();
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
        queue_depth: None,
        output: None,
        follow_links: settings.follow_links,
        allow_system_paths: settings.allow_system_paths,
        // GUI settings don't surface the placeholder-policy knob yet;
        // tier guard defaults to conservative (refuse cloud recalls).
        // Phase 7 GUI counter exposes the bucket; a future iteration
        // can add the toggle if user feedback shows it's wanted.
        allow_recall_on_read: false,
        hash_algo: settings.hash_algo,
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
