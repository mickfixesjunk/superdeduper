//! Engine → UI events. One message per "thing that happened" so the UI
//! can replay them into its state model with no branching on the
//! engine's internal mood.

use std::path::PathBuf;
use std::time::Instant;

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
        match self {
            Stage::Inventory => "Inventory",
            Stage::SizeGroup => "Size group",
            Stage::LayoutResolve => "Layout",
            Stage::Tier0Format => "Tier 0 · format",
            Stage::Tier1Head => "Tier 1 · 4 KiB head",
            Stage::Tier2HeadMidTail => "Tier 2 · head+mid+tail",
            Stage::Tier3Full => "Tier 3 · full BLAKE3",
            Stage::Confirmed => "Confirmed",
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

#[derive(Clone, Debug)]
pub struct DuplicateGroupSummary {
    pub size: u64,
    pub content_hash: String,
    pub files: Vec<PathBuf>,
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
}

