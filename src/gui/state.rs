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
    /// Content-hash algorithm. Defaults to RIVER5 since v0.2 — see
    /// `HashAlgo::default()`.
    #[serde(default)]
    pub hash_algo: crate::pipeline::hash::HashAlgo,
    /// Skip the "type DELETE to confirm" modal on every destructive
    /// action (Recycle / SafeRename / Hardlink / bulk Safe-rename).
    /// Default `false` — every destructive action prompts. Power
    /// users who run dedup against the same corpus repeatedly can
    /// flip this in Settings → Safety to bypass the prompt.
    ///
    /// Reveal-in-Explorer and Unsuperdeduper (the reverse operation)
    /// never prompt regardless — Reveal touches nothing, and
    /// Unsuperdeduper only RESTORES files that were safe-renamed,
    /// so the prompt would be more friction than safety.
    #[serde(default)]
    pub bypass_destructive_confirmation: bool,
    /// Suppress the "cache available — use it?" banner above the
    /// scan controls. When `false` (default) the banner appears
    /// whenever superdeduper finds inventory-meta data for any of
    /// the scan roots' volumes; the user picks per-scan whether to
    /// use the cache via the banner's toggle. When `true` the
    /// banner is hidden and the cache is silently used whenever
    /// available — the original behavior before v0.1.5.
    #[serde(default)]
    pub always_use_cache: bool,
    /// Suppress the alpha-software warning modal on startup. Flipped
    /// to `true` when the user clicks "Don't show again" in the
    /// modal. Default `false` — every launch shows the warning so a
    /// new user can't accidentally bulk-delete without seeing the
    /// caveats. Persisted across restarts.
    #[serde(default)]
    pub dismissed_alpha_warning: bool,
    /// Skip the credit-report-style pre-flight modal that runs
    /// `diagnose` against every drive in the scan before launching.
    /// Default `false` — pre-flight runs unless the user has flipped
    /// this in Settings → "Skip pre-flight modal". Independent of the
    /// per-probe "Skip pre-flight →" button which fires once mid-probe;
    /// this is the persistent always-off preference.
    #[serde(default)]
    pub skip_preflight: bool,
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
            hash_algo: crate::pipeline::hash::HashAlgo::default(),
            bypass_destructive_confirmation: false,
            always_use_cache: false,
            skip_preflight: false,
            dismissed_alpha_warning: false,
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

/// Snapshot of an in-flight long-running user-requested action
/// (Safe-rename, Archive, Recycle, Hardlink, Unsuperdeduper). Drives
/// the spinner-modal that pops up while the worker thread is
/// processing. `total = None` means the worker can't predict its
/// total upfront (Unsuperdeduper walks for `.superdeduper` markers)
/// and the modal renders an indeterminate spinner with a running
/// "X processed" counter.
#[derive(Debug, Clone)]
pub struct ActionState {
    pub name: String,
    pub total: Option<u64>,
    pub done: u64,
    /// Path (or short label) currently being processed. Shown in the
    /// modal so the user sees motion on what would otherwise look
    /// like a frozen spinner.
    pub current: Option<String>,
}

/// One per-volume summary populated by `App::refresh_cache_banner`
/// before paint. Lets the banner widget display `{count} files
/// cached, captured {age}` without re-querying SQLite per frame.
#[derive(Debug, Clone)]
pub struct CacheVolumeSummary {
    pub volume_guid: String,
    pub captured_at_unix: i64,
    pub record_count: u64,
}

pub struct UiState {
    pub scan_started_at: Option<Instant>,
    pub scan_finished_at: Option<Instant>,
    pub status: String,
    pub roots: Vec<PathBuf>,
    /// Populated whenever the scan roots change; one entry per
    /// volume of the current scan roots that has cached inventory
    /// data. Empty = no banner. See widgets/cache_banner.rs.
    pub cache_volume_summaries: Vec<CacheVolumeSummary>,
    /// Banner-toggle state, persisted in-memory across redraws but
    /// reset between sessions. `true` means the next scan will use
    /// the cache; user can flip via the banner toggle. Has no
    /// effect when settings.always_use_cache is `true` (banner
    /// hidden, cache silently used).
    pub use_cache_for_next_scan: bool,
    /// `Some` while a worker thread is processing a destructive
    /// action (recycle, hardlink, safe-rename, archive,
    /// unsuperdeduper). Cleared on `ActionFinished`. Drives the
    /// modal-with-spinner overlay.
    pub action_in_progress: Option<ActionState>,

    pub stage_counts: HashMap<Stage, StageCounter>,
    pub drives: HashMap<DriveId, DriveLive>,
    pub duplicates: Vec<DuplicateGroupSummary>,
    pub totals: Totals,
    pub logs: VecDeque<LogEntry>,
    pub overall: OverallProgress,
}

