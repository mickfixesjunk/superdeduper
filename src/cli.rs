//! Command-line surface. Mirrors the spec's CLI section exactly.
//!
//! Parsing happens here; the dispatch into the engine lives in `main.rs`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

/// Top-level CLI for `superdeduper`.
#[derive(Debug, Parser)]
#[command(
    name = "superdeduper",
    version,
    about = "The fastest duplicate file finder for Windows / NTFS.",
    long_about = None,
)]
pub struct Cli {
    /// Verbosity: -v = info, -vv = debug, -vvv = trace. Default = warn.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Silence all non-error output.
    #[arg(short, long, global = true, conflicts_with = "verbose")]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Scan one or more paths and report duplicate groups (non-destructive).
    Scan(ScanArgs),

    /// Apply destructive actions against a saved scan-results file.
    Dedupe(DedupeArgs),

    /// Inspect or maintain the persistent cache.
    #[command(subcommand)]
    Cache(CacheCommand),

    /// Print storage-device diagnostics for every fixed/external
    /// drive Windows can see. Use this when superdeduper misclassifies
    /// a drive as HDD-vs-SSD: the output shows the raw bus type,
    /// the seek-penalty IOCTL result, and the rule that picked the
    /// final answer. Windows only.
    DriveInfo,

    /// Probe the user's machine + workload and report where their
    /// scans are bound (Tier 1 syscall, Tier 3 IO, hash compute).
    /// Outputs both human-readable text and structured JSON
    /// (when --format json). The GUI preflight ("credit report")
    /// consumes the JSON form.
    Diagnose(DiagnoseArgs),

    /// G-track: register this install for leaderboard participation.
    /// Solves a small CPU proof-of-work (~1s) and POSTs the result
    /// to superdeduper.io. Idempotent — already-registered installs
    /// print "Already registered."
    #[cfg(feature = "telemetry")]
    Register(RegisterArgs),

    /// G-track: print or update the local share preference and
    /// install-state location.
    #[cfg(feature = "telemetry")]
    #[command(subcommand)]
    Config(ConfigCommand),
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    /// One or more paths to scan.
    #[arg(value_name = "PATHS", required = true)]
    pub paths: Vec<PathBuf>,

    /// Mark a path as "reference" — never deleted from during `dedupe`.
    /// May be repeated.
    #[arg(long, value_name = "PATH")]
    pub reference: Vec<PathBuf>,

    /// Skip files smaller than this. Accepts suffixes K/M/G/T.
    #[arg(long, value_name = "BYTES", default_value = "4K")]
    pub min_size: String,

    /// Skip files larger than this.
    #[arg(long, value_name = "BYTES")]
    pub max_size: Option<String>,

    /// Include only paths matching glob. May be repeated.
    #[arg(long, value_name = "GLOB")]
    pub include: Vec<String>,

    /// Exclude paths matching glob. May be repeated.
    #[arg(long, value_name = "GLOB")]
    pub exclude: Vec<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Final byte-by-byte verification before reporting.
    #[arg(long)]
    pub paranoid: bool,

    /// Disable the persistent cache for this run.
    #[arg(long)]
    pub no_cache: bool,

    /// Disable Tier 0 format-aware fingerprints.
    #[arg(long)]
    pub no_format_aware: bool,

    /// Hashing thread count. Defaults to logical CPU count.
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,

    /// Worker count for the hashing par_iter. Defaults to
    /// `threads × 3` because the per-file open()/read()/close()
    /// cycle (Tier 1 + small-file Tier 3) spends most of its time
    /// blocked in syscalls. Oversubscribe to keep more I/O in
    /// flight. Set explicitly to sweep — `--io-threads 1` for a
    /// CPU-only baseline, `--io-threads 64` to find the saturation
    /// point on a fast SSD.
    #[arg(long, value_name = "N")]
    pub io_threads: Option<usize>,

    /// Per-drive I/O queue depth. Defaults to auto (HDD=32, SSD=256).
    #[arg(long, value_name = "N")]
    pub queue_depth: Option<usize>,

    /// Write results to file instead of stdout.
    #[arg(long, short, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Follow reparse points / symbolic links.
    #[arg(long)]
    pub follow_links: bool,

    /// Permit scanning system-critical paths (Windows, Program Files, ...).
    #[arg(long)]
    pub allow_system_paths: bool,

