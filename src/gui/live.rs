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
    let inv_result = inventory::enumerate(&cfg);
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
        delta: total_files,
        total: total_files,
    });
    if total_files == 0 {
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Warn,
            message: "Inventory returned 0 files. Either the roots are empty, all files are below the min-size filter, or permission was denied on the directories.".into(),
        });
    } else {
        let _ = tx.send(EngineEvent::Log {
            level: LogLevel::Info,
            message: format!("inventory complete: {} file(s)", total_files),
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

    // Chunk the hashing so per-chunk events flow to the UI as work
    // progresses. Each chunk is internally rayon-parallelised by
    // `pipeline::hash::run_with_counters`, so we keep CPU saturated
    // while still emitting smooth progress.
    let chunks = chunk_groups(laid, 8);
    let total_chunks = chunks.len();
    let _ = tx.send(EngineEvent::Status(format!(
        "Stage 4 — hashing {} candidate group(s)…",
        total_chunks
    )));

    let mut total_bytes_read: u64 = 0;
    let mut total_dups: u64 = 0;
    let mut reclaimable: u64 = 0;
    let mut tier3_done: u64 = 0;
    let mut confirmed: u64 = 0;

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
        let (dups, counters) =
            pipeline::hash::run_with_counters(chunk, &cfg, cache.clone())?;
        let chunk_bytes = counters.bytes_read.load(Ordering::Relaxed);
        total_bytes_read = total_bytes_read.saturating_add(chunk_bytes);

        // Cheap synthetic ReadSample so the live drive scope animates
        // during real scans, not just demos. Each chunk produces one
        // sample carrying the bytes-read total; the LCN is the
        // cumulative total so the scope shows a rising trace.
        let _ = tx.send(EngineEvent::Read(ReadSample {
            drive: (i as u32) % roots.len().max(1) as u32,
            lcn_bytes: total_bytes_read,
            bytes: chunk_bytes,
            latency_us: 1,
            at: Instant::now(),
        }));

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

fn chunk_groups(
    laid: Vec<pipeline::layout::LaidOutGroup>,
    target_chunks: usize,
) -> Vec<Vec<pipeline::layout::LaidOutGroup>> {
    if laid.is_empty() {
        return Vec::new();
    }
    let chunk_size = (laid.len() / target_chunks.max(1)).max(1);
    let mut chunks = Vec::with_capacity(target_chunks);
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
