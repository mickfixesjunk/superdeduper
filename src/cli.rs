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

    /// Disable per-file `GetFileInformationByHandle` lookups in the
    /// fallback walker. With this set, the walker leaves `file_ref`
    /// at 0 and `volume_guid` at `None`, so the Stage-4
    /// `link_equivalent` hardlink detection can't fire. Use for
    /// apples-to-apples benchmarks against tools that don't pay this
    /// cost (e.g. another tool with --allow-hard-links skips inode
    /// resolution entirely). Production scans should leave this OFF.
    #[arg(long)]
    pub no_file_id: bool,

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

#[derive(Copy, Clone, Debug, ValueEnum)]
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
    Smart,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum DedupeAction {
    /// Permanently remove the file.
    Remove,
    /// Send the file to the Recycle Bin (uses SHFileOperationW).
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
