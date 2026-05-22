//! Engine → UI events. One message per "thing that happened" so the UI
//! can replay them into its state model with no branching on the
//! engine's internal mood.

use std::path::PathBuf;
use std::time::Instant;

use serde::{Deserialize, Serialize};

/// Which physical drive an event is about. `0` = the first detected
/// device, etc. The UI never reads beyond this opaque id.
pub type DriveId = u32;

/// Which pipeline stage produced a count update.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Stage {
    Inventory,
    SizeGroup,
    LayoutResolve,
    Tier0Format,
    Tier1Head,
    Tier2HeadMidTail,
    Tier3Full,
    Confirmed,
}

impl Stage {
    pub const ALL: [Stage; 8] = [
        Stage::Inventory,
        Stage::SizeGroup,
        Stage::LayoutResolve,
        Stage::Tier0Format,
        Stage::Tier1Head,
        Stage::Tier2HeadMidTail,
        Stage::Tier3Full,
        Stage::Confirmed,
    ];

    pub fn label(self) -> &'static str {
        // Algo-agnostic label. Use [`label_with_algo`] when you want
        // the Tier 3 line to say "full BLAKE3" / "full DDH-128"
        // instead of just "full hash".
        match self {
            Stage::Inventory => "Inventory",
            Stage::SizeGroup => "Size group",
            Stage::LayoutResolve => "Layout",
            Stage::Tier0Format => "Tier 0 · format",
            Stage::Tier1Head => "Tier 1 · 4 KiB head",
            Stage::Tier2HeadMidTail => "Tier 2 · head+mid+tail",
            Stage::Tier3Full => "Tier 3 · full hash",
            Stage::Confirmed => "Confirmed",
        }
    }

    /// Same as [`label`] but the Tier 3 row carries the currently-
    /// selected content-hash algorithm name. Used by the funnel and
    /// overall progress bar so flipping the algo dropdown updates the
    /// UI label too.
    pub fn label_with_algo(self, algo: crate::pipeline::hash::HashAlgo) -> String {
        use crate::pipeline::hash::HashAlgo;
        match self {
            Stage::Tier3Full => match algo {
                HashAlgo::Blake3 => "Tier 3 · full BLAKE3".to_string(),
                HashAlgo::River5 => "Tier 3 · full RIVER5".to_string(),
            },
            other => other.label().to_string(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DriveInfo {
    pub id: DriveId,
    pub model: String,
    pub has_seek_penalty: bool,
    pub capacity_bytes: u64,
    pub volume_label: String,
    /// Volume GUID, used as the persistent key for HDD/SSD render
    /// overrides — drive letters reshuffle when external drives are
    /// re-plugged, but the GUID is stable across mounts. Empty
    /// string when the engine couldn't determine one (e.g. mount
    /// path lookup failed); the override system treats empty as
    /// "skip persistence for this drive".
    pub volume_guid: String,
}

/// One completed read submitted to the UI for the LCN trace and the
/// throughput sparkline.
#[derive(Copy, Clone, Debug)]
pub struct ReadSample {
    pub drive: DriveId,
    /// Volume-relative byte offset of the read. UI plots this on Y.
    pub lcn_bytes: u64,
    pub bytes: u64,
    /// Microseconds the read spent in-flight (submission → completion).
    pub latency_us: u64,
    pub at: Instant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DuplicateGroupSummary {
    pub size: u64,
    pub content_hash: String,
    pub files: Vec<PathBuf>,
    /// All files in this group share the same NTFS file_ref on the
    /// same volume — i.e. they're hardlinks of each other, not
    /// separately-stored copies. Reclaimable space is 0 because
    /// every "duplicate" is already pointing at the same data on
    /// disk; the GUI badges these distinctly and excludes them from
    /// the global Reclaimable total. `#[serde(default)]` so old
    /// checkpoints (without this field) still load.
    #[serde(default)]
    pub link_equivalent: bool,
}

/// Severity tag for [`EngineEvent::Log`] entries.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Every event flows through this enum.
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// Scan began at this instant. Resets the UI counters.
    ScanStarted { at: Instant, roots: Vec<PathBuf> },
    /// A physical drive was discovered. Sent once per drive.
    DriveDiscovered(DriveInfo),
    /// Per-stage count update. `delta` is the number of files that just
    /// arrived at this stage (use `0` plus a `total` to seed initial
    /// counts).
    StageTick {
        stage: Stage,
        delta: u64,
        total: u64,
    },
    /// A completed read sample for the live drive scope.
    Read(ReadSample),
    /// A confirmed duplicate group ready for the results panel.
    DuplicateFound(DuplicateGroupSummary),
    /// Final wall-clock totals; scan is done.
    ScanFinished {
        at: Instant,
        total_files: u64,
        total_bytes_read: u64,
        duplicates: u64,
        reclaimable_bytes: u64,
    },
    /// Human-readable status line to surface in the header.
    Status(String),
    /// Adds a row to the GUI log panel. Used for things like "scan
    /// finished with 0 results because 4,182 directories were
    /// permission-denied" — the kind of context that turns a silent
    /// failure into an actionable signal.
    Log { level: LogLevel, message: String },
    /// User requested a pause; the engine has flushed checkpoint state
    /// and is now idle. Re-issuing a scan with the same roots resumes.
    ScanPaused { at: Instant, checkpoint_id: String },
    /// Overall scan progress — drives the top-of-window progress bar.
    /// `done` and `total` count whatever unit the current stage uses
    /// (files for inventory, candidate files for hashing). If
    /// `total == 0` the bar renders indeterminate (sweeping).
    OverallProgress {
        stage: OverallStage,
        done: u64,
        total: u64,
        eta_secs: Option<f32>,
    },
    /// A long-running user-requested action (Safe-rename, Archive,
    /// Recycle, Hardlink, Unsuperdeduper, Restore-from-manifest)
    /// has begun on a background thread. The GUI shows a modal with
    /// a spinner + counter for the duration. `total = None` means
    /// the action can't predict its total ahead of time (e.g.
    /// Unsuperdeduper walks the tree looking for `.superdeduper`
    /// markers); the modal then shows "X processed so far" rather
    /// than a determinate progress bar.
    ActionStarted { name: String, total: Option<u64> },
    /// Per-item progress within the active action. `done` is the
    /// running count of processed items; `current` is the path
    /// or short label being worked on right now (shown in the modal
    /// so users see motion).
    ActionProgress {
        done: u64,
        current: Option<String>,
    },
    /// The action thread is done. `summary` becomes the status-line
    /// message. The modal closes the next time `drain_events` runs.
    ActionFinished { summary: String },
}

/// Coarse "what's the engine doing right now" tag. Drives the
/// overall progress bar's label and tint.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverallStage {
    Idle,
    Inventory,
    SizeGroup,
    Layout,
    Hashing,
    Finishing,
}

impl OverallStage {
    pub fn label(self) -> &'static str {
        match self {
            OverallStage::Idle => "Idle",
            OverallStage::Inventory => "Inventory",
            OverallStage::SizeGroup => "Size grouping",
            OverallStage::Layout => "Layout",
            OverallStage::Hashing => "Hashing",
            OverallStage::Finishing => "Finishing",
        }
    }
}
