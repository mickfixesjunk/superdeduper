//! Bridge between the real scan engine and the GUI's event channel.
//!
//! When `superdupe-gui --live <paths…>` is invoked, [`spawn`] kicks off
//! a worker thread that runs the same engine the CLI uses and emits
//! the same [`EngineEvent`]s the demo does. The UI thread never knows
//! the difference.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use crossbeam_channel::Sender;
use parking_lot::Mutex;

use crate::cache::Cache;
use crate::cli::OutputFormat;
use crate::config::ScanConfig;
use crate::gui::events::{DuplicateGroupSummary, EngineEvent, Stage};
use crate::inventory;
use crate::pipeline;

pub fn spawn(tx: Sender<EngineEvent>, roots: Vec<PathBuf>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("superdupe-engine".into())
        .spawn(move || {
            if let Err(e) = run(tx.clone(), roots.clone()) {
                let _ = tx.send(EngineEvent::Status(format!("engine error: {e}")));
            }
        })
        .expect("spawn engine thread")
}

fn run(tx: Sender<EngineEvent>, roots: Vec<PathBuf>) -> crate::Result<()> {
    let cfg = ScanConfig {
        roots: roots.clone(),
        reference_roots: vec![],
        min_size: 4096,
        max_size: None,
        include: None,
        exclude: None,
        format: OutputFormat::Json,
        paranoid: false,
        use_cache: true,
        use_format_aware: true,
        threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
        queue_depth: None,
        output: None,
        follow_links: false,
        allow_system_paths: false,
    };

    let _ = tx.send(EngineEvent::ScanStarted {
        at: Instant::now(),
        roots: roots.clone(),
    });
    let _ = tx.send(EngineEvent::Status("Stage 1 — inventory".into()));

    let files = inventory::enumerate(&cfg)?;
    let total_files = files.len() as u64;
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::Inventory,
        delta: total_files,
        total: total_files,
    });

    let _ = tx.send(EngineEvent::Status("Stage 2 — size grouping".into()));
    let size_groups = pipeline::grouping::group_by_size(files);
    let candidates: u64 = size_groups.iter().map(|g| g.files.len() as u64).sum();
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::SizeGroup,
        delta: candidates,
        total: candidates,
    });

    let _ = tx.send(EngineEvent::Status("Stage 3 — layout".into()));
    let laid = pipeline::layout::resolve(size_groups)?;
    let laid_count: u64 = laid.iter().map(|g| g.files.len() as u64).sum();
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::LayoutResolve,
        delta: laid_count,
        total: laid_count,
    });

    let cache = if cfg.use_cache {
        crate::cache::default_cache_path()
            .and_then(|p| Cache::open(&p))
            .ok()
            .map(|c| Arc::new(Mutex::new(c)))
    } else {
        None
    };

    let _ = tx.send(EngineEvent::Status("Stage 4 — progressive hashing".into()));
    let (dups, counters) = pipeline::hash::run_with_counters(laid, &cfg, cache)?;
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::Tier3Full,
        delta: dups.len() as u64,
        total: dups.len() as u64,
    });
    let _ = tx.send(EngineEvent::StageTick {
        stage: Stage::Confirmed,
        delta: dups.len() as u64,
        total: dups.len() as u64,
    });

    let mut reclaimable = 0u64;
    for g in &dups {
        let savings = g.size.saturating_mul(g.files.len().saturating_sub(1) as u64);
        reclaimable = reclaimable.saturating_add(savings);
        let _ = tx.send(EngineEvent::DuplicateFound(DuplicateGroupSummary {
            size: g.size,
            content_hash: g.content_hash.clone(),
            files: g.files.clone(),
        }));
    }

    let _ = tx.send(EngineEvent::ScanFinished {
        at: Instant::now(),
        total_files,
        total_bytes_read: counters.bytes_read.load(Ordering::Relaxed),
        duplicates: dups.len() as u64,
        reclaimable_bytes: reclaimable,
    });
    Ok(())
}