    /// Skip stages 2-4 (size grouping, layout, hashing) and emit only
    /// the placeholder inventory. Use when auditing a tree for
    /// cloud-placeholder presence without paying any tier-1 read cost.
    /// JSON output's `groups[]` is empty; `skipped[]` carries the
    /// per-file placeholder records; `summary.placeholder_skipped`
    /// counts them.
    #[arg(long)]
    pub placeholders_only: bool,

    /// Diagnostic / benchmark mode: bypass size-grouping and the tier
    /// hierarchy entirely. Hashes every file via Tier 3 (full content)
    /// regardless of whether it has a same-size sibling, then reports
    /// throughput. Useful for measuring pure hash + Tier 3 IO
    /// throughput on real corpora where most files have unique sizes
    /// (videos, archives) and would normally never enter Tier 3 under
    /// the standard dup-detection pipeline. JSON output's `groups[]`
    /// is empty in this mode — `summary` carries the throughput
    /// numbers and the diagnostic message lives on stderr.
    #[arg(long)]
    pub force_hash: bool,

    /// Allow the hash worker to read cloud-placeholder files
    /// (`RecallOnOpen` / `RecallOnDataAccess`) even though opening them
    /// triggers cloud hydration. Default OFF — the conservative default
    /// is to skip placeholders so a dedup scan doesn't quietly download
    /// gigabytes from OneDrive/iCloud/SharePoint. Set this when you
    /// explicitly want those files hashed (e.g. you're auditing a
    /// fully-cached OneDrive Files-On-Demand root). Unknown reparse
    /// tags (`OtherReparse`) stay blocked even with this flag — they
    /// might be HSM / PrjFS / other hydration-class.
    #[arg(long)]
    pub allow_recall_on_read: bool,

    /// Content-hash algorithm. `river5` (default, 16-byte,
    /// AES-NI hardware-accelerated, ~3× faster than BLAKE3 on
    /// supported CPUs) or `blake3` (32-byte, cryptographic).
    /// The legacy spellings `ddh128` and `river128` are accepted
    /// as aliases for `river5` so older scripts keep working
    /// after the crate renames.
    #[arg(long, value_enum, default_value_t = HashAlgoArg::River5)]
    pub hash_algo: HashAlgoArg,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum HashAlgoArg {
    Blake3,
    #[clap(alias = "ddh128", alias = "river128")]
    River5,
}

impl From<HashAlgoArg> for crate::pipeline::hash::HashAlgo {
    fn from(v: HashAlgoArg) -> Self {
        match v {
            HashAlgoArg::Blake3 => crate::pipeline::hash::HashAlgo::Blake3,
            HashAlgoArg::River5 => crate::pipeline::hash::HashAlgo::River5,
        }
    }
}

#[derive(Debug, Args)]
pub struct DedupeArgs {
    /// Path to a results file previously produced by `scan`.
    #[arg(value_name = "RESULTS_FILE", required = true)]
    pub results_file: PathBuf,

    /// Which file in each group to keep.
    #[arg(long, value_enum, default_value_t = KeepStrategy::Oldest)]
    pub strategy: KeepStrategy,

    /// What to do with the losers in each group.
    #[arg(long, value_enum, default_value_t = DedupeAction::Recycle)]
    pub action: DedupeAction,

    /// Print what would happen, do nothing.
    #[arg(long)]
    pub dry_run: bool,

    /// Permit destructive operations under system-critical paths.
    #[arg(long)]
    pub allow_system_paths: bool,

    /// Allow destructive actions (recycle, hardlink-replace, etc.)
    /// against files marked with `IO_REPARSE_TAG_DEDUP` (NTFS data
    /// deduplication). Default OFF — the conservative default refuses
    /// any reparse-tagged file. Dedup'd files are safe to act on (the
    /// extents are already FS-shared, so the data is local and intact)
    /// but reclaim ~0 bytes per action since the FS shares them.
    /// Cloud-placeholder states (`RecallOnOpen` / `RecallOnDataAccess`
    /// / `OtherReparse`) remain blocked regardless of this flag — those
    /// guard against cloud-hydration, not FS-dedup transparency.
    #[arg(long)]
    pub allow_destructive_on_deduped: bool,
}

#[derive(Debug, Args)]
pub struct DiagnoseArgs {
    /// Path to probe against. The diagnose probes write a small
    /// temporary scratch directory under this path (or the system
    /// temp dir if not writable), then clean up. If omitted, uses
    /// the system temp directory.
    #[arg(value_name = "PATH")]
    pub path: Option<PathBuf>,

