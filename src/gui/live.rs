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
use crate::gui::events::{
    DriveInfo, DuplicateGroupSummary, EngineEvent, LogLevel, ReadSample, Stage,
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
    )
}

pub fn spawn_with_settings(
    tx: Sender<EngineEvent>,
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    cancel: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("superdupe-engine".into())
        .spawn(move || {
            if let Err(e) = run(tx.clone(), roots, settings, cancel) {
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
) -> crate::Result<()> {
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
                if r.is_reference { "★ reference  " } else { "             " },
                r.path.display()
            ),
        });
    }
    for (i, r) in root_paths.iter().enumerate() {
        let _ = tx.send(EngineEvent::DriveDiscovered(DriveInfo {
            id: i as u32,
            model: format!("Root {}", i + 1),
            has_seek_penalty: true,
            capacity_bytes: 0,
            volume_label: r.to_string_lossy().into_owned(),
        }));
    }

    // ---------------- Stage 1: inventory ----------------
    let _ = tx.send(EngineEvent::Status("Stage 1 — scanning files".into()));
    if cancel.load(Ordering::Relaxed) {
        emit_paused(&tx);
        return Ok(());
    }
    let inv_tx = tx.clone();
    let mut files_seen: u64 = 0;
    let mut dirs_entered: u64 = 0;
    let mut dirs_denied: u64 = 0;
    let mut entries_skipped: u64 = 0;
    let mut skipped_below_min: u64 = 0;
    let mut last_emit = Instant::now();
    let inv_result = inventory::walk::enumerate_with_progress(&cfg, |evt| {
        use crate::inventory::walk::WalkEvent;
        match evt {
            WalkEvent::Entered { path, depth } => {
                dirs_entered += 1;
                // Don't spam the status with every directory; show one
                // current path every ~250ms so the user sees activity.
                if last_emit.elapsed() > std::time::Duration::from_millis(250) {
                    last_emit = Instant::now();
                    let display = path.display().to_string();
                    let _ = inv_tx.send(EngineEvent::Status(format!(
                        "Walking: {} ({} dirs, {} files so far)",
                        truncate_tail(&display, 60),
                        dirs_entered,
                        files_seen,
                    )));
                    let _ = inv_tx.send(EngineEvent::StageTick {
                        stage: Stage::Inventory,
                        delta: 0,
                        total: files_seen,
                    });
                }
                let _ = depth; // reserved for a future visualisation
            }
            WalkEvent::FileFound { size: _, .. } => {
                files_seen += 1;
                if files_seen.is_multiple_of(200) {
                    let _ = inv_tx.send(EngineEvent::StageTick {
                        stage: Stage::Inventory,
                        delta: 200,
                        total: files_seen,
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
    let files = match inv_result {
        Ok(v) => v,
        Err(e) => {
            let _ = tx.send(EngineEvent::Log {
                level: LogLevel::Error,
                message: format!("inventory failed: {e}"),
            });
            return Err(e);
        }
    };
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
    if total_files == 0 {
        let mut hint = "Inventory returned 0 files.".to_string();
        if dirs_denied > 0 {
            hint.push_str(&format!(
                " {} director(ies) were permission-denied — try running superdupe-gui as administrator.",
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
    let size_groups = pipeline::grouping::group_by_size(files);
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

    if cancel.load(Ordering::Relaxed) {
        emit_paused(&tx);
        return Ok(());
    }

    // ---------------- Stage 3: layout ----------------
    let _ = tx.send(EngineEvent::Status("Stage 3 — layout".into()));
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

    // Smaller chunks → more frequent updates between chunks. We also
    // wire a per-file progress callback into the hasher so the UI
    // animates *within* a chunk, not just between them.
    let chunks = chunk_groups(laid, 32, 50);
    let total_chunks = chunks.len();
    let _ = tx.send(EngineEvent::Status(format!(
        "Stage 4 — hashing {} chunk(s)…",
        total_chunks
    )));

    let mut total_bytes_read: u64 = 0;
    let mut total_dups: u64 = 0;
    let mut reclaimable: u64 = 0;
    let mut tier3_done: u64 = 0;
    let mut confirmed: u64 = 0;
    let files_hashed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let bytes_hashed = Arc::new(std::sync::atomic::AtomicU64::new(0));

    let already_reported: hashbrown::HashSet<String> = checkpoint_state
        .completed_hashes
        .iter()
        .cloned()
        .collect();

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
            emit_paused(&tx);
            return Ok(());
        }
        let progress_tx = tx.clone();
        let progress_files = Arc::clone(&files_hashed);
        let progress_bytes = Arc::clone(&bytes_hashed);
        let progress_drive = (i as u32) % roots.len().max(1) as u32;
        let on_file: pipeline::hash::FileProgress = Arc::new(move |_tier, bytes| {
            let n = progress_files.fetch_add(1, Ordering::Relaxed) + 1;
            let total_bytes =
                progress_bytes.fetch_add(bytes, Ordering::Relaxed).saturating_add(bytes);
            // Drive scope: one ReadSample every 5 files. Cumulative
            // bytes go on the LCN axis so the trace climbs.
            if n.is_multiple_of(5) {
                let _ = progress_tx.send(EngineEvent::Read(ReadSample {
                    drive: progress_drive,
                    lcn_bytes: total_bytes,
                    bytes,
                    latency_us: 1,
                    at: Instant::now(),
                }));
            }
            // Funnel + survival ratio: a Tier 3 tick every 25 files.
            if n.is_multiple_of(25) {
                let _ = progress_tx.send(EngineEvent::StageTick {
                    stage: Stage::Tier3Full,
                    delta: 25,
                    total: n,
                });
            }
        });

        let (dups, counters) = pipeline::hash::run_with_progress(
            chunk,
            &cfg,
            cache.clone(),
            on_file,
        )?;
        let chunk_bytes = counters.bytes_read.load(Ordering::Relaxed);
        total_bytes_read = total_bytes_read.saturating_add(chunk_bytes);

        for g in dups {
            if already_reported.contains(&g.content_hash) {
                continue; // carried over from a prior checkpoint
            }
            let visible_files = filter_reference_only(g.files, &reference_set);
            if visible_files.len() < 2 {
                continue;
            }
            confirmed += 1;
            let savings = g.size.saturating_mul(visible_files.len().saturating_sub(1) as u64);
            reclaimable = reclaimable.saturating_add(savings);
            total_dups += 1;
            let summary = DuplicateGroupSummary {
                size: g.size,
                content_hash: g.content_hash,
                files: visible_files,
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
        let _ = tx.send(EngineEvent::StageTick {
            stage: Stage::Tier3Full,
            delta: 1,
            total: tier3_done,
        });
        let _ = tx.send(EngineEvent::StageTick {
            stage: Stage::Confirmed,
            delta: 0,
            total: confirmed,
        });
        let _ = tx.send(EngineEvent::Status(format!(
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

    let _ = tx.send(EngineEvent::ScanFinished {
        at: Instant::now(),
        total_files,
        total_bytes_read,
        duplicates: total_dups,
        reclaimable_bytes: reclaimable,
    });
    let _ = tx.send(EngineEvent::Log {
        level: LogLevel::Info,
        message: format!(
            "scan complete: {} group(s), {} reclaimable",
            total_dups,
            crate::gui::theme::humansize(reclaimable)
        ),
    });
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
    let any_non_reference = files
        .iter()
        .any(|p| !reference_belongs(p, reference_set));
    if !any_non_reference {
        return Vec::new();
    }
    files
}

fn reference_belongs(path: &PathBuf, reference_set: &hashbrown::HashSet<PathBuf>) -> bool {
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
        b.add(Glob::new(&settings.include_glob).map_err(|e| {
            crate::Error::BadGlob {
                pattern: settings.include_glob.clone(),
                source: e,
            }
        })?);
        Some(b.build().map_err(|e| crate::Error::BadGlob {
            pattern: settings.include_glob.clone(),
            source: e,
        })?)
    };
    let exclude = if settings.exclude_glob.is_empty() {
        None
    } else {
        let mut b = GlobSetBuilder::new();
        b.add(Glob::new(&settings.exclude_glob).map_err(|e| {
            crate::Error::BadGlob {
                pattern: settings.exclude_glob.clone(),
                source: e,
            }
        })?);
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
        threads: settings
            .threads
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(1)
            }),
        queue_depth: None,
        output: None,
        follow_links: settings.follow_links,
        allow_system_paths: settings.allow_system_paths,
    })
}
