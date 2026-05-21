//! Live UI state. The render thread reads from this; the event drain
//! mutates it once per frame. No locking — single-threaded by design.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use hashbrown::HashMap;
use serde::{Deserialize, Serialize};

use crate::gui::events::{
    DriveId, DriveInfo, DuplicateGroupSummary, EngineEvent, LogLevel, OverallStage, ReadSample,
    Stage,
};

/// Ring buffer of read samples for the live scope, capped to keep
/// rendering fast even on multi-hour scans.
const SCOPE_BUFFER: usize = 4096;
/// Throughput sparkline window (seconds shown on the X axis).
pub const THROUGHPUT_WINDOW_SECS: f64 = 30.0;

/// One row in the Roots panel. `is_reference` files are never offered
/// for destructive action; they're always treated as keepers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RootEntry {
    pub path: PathBuf,
    pub is_reference: bool,
}

/// Persisted user preferences. Loaded via egui's persistence on
/// startup; survives app restarts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScanSettings {
    pub min_size_bytes: u64,
    pub max_size_bytes: Option<u64>,
    pub include_glob: String,
    pub exclude_glob: String,
    pub use_format_aware: bool,
    pub use_cache: bool,
    pub paranoid: bool,
    pub follow_links: bool,
    pub threads: Option<usize>,
    /// Per-scan I/O worker count for the hashing par_iter. None ⇒
    /// engine picks `threads × 3` to oversubscribe past the cold-
    /// metadata `open()` wait on small files. `#[serde(default)]`
    /// keeps old persisted settings (without this field) loadable.
    #[serde(default)]
    pub io_threads: Option<usize>,
    pub allow_system_paths: bool,
    /// Content-hash algorithm: BLAKE3 (default, cryptographic) or
    /// River128 (16-byte output, AES-NI hardware-accelerated; was
    /// `ddh128` prior to the crate rename).
    #[serde(default)]
    pub hash_algo: crate::pipeline::hash::HashAlgo,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            min_size_bytes: 4 * 1024,
            max_size_bytes: None,
            include_glob: String::new(),
            exclude_glob: String::new(),
            use_format_aware: true,
            use_cache: true,
            paranoid: false,
            follow_links: false,
            threads: None,
            io_threads: None,
            allow_system_paths: false,
            hash_algo: crate::pipeline::hash::HashAlgo::Blake3,
        }
    }
}

/// One row in the Log panel. Surfaced from `EngineEvent::Log`.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: Instant,
    pub level: LogLevel,
    pub message: String,
}

#[derive(Default)]
pub struct UiState {
    pub scan_started_at: Option<Instant>,
    pub scan_finished_at: Option<Instant>,
    pub status: String,
    pub roots: Vec<PathBuf>,

    pub stage_counts: HashMap<Stage, StageCounter>,
    pub drives: HashMap<DriveId, DriveLive>,
    pub duplicates: Vec<DuplicateGroupSummary>,
    pub totals: Totals,
    pub logs: VecDeque<LogEntry>,
    pub overall: OverallProgress,
}

#[derive(Copy, Clone, Debug)]
pub struct OverallProgress {
    pub stage: OverallStage,
    pub done: u64,
    pub total: u64,
    pub eta_secs: Option<f32>,
}

impl Default for OverallProgress {
    fn default() -> Self {
        Self {
            stage: OverallStage::Idle,
            done: 0,
            total: 0,
            eta_secs: None,
        }
    }
}

impl OverallProgress {
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.done as f64 / self.total as f64).clamp(0.0, 1.0) as f32
        }
    }

    pub fn is_determinate(&self) -> bool {
        self.total > 0
    }
}

#[derive(Default, Copy, Clone)]
pub struct Totals {
    pub files: u64,
    pub bytes_read: u64,
    pub duplicates: u64,
    pub reclaimable_bytes: u64,
}

#[derive(Default, Copy, Clone)]
pub struct StageCounter {
    pub total: u64,
    pub last_delta: u64,
    pub last_update: Option<Instant>,
}

pub struct DriveLive {
    pub info: DriveInfo,
    /// Recent reads in arrival order, used for the LCN trace.
    pub reads: VecDeque<ReadSample>,
    /// Per-second bytes-read totals (newest at the back).
    pub throughput: VecDeque<(Instant, u64)>,
    /// Total bytes read this scan.
    pub bytes_read: u64,
    /// Most recent observed LCN, in clusters-ish (the UI just uses it
    /// as an opaque Y coordinate).
    pub last_lcn: u64,
    /// Rolling estimate of the drive's peak MB/s observed this run.
    pub peak_mbps: f32,
}

impl DriveLive {
    pub fn new(info: DriveInfo) -> Self {
        Self {
            info,
            reads: VecDeque::with_capacity(SCOPE_BUFFER),
            throughput: VecDeque::with_capacity(64),
            bytes_read: 0,
            last_lcn: 0,
            peak_mbps: 0.0,
        }
    }

