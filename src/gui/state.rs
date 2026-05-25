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
    /// Which-file-to-keep strategy applied at duplicate-confirm time.
    /// Default Smart — heuristic scoring against Recycle Bin / temp
    /// / draft / `(1)` penalties + depth + newness bonuses.
    /// Other strategies (Oldest, Newest, ShortestPath, …) are
    /// available via Settings → Keep Strategy. Interactive only makes
    /// sense in the CLI, so the GUI hides it.
    #[serde(default)]
    pub keep_strategy: crate::cli::KeepStrategy,
    /// #41 — Settings → Privacy → Scan history retention. Number of
    /// days to keep `scan_history` rows before auto-pruning on app
    /// start. `0` = forever (default; matches v1 behavior — no
    /// retention enforcement). The Privacy widget renders 30 / 90 /
    /// 365 / forever as the four standard buckets.
    #[serde(default)]
    pub history_retention_days: u32,
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
            keep_strategy: crate::cli::KeepStrategy::Smart,
            history_retention_days: 0,
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
    /// Identity index for `self.duplicates`. The #39 fix needs to
    /// dedupe `DuplicateFound` events against already-known
    /// content_hashes; a `HashSet` keeps that check O(1) per event
    /// (vs O(n) on `self.duplicates.iter().any(...)`). Without this,
    /// accumulating N groups becomes O(N²) — the #40 perf hit on
    /// 26K+-group scans. Membership must stay in lockstep with the
    /// `duplicates` Vec; every push here pushes there + vice versa.
    pub duplicate_hashes: hashbrown::HashSet<String>,
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
            duplicate_hashes: hashbrown::HashSet::new(),
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
                // #39 fix — on resume the App's `adopt_resume_state`
                // populates `self.duplicates` from the on-disk
                // checkpoint BEFORE the engine spawns; the engine
                // then re-emits the same groups via DuplicateFound
                // to keep the streaming-event surface uniform.
                // Without de-dupe, every resumed group would be
                // counted twice in `totals.duplicates` + appear
                // twice in the Groups table. Content_hash is the
                // right identity: BLAKE3 hex for byte-identical;
                // `perceptual-{fp}` token for Tier-4. Both are
                // stable across the resume hand-off.
                //
                // #40 perf — the dedupe check MUST be O(1), not the
                // earlier O(n) `iter().any(...)` scan. With 26K+
                // groups accumulating per scan, the linear check made
                // state.apply O(n²) for the population phase — Mick's
                // "sluggish during dupe population" symptom. HashSet
                // membership is constant-time.
                if !self.duplicate_hashes.insert(g.content_hash.clone()) {
                    return;
                }
                self.totals.duplicates = self.totals.duplicates.saturating_add(1);
                let savings = inode_aware_savings(&g);
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
            EngineEvent::ArchiveActionSummary(_) => {
                // #80 Bug C — handled in app.rs::drain_events
                // (pops the post-archive summary modal). UiState
                // doesn't store the rollup; the modal owns it.
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

/// True reclaimable bytes for a duplicate group. Returns 0 when the
/// group is purely hardlinked (data already shared on disk; nothing
/// to free). Uses `unique_inodes` for the partial-hardlink case so
/// a group with 8 path-aliases sharing 3 inodes credits `2 * size`,
/// not `7 * size`. Falls back to `files.len()` when `unique_inodes`
/// is 0 (older checkpoint formats that pre-date the field).
pub fn inode_aware_savings(g: &DuplicateGroupSummary) -> u64 {
    if g.link_equivalent {
        return 0;
    }
    let unique = if g.unique_inodes == 0 {
        g.files.len() as u64
    } else {
        g.unique_inodes
    };
    if unique < 2 {
        return 0;
    }
    g.size.saturating_mul(unique - 1)
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

#[cfg(test)]
mod inode_aware_tests {
    use super::*;
    use std::path::PathBuf;

    fn mk_group(
        size: u64,
        n_files: usize,
        link_equivalent: bool,
        unique_inodes: u64,
    ) -> DuplicateGroupSummary {
        // Stamp content_hash with the group's distinguishing attrs so
        // every call to `mk_group` produces a unique hash. The #39
        // dedupe-on-DuplicateFound check uses content_hash as the
        // identity key; sharing a hash across test groups would
        // collapse them into one in the apply loop.
        let content_hash = format!("h-{size}-{n_files}-{link_equivalent}-{unique_inodes}");
        DuplicateGroupSummary {
            size,
            content_hash,
            files: (0..n_files)
                .map(|i| PathBuf::from(format!("/p/{i}")))
                .collect(),
            link_equivalent,
            unique_inodes,
            similarity_kind: crate::pipeline::SimilarityKind::ByteIdentical,
        }
    }

    #[test]
    fn link_equivalent_group_returns_zero() {
        // Stream A — every file shares one inode, nothing to reclaim.
        let g = mk_group(4096, 8, true, 1);
        assert_eq!(inode_aware_savings(&g), 0);
    }

    #[test]
    fn single_alias_group_uses_files_count() {
        // Stream B — N distinct inodes, N-1 are reclaimable.
        let g = mk_group(1000, 5, false, 5);
        assert_eq!(inode_aware_savings(&g), 4000);
    }

    #[test]
    fn unique_inodes_zero_falls_back_to_files_len() {
        // Older checkpoint format — `unique_inodes` was missing.
        // Fall back to path-aware count for backwards compat.
        let g = mk_group(100, 3, false, 0);
        assert_eq!(inode_aware_savings(&g), 200);
    }

    #[test]
    fn partial_hardlink_uses_unique_inodes_not_files_count() {
        // 8 path-aliases sharing 3 inodes → (3-1)*size, not (8-1)*size.
        // This is the C:\Windows / WinSxS shape that was inflating
        // header reclaim 4x on Mick's scans before the fix.
        let g = mk_group(1000, 8, false, 3);
        assert_eq!(inode_aware_savings(&g), 2000);
        // The path-aware figure for the same group would be 7000.
        // Verify they actually diverge (the test is meaningful).
        assert_ne!(inode_aware_savings(&g), 7000);
    }

    #[test]
    fn unique_inodes_one_returns_zero() {
        // Edge case: link_equivalent flag missed but inode count
        // says single inode → still 0 reclaimable. Defends against
        // a future bug where Stream A is mis-tagged.
        let g = mk_group(4096, 5, false, 1);
        assert_eq!(inode_aware_savings(&g), 0);
    }

    #[test]
    fn zero_size_group_returns_zero() {
        // Empty files: every file is its own inode, but (count-1) *
        // 0 is still 0. Don't crash on 0-byte content.
        let g = mk_group(0, 4, false, 4);
        assert_eq!(inode_aware_savings(&g), 0);
    }

    #[test]
    fn saturating_mul_does_not_overflow_at_u64_max() {
        // Defense against a corrupted profile claiming a huge group.
        // saturating_mul caps at u64::MAX instead of panicking.
        let g = mk_group(u64::MAX, 3, false, 3);
        // (3-1)*u64::MAX saturates to u64::MAX.
        assert_eq!(inode_aware_savings(&g), u64::MAX);
    }

    #[test]
    fn ui_state_totals_match_inode_aware_after_full_event_stream() {
        // Walk a UiState through the same event sequence the engine
        // emits at scan-end: a mix of DuplicateFound (link-eq +
        // single-alias) → ScanFinished. The header's reclaimable
        // tile reads `self.totals.reclaimable_bytes` which gets
        // overwritten by ScanFinished, so this test pins both
        // the accumulator path AND the wholesale overwrite path.
        use crate::gui::events::{EngineEvent, OverallStage};
        let mut state = UiState::default();

        // Three groups: one hardlinked (link_equivalent), two
        // single-alias. Path-aware would sum to a different total
        // than inode-aware on hardlinked groups.
        let g_hardlink = mk_group(1_000_000_000, 8, true, 1); // 0 reclaim
        let g_normal_a = mk_group(50_000_000, 3, false, 3); // 100M reclaim
        let g_normal_b = mk_group(20_000_000, 2, false, 2); // 20M reclaim

        state.apply(EngineEvent::DuplicateFound(g_hardlink.clone()));
        state.apply(EngineEvent::DuplicateFound(g_normal_a.clone()));
        state.apply(EngineEvent::DuplicateFound(g_normal_b.clone()));

        // Pre-ScanFinished, the accumulator should equal the
        // sum of inode-aware savings across all three groups.
        let expected = inode_aware_savings(&g_hardlink)
            + inode_aware_savings(&g_normal_a)
            + inode_aware_savings(&g_normal_b);
        assert_eq!(state.totals.reclaimable_bytes, expected);
        assert_eq!(state.totals.reclaimable_bytes, 120_000_000);
        assert_eq!(state.totals.duplicates, 3);

        // Now fire ScanFinished — this OVERWRITES self.totals
        // wholesale. The reclaimable_bytes carried on the event
        // must already be inode-aware (engine's responsibility),
        // and after the overwrite the header tile must still
        // show the inode-aware figure.
        state.apply(EngineEvent::ScanFinished {
            at: std::time::Instant::now(),
            total_files: 100,
            total_bytes_read: 5_000_000_000,
            duplicates: 3,
            reclaimable_bytes: expected, // mirroring what live.rs sends
        });
        assert_eq!(state.totals.reclaimable_bytes, expected);
        assert_eq!(state.totals.reclaimable_bytes, 120_000_000);
        // bytes_read must always be >= reclaimable_bytes — the
        // backend's result_self_consistency sanity check requires
        // it, and this is the user-visible header pair.
        assert!(
            state.totals.bytes_read >= state.totals.reclaimable_bytes,
            "bytes_read ({}) must be >= reclaimable_bytes ({})",
            state.totals.bytes_read,
            state.totals.reclaimable_bytes,
        );
        let _ = OverallStage::Idle; // just confirm enum is in scope
    }

    #[test]
    fn largest_per_group_reclaim_is_at_most_total() {
        // The invariant the backend's sanity check enforces:
        // for any set of non-link-equivalent groups, the max of
        // (per-group reclaim) <= sum of (per-group reclaim).
        let groups = [
            mk_group(100, 4, false, 4),  // (4-1)*100 = 300
            mk_group(1000, 3, false, 2), // (2-1)*1000 = 1000  <- biggest
            mk_group(500, 2, false, 2),  // (2-1)*500 = 500
            mk_group(50, 8, true, 1),    // 0 (link_equivalent)
        ];
        let per_group: Vec<u64> = groups.iter().map(inode_aware_savings).collect();
        let total: u64 = per_group.iter().sum();
        let largest = *per_group.iter().max().unwrap();
        assert!(
            largest <= total,
            "largest {largest} must be <= total {total}",
        );
        assert_eq!(total, 1800);
        assert_eq!(largest, 1000);
    }

    /// #39 regression test — when `adopt_resume_state` populates
    /// `state.duplicates` from disk THEN the engine emits
    /// `DuplicateFound` for the same groups (which it does via
    /// `live::run`'s `for g in &prior.previous_duplicates`), the
    /// totals must not double. The dedupe lives in `state.apply`
    /// keyed on `content_hash` so the replay events become no-ops
    /// for already-adopted groups.
    #[test]
    fn duplicate_found_dedupes_on_content_hash_after_resume() {
        use crate::gui::events::EngineEvent;
        let mut state = UiState::default();
        let g_a = mk_group(50_000_000, 3, false, 3);
        let g_b = mk_group(20_000_000, 2, false, 2);

        // Simulate adopt_resume_state's behaviour — push the prior
        // groups directly into state without going through apply,
        // bumping totals + the dedupe index as adopt does.
        for g in [&g_a, &g_b] {
            state.totals.duplicates += 1;
            state.totals.reclaimable_bytes += inode_aware_savings(g);
            state.duplicate_hashes.insert(g.content_hash.clone());
            state.duplicates.push(g.clone());
        }
        assert_eq!(state.totals.duplicates, 2);
        assert_eq!(state.duplicates.len(), 2);

        // Now replay them via DuplicateFound — engine does this on
        // resume to keep the streaming-event surface uniform.
        state.apply(EngineEvent::DuplicateFound(g_a.clone()));
        state.apply(EngineEvent::DuplicateFound(g_b.clone()));

        // Without the fix, totals would now be 4 + reclaimable
        // doubled. With it, the dedupe filters by content_hash:
        assert_eq!(
            state.totals.duplicates, 2,
            "resume-replayed groups must not double-count"
        );
        assert_eq!(
            state.duplicates.len(),
            2,
            "resume-replayed groups must not duplicate the list"
        );
    }

    /// Companion test — fresh-scan emission with distinct
    /// content_hashes must STILL count each group. The dedupe
    /// only kicks in when the same identity arrives twice.
    #[test]
    fn duplicate_found_still_counts_distinct_groups() {
        use crate::gui::events::EngineEvent;
        let mut state = UiState::default();
        let g_a = mk_group(50_000_000, 3, false, 3);
        let g_b = mk_group(20_000_000, 2, false, 2);
        state.apply(EngineEvent::DuplicateFound(g_a));
        state.apply(EngineEvent::DuplicateFound(g_b));
        assert_eq!(state.totals.duplicates, 2);
        assert_eq!(state.duplicates.len(), 2);
    }
}