impl Default for UiState {
    fn default() -> Self {
        Self {
            scan_started_at: None,
            scan_finished_at: None,
            status: String::new(),
            roots: Vec::new(),
            cache_volume_summaries: Vec::new(),
            // Default ON — banner toggle starts checked so the most
            // common path (user wants the cache) is a single Start
            // click. Flipping off is explicit per-scan opt-out.
            use_cache_for_next_scan: true,
            action_in_progress: None,
            stage_counts: HashMap::new(),
            drives: HashMap::new(),
            duplicates: Vec::new(),
            totals: Totals::default(),
            logs: VecDeque::new(),
            overall: OverallProgress::default(),
        }
    }
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
            // Holds ~10 buckets/sec × 30s window = ~300 entries. The
            // self-trim in roll_throughput keeps it bounded.
            throughput: VecDeque::with_capacity(320),
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
    /// Buckets are at most ~100 ms apart (was 1 s), so the sparkline
    /// gets ~10 samples per second and the trace looks like a
    /// continuous line instead of stepping in big chunks. Each bucket
    /// holds the bytes seen in the trailing 1-second window so the
    /// reported MB/s is still a stable per-second figure, not a
    /// tenth-of-a-second spike. Old buckets fall off after
    /// [`THROUGHPUT_WINDOW_SECS`] seconds so the sparkline self-trims.
    pub fn roll_throughput(&mut self, now: Instant) {
        // 100 ms cadence ⇒ ~10 Hz update; below the screen refresh of
        // a 60 Hz monitor so we still get a fresh frame every render,
        // but cheap enough that we're not summing the reads buffer 60
        // times a second on a quiet drive.
        const BUCKET_PERIOD_MS: u64 = 100;
        let should_push = match self.throughput.back() {
            Some((t, _)) => {
                now.saturating_duration_since(*t) >= Duration::from_millis(BUCKET_PERIOD_MS)
            }
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
                // Hardlinked groups already share storage on disk —
                // the (n-1) × size figure overcounts the actual
                // reclaimable space (it's zero, the data is shared).
                // Exclude them from the header Reclaimable stat so
                // the user isn't told they can recover space that's
                // already been recovered. The groups still show in
                // the table (badged distinctly via the GUI).
                if !g.link_equivalent {
                    let savings = g
                        .size
                        .saturating_mul(g.files.len().saturating_sub(1) as u64);
                    self.totals.reclaimable_bytes =
                        self.totals.reclaimable_bytes.saturating_add(savings);
                }
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
                // Roll the wallclock into the status so a user
                // glancing at the bar sees the duration of the
                // completed scan, not just "Done."
                self.status = match self.scan_started_at {
                    Some(start) => format!(
                        "Done — scanned in {}.",
                        fmt_wallclock(at.saturating_duration_since(start))
                    ),
                    None => "Done.".into(),
                };
                // Audible "ding" so a user who walked away during a
                // long scan knows it finished. Fires on a detached
                // thread, swallows audio-device failures silently.
                // Gated on the `audio` feature so non-audio builds
                // (CI tests, headless headless dev) compile without
                // the rodio/alsa-sys transitive deps.
                #[cfg(feature = "audio")]
                crate::gui::sound::play_done_chime();
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
            EngineEvent::ActionStarted { name, total } => {
                self.action_in_progress = Some(ActionState {
                    name,
                    total,
                    done: 0,
                    current: None,
                });
            }
            EngineEvent::ActionProgress { done, current } => {
                if let Some(a) = &mut self.action_in_progress {
                    a.done = done;
                    a.current = current;
                }
            }
            EngineEvent::ActionFinished { summary } => {
                self.action_in_progress = None;
                self.status = summary;
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

/// Human-readable wallclock formatter shared by the header tile and
/// the "Done — Xm Ys" status line. Picks the right unit so a 4-second
/// scan reads `4.2s` while a 6-minute scan reads `6m 12s` and a
/// multi-hour run reads `1h 8m`.
pub fn fmt_wallclock(d: Duration) -> String {
    let total = d.as_secs_f64();
    if total < 60.0 {
        format!("{:.1}s", total)
    } else if total < 3600.0 {
        let mins = (total / 60.0) as u64;
        let secs = (total - (mins as f64) * 60.0) as u64;
        format!("{}m {:02}s", mins, secs)
    } else {
        let hours = (total / 3600.0) as u64;
        let mins = ((total - (hours as f64) * 3600.0) / 60.0) as u64;
        format!("{}h {:02}m", hours, mins)
    }
}