    pub fn push_read(&mut self, sample: ReadSample) {
        if self.reads.len() == SCOPE_BUFFER {
            self.reads.pop_front();
        }
        self.last_lcn = sample.lcn_bytes;
        self.bytes_read = self.bytes_read.saturating_add(sample.bytes);
        self.reads.push_back(sample);
    }

    /// Roll the throughput window — call once a frame from the UI.
    ///
    /// At most one bucket per second; each bucket holds the bytes seen
    /// in the trailing 1-second window. Old buckets fall off after
    /// [`THROUGHPUT_WINDOW_SECS`] seconds so the sparkline self-trims.
    pub fn roll_throughput(&mut self, now: Instant) {
        let should_push = match self.throughput.back() {
            Some((t, _)) => now.saturating_duration_since(*t) >= Duration::from_millis(950),
            None => !self.reads.is_empty(),
        };
        if should_push {
            let cutoff = now - Duration::from_secs(1);
            let bytes_in_window: u64 = self
                .reads
                .iter()
                .rev()
                .take_while(|r| r.at >= cutoff)
                .map(|r| r.bytes)
                .sum();
            self.throughput.push_back((now, bytes_in_window));
            let mbps = bytes_in_window as f32 / 1_048_576.0;
            if mbps > self.peak_mbps {
                self.peak_mbps = mbps;
            }
        }

        let drop_cutoff = now - Duration::from_secs_f64(THROUGHPUT_WINDOW_SECS);
        while let Some((t, _)) = self.throughput.front() {
            if *t < drop_cutoff {
                self.throughput.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn current_mbps(&self) -> f32 {
        self.throughput
            .back()
            .map(|(_, b)| *b as f32 / 1_048_576.0)
            .unwrap_or(0.0)
    }
}

impl UiState {
    pub fn apply(&mut self, ev: EngineEvent) {
        match ev {
            EngineEvent::ScanStarted { at, roots } => {
                *self = UiState::default();
                self.scan_started_at = Some(at);
                self.roots = roots;
                self.status = "Scanning…".into();
            }
            EngineEvent::DriveDiscovered(info) => {
                let id = info.id;
                self.drives.insert(id, DriveLive::new(info));
            }
            EngineEvent::StageTick {
                stage,
                delta,
                total,
            } => {
                let c = self.stage_counts.entry(stage).or_default();
                c.total = total.max(c.total.saturating_add(delta));
                c.last_delta = delta;
                c.last_update = Some(Instant::now());
            }
            EngineEvent::Read(sample) => {
                if let Some(drive) = self.drives.get_mut(&sample.drive) {
                    drive.push_read(sample);
                }
                self.totals.bytes_read = self.totals.bytes_read.saturating_add(sample.bytes);
            }
            EngineEvent::DuplicateFound(g) => {
                self.totals.duplicates = self.totals.duplicates.saturating_add(1);
                let savings = g
                    .size
                    .saturating_mul(g.files.len().saturating_sub(1) as u64);
                self.totals.reclaimable_bytes =
                    self.totals.reclaimable_bytes.saturating_add(savings);
                self.duplicates.push(g);
            }
            EngineEvent::ScanFinished {
                at,
                total_files,
                total_bytes_read,
                duplicates,
                reclaimable_bytes,
            } => {
                self.scan_finished_at = Some(at);
                self.totals = Totals {
                    files: total_files,
                    bytes_read: total_bytes_read,
                    duplicates,
                    reclaimable_bytes,
                };
                self.status = "Done.".into();
                self.overall = OverallProgress {
                    stage: OverallStage::Idle,
                    done: total_files,
                    total: total_files.max(1),
                    eta_secs: Some(0.0),
                };
            }
            EngineEvent::ScanPaused { at, checkpoint_id } => {
                self.scan_finished_at = Some(at);
                self.status = format!("Paused — resume by clicking Scan ({} saved)", checkpoint_id);
                self.overall.eta_secs = None;
            }
            EngineEvent::Status(s) => self.status = s,
            EngineEvent::Log { level, message } => {
                self.push_log(level, message);
            }
            EngineEvent::OverallProgress {
                stage,
                done,
                total,
                eta_secs,
            } => {
                self.overall = OverallProgress {
                    stage,
                    done,
                    total,
                    eta_secs,
                };
            }
        }
    }

    pub fn push_log(&mut self, level: LogLevel, message: String) {
        if self.logs.len() >= 1024 {
            self.logs.pop_front();
        }
        self.logs.push_back(LogEntry {
            at: Instant::now(),
            level,
            message,
        });
    }

    pub fn scan_elapsed(&self) -> Option<Duration> {
        let start = self.scan_started_at?;
        let end = self.scan_finished_at.unwrap_or_else(Instant::now);
        Some(end.saturating_duration_since(start))
    }
}