    /// Output format. Default `text` is human-readable; `json` is
    /// the structured form the GUI preflight consumes.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,

    /// Write the diagnostic report to a file instead of stdout.
    #[arg(long, short, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Skip the Tier 3 sequential-read probe. Useful when running
    /// against a remote/slow filesystem where writing a 256 MiB
    /// scratch file would take too long for a "quick check."
    #[arg(long)]
    pub skip_io: bool,
}

#[derive(Debug, Subcommand)]
pub enum CacheCommand {
    /// Show cache statistics.
    Info,
    /// Wipe the cache.
    Clear,
    /// Compact the cache database (VACUUM).
    Vacuum,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum OutputFormat {
    Text,
    Json,
    Csv,
}

#[derive(
    Copy, Clone, Debug, ValueEnum, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default,
)]
pub enum KeepStrategy {
    Oldest,
    Newest,
    ShortestPath,
    LongestPath,
    InReference,
    First,
    Interactive,
    /// Pick the keeper by scoring each file on multiple signals —
    /// path quality (Recycle Bin / temp / cache penalised, depth
    /// rewarded), filename patterns (`_final` rewarded,
    /// `_draft` / `copy of ` / ` (1)` penalised), and mtime. The
    /// highest-scored file is kept; ties resolve to newest.
    /// Reasoning is logged so a user can audit a surprising pick.
    #[default]
    Smart,
}

/// G-track CLI args for `sd register`.
#[cfg(feature = "telemetry")]
#[derive(Debug, Args)]
pub struct RegisterArgs {
    /// Wipe install.json + re-register from scratch. Use only if
    /// the existing install is broken or you've explicitly been
    /// told to. Will invalidate prior submissions linked to the
    /// old install_id.
    #[arg(long)]
    pub reset: bool,

    /// Override the backend URL. Default `https://api.superdeduper.io`.
    #[arg(long, value_name = "URL")]
    pub server_url: Option<String>,
}

/// G-track CLI subcommands for `sd config`.
#[cfg(feature = "telemetry")]
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Print the current share preference, registered install_id,
    /// and install.json path.
    Show,

    /// Set the default share behaviour. `always-ask` (default),
    /// `auto-opt-in`, or `never`.
    SetShare {
        #[arg(value_enum)]
        value: ShareValue,
    },
}

#[cfg(feature = "telemetry")]
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum ShareValue {
    AlwaysAsk,
    AutoOptIn,
    Never,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum DedupeAction {
    /// Permanently remove the file.
    Remove,
    /// Send the file to the Recycle Bin (uses IFileOperation).
    Recycle,
    /// Replace the file with a hardlink to the keeper.
    Hardlink,
    /// Replace the file with a reflink / block-clone (ReFS only).
    Reflink,
    /// Safe-mode: append `.superdeduper` to the filename. Reversible by
    /// running `unsuperdeduper` against the root (no deletion happens).
    SafeRename,
}

/// Parse a human-friendly size string like `"4K"`, `"512M"`, `"2G"`.
///
/// Returns the byte count. `K`, `M`, `G`, `T` are binary multipliers
/// (1 KiB = 1024 B). A bare number is interpreted as bytes.
pub fn parse_size(s: &str) -> crate::Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return Err(crate::Error::BadSize(s.to_string()));
    }
    let (num_part, mult) = match trimmed.as_bytes().last().copied() {
        Some(b'k') | Some(b'K') => (&trimmed[..trimmed.len() - 1], 1024u64),
        Some(b'm') | Some(b'M') => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        Some(b'g') | Some(b'G') => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        Some(b't') | Some(b'T') => (&trimmed[..trimmed.len() - 1], 1024u64.pow(4)),
        Some(b'0'..=b'9') => (trimmed, 1),
        _ => return Err(crate::Error::BadSize(s.to_string())),
    };
    let n: u64 = num_part
        .trim()
        .parse()
        .map_err(|_| crate::Error::BadSize(s.to_string()))?;
    n.checked_mul(mult)
        .ok_or_else(|| crate::Error::BadSize(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_accepts_units() {
        assert_eq!(parse_size("0").unwrap(), 0);
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("4K").unwrap(), 4 * 1024);
        assert_eq!(parse_size("4k").unwrap(), 4 * 1024);
        assert_eq!(parse_size("2M").unwrap(), 2 * 1024 * 1024);
        assert_eq!(parse_size("1G").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("1T").unwrap(), 1024u64.pow(4));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("4X").is_err());
        assert!(parse_size("K").is_err());
    }
}
