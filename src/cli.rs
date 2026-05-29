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

    /// Override the server channel for this invocation. One of:
    /// `prod` (default), `dev`, `local`. Higher-precedence than
    /// `SUPERDEDUPER_CHANNEL` ENV var and the persisted config
    /// `[network] channel` setting. Useful for one-off test runs:
    /// `superdeduper dedupe --channel dev …`.
    #[arg(long, value_name = "NAME", global = true)]
    pub channel: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

// `ScanArgs` legitimately carries ~270 bytes of CLI flags + the
// other variants are small; clippy's large_enum_variant lint flags
// the size difference. Boxing the big variant would force every
// dispatcher arm to deref through `Box<ScanArgs>`, and the entire
// Command enum is only ever constructed ONCE per process via
// `Cli::parse()`, so the "stack-copy each variant" perf cost the
// lint warns about doesn't apply. Silenced rather than Box'd.
#[allow(clippy::large_enum_variant)]
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

    /// G-track: list, refresh, or inspect the install's
    /// achievement-grant state. Useful for verifying that a recent
    /// submission actually granted the badges it should have, and
    /// for testdesign-style acceptance tests that shell out + parse
    /// JSON.
    #[cfg(feature = "telemetry")]
    #[command(subcommand)]
    Achievements(AchievementsCommand),

    /// G3: link this install to a Google or Discord account so
    /// achievements roll up across machines + the public profile
    /// can show a display name. Per-channel: linking on prod
    /// doesn't transfer to dev.
    #[cfg(feature = "telemetry")]
    #[command(subcommand)]
    Account(AccountCommand),

    /// #94 — Drain locally-recorded scan-history rows whose
    /// submission_state is `pending` to the leaderboard backend.
    /// Closes the CLI auto-submit gap that the GUI Resubmit button
    /// already covers — `superdeduper scan` records the payload but
    /// doesn't POST it; this subcommand does the POST for every
    /// such row in one shot. Idempotent: already-submitted rows
    /// are skipped (state-driven, not re-POSTed).
    #[cfg(feature = "telemetry")]
    SubmitPending(SubmitPendingArgs),

    /// T-BENCH-ME: run the canonical-bench loop against the leaderboard —
    /// POST /bench/start, download the corpus, run the real dedupe over it,
    /// answer the server's possession challenge (hashing the downloaded
    /// bytes), and submit (scope=canonical-bench). The ranked metric is the
    /// dedupe-only wall; the server verifies the result + challenge directly.
    /// Telemetry-gated.
    #[cfg(feature = "telemetry")]
    BenchMe(BenchMeArgs),

    /// #38 v1 — inspect or maintain the local scan history. Tester
    /// surface; cross-validates the persistence layer without
    /// requiring filesystem spelunking.
    #[command(subcommand)]
    ScanHistory(ScanHistoryCommand),

    /// Debug helpers — read-only state-dump commands the
    /// containment-integration test harness shells out to.
    #[command(subcommand)]
    Debug(DebugCommand),
}

/// #94 — Arguments for `superdeduper submit-pending`.
#[cfg(feature = "telemetry")]
#[derive(Debug, Args)]
pub struct SubmitPendingArgs {
    /// Only drain rows recorded on this channel. Defaults to all
    /// channels — drains prod + dev + local pending rows in one
    /// pass. Use `--channel dev` if you only want to flush dev
    /// records (e.g. CI test runs that shouldn't bleed into the
    /// prod leaderboard).
    #[arg(long, value_name = "CHANNEL")]
    pub channel: Option<String>,

    /// Don't POST anything. Print what would be submitted (scan_id,
    /// channel, recorded reclaim bytes), then exit 0. Useful for
    /// confirming the set before draining.
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,

    /// #109 F25 — Output format. `text` (default) prints the
    /// human-readable per-row + summary stream the GUI mirrors.
    /// `json` emits a single object with the per-row outcomes +
    /// totals, so integration tests + shell scripts don't have
    /// to string-match the human stream. Same format-flag pattern
    /// as `scan`, `dedupe`, `diagnose`, `scan-history list`.
    #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
    pub format: OutputFormat,
}

/// T-BENCH-ME `--bench-me` arguments.
#[cfg(feature = "telemetry")]
#[derive(Debug, Args)]
pub struct BenchMeArgs {
    /// Corpus version to request (default the quick tier).
    #[arg(long, value_name = "VERSION", default_value = "corpus-v1-quick")]
    pub corpus_version: String,
    /// Tier label sent to /bench/start.
    #[arg(long, value_name = "TIER", default_value = "quick")]
    pub tier: String,
    /// Force a fresh corpus download, ignoring (and replacing) the cached copy.
    /// By default the corpus is CACHED per corpus_version and reused across
    /// runs (no re-download of the 100MB+ corpus every time).
    #[arg(long, default_value_t = false)]
    pub fresh: bool,
    /// Working directory for the downloaded corpus. Defaults to the system temp
    /// dir; override to a REAL disk if temp is RAM-backed (e.g. tmpfs `/tmp`),
    /// so the dedupe read is disk-bound + disk-class is detected correctly.
    #[arg(long, value_name = "DIR")]
    pub workdir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum ScanHistoryCommand {
    /// List past scans, newest first. Same content as the GUI
    /// History tab, exposed for integration testing + scripting.
    List {
        /// Output format. `text` (default) is a column-aligned
        /// table; `json` emits a top-level array of records.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
    /// Delete a scan history record by scan_id. Idempotent.
    Delete {
        /// The 32-hex scan_id from `scan-history list`.
        #[arg(value_name = "SCAN_ID")]
        scan_id: String,
    },
    /// #56 — Replay a captured submission payload against its
    /// recorded channel's server URL. Useful for retrying after a
    /// network failure or for headless CI re-submit workflows.
    ///
    /// Pass either a specific `<SCAN_ID>` to resubmit one row, or
    /// `--pending` to drain every row whose `submission_state` is
    /// still `Pending` (in newest-first order). Returns non-zero
    /// when ANY targeted row failed; standard out carries per-row
    /// status lines.
    #[cfg(feature = "telemetry")]
    Resubmit {
        /// 32-hex scan_id from `scan-history list`. Mutually
        /// exclusive with `--pending`.
        #[arg(value_name = "SCAN_ID", conflicts_with = "pending")]
        scan_id: Option<String>,
        /// Resubmit every row whose `submission_state` is currently
        /// `Pending`. Same semantics as `superdeduper submit-pending`
        /// but kept here for surface symmetry — testrunner uses this
        /// to exercise the resubmit-by-row path without driving the
        /// GUI launch modal.
        #[arg(long)]
        pending: bool,
    },
    /// #56 — Prune scan-history records older than a given duration.
    /// Mirrors the GUI Settings → Privacy → "Auto-prune older than"
    /// flow but runs once on demand from the CLI. Useful for
    /// scripted retention enforcement on headless deployments.
    Prune {
        /// Maximum age in days. Rows with `started_at_unix` older
        /// than `<now> - days*86400` are deleted. Pass `0` to make
        /// the call a no-op (the GUI uses 0 as the "forever — never
        /// prune" sentinel).
        #[arg(value_name = "DAYS")]
        days: u32,
    },
}

/// G-track CLI subcommands for `superdeduper account`.
#[cfg(feature = "telemetry")]
#[derive(Debug, Subcommand)]
pub enum AccountCommand {
    /// Open a browser to the chosen OAuth provider, wait for the
    /// loopback callback, and store the resulting token at
    /// `<data_dir>/install/oauth.{channel}.json`. Per spec §10.3
    /// + Mick's 2026-05-24T22:14:51Z directive.
    Link {
        /// Which provider: `google` or `discord`.
        #[arg(value_name = "PROVIDER")]
        provider: String,
        /// Override the OAuth flow timeout. Default 5 minutes —
        /// longer than provider authorization codes usually live.
        #[arg(long, value_name = "SECS", default_value_t = 300)]
        timeout_secs: u64,
    },

    /// Delete the stored OAuth token + tell the backend to revoke
    /// the link. Future scans on this channel revert to the
    /// anonymous install_id identity.
    Unlink,

    /// Print the current account status: Anonymous (UUID) or
    /// Linked (provider + display name + expired-flag).
    Status {
        /// JSON output for scripting; default text is human.
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
    },
}

/// `sd debug …` subcommands. Currently just the snapshot helper;
/// more debug surface may land here as the containment-integration
/// spec expands.
#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// Walk `<path>` recursively + emit the canonical containment-
    /// test snapshot (paths + sizes + inodes + content hashes +
    /// nlinks + ACL hashes + mtimes + reparse-tag metadata). JSON
    /// output matches the schema in
    /// `testdesign/specs/containment-fixtures/_snapshot-schema-examples.md`.
    Snapshot {
        /// Root path to snapshot. Absolute or relative.
        #[arg(value_name = "PATH")]
        path: PathBuf,
        /// Output format. Only `json` supported at v1.
        #[arg(long, value_enum, default_value_t = SnapshotFormat::Json)]
        format: SnapshotFormat,
        /// Write to file instead of stdout. Truncates on each call.
        #[arg(long, value_name = "FILE")]
        out: Option<PathBuf>,
    },

    /// T-BENCH-ME: materialize a canonical-bench corpus tier to disk
    /// (`f{index:010}.bin` files + `manifest.json`) so the reference rig
    /// and the `--bench-me` dev E2E can run a dedupe over the exact
    /// deterministic corpus. The served manifest carries NO root and NO
    /// groundtruth (work-proof invariant). Telemetry-gated.
    #[cfg(feature = "telemetry")]
    MakeBenchCorpus {
        /// Tier: `quick` (~2.5 GB) or `full` (~19 GB).
        #[arg(long, value_enum, default_value_t = BenchTier::Quick)]
        tier: BenchTier,
        /// Output directory (created if absent).
        #[arg(long, value_name = "DIR")]
        out: PathBuf,
        /// REQUIRED 32-byte corpus seed as 64 hex chars. There is NO default —
        /// the corpus seed is private (held server-side) and never hardcoded in
        /// this public source. Clients use `--bench-me` (which DOWNLOADS the
        /// corpus and never needs the seed); this generator tool is dev/server
        /// only. For a perf/throughput pass any 64-hex value works.
        #[arg(long, value_name = "HEX")]
        seed: Option<String>,
        /// Also compute + print the Merkle root (base64) after writing, with
        /// its wall time. Hashes every leaf (the proof-of-work step) — lets a
        /// determinism check confirm same-seed → same root, and times the
        /// proof-hash pass. The root is not-served-not-secret. Off by default
        /// (a plain corpus write doesn't need it).
        #[arg(long)]
        emit_root: bool,
        /// Write the FULL PRIVATE manifest (merkle_root + groundtruth_dupsets +
        /// manifest_hash + M-fields) to this path — the server-side verification
        /// data web keeps private (never served to clients). Requires hashing
        /// every leaf. Use this when generating the canonical corpus server-side.
        #[arg(long, value_name = "FILE")]
        private_manifest: Option<PathBuf>,
    },
}

#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum SnapshotFormat {
    Json,
}

/// T-BENCH-ME corpus tier selector.
#[cfg(feature = "telemetry")]
#[derive(Debug, Copy, Clone, ValueEnum)]
pub enum BenchTier {
    Quick,
    Full,
}

#[derive(Debug, Args)]
pub struct ScanArgs {
    /// One or more paths to scan. Required for actual scans; not
    /// required when `--list-exclusion-packs` is given (which just
    /// prints info and exits).
    #[arg(value_name = "PATHS", required_unless_present = "list_exclusion_packs")]
    pub paths: Vec<PathBuf>,

    /// Mark a path as "reference" — never deleted from during `dedupe`.
    /// May be repeated.
    #[arg(long, value_name = "PATH")]
    pub reference: Vec<PathBuf>,

    /// Skip files smaller than this. Accepts suffixes K/M/G/T.
    #[arg(long, value_name = "BYTES", default_value = "4K")]
    pub min_size: String,

    /// Tier 1 head-read size. Accepts suffixes K/M/G. Default 4K.
    /// Experimental knob — lets bench coord measure whether the
    /// cz-vs-sd small-file perf gap shrinks when sd's Tier 1 read
    /// size matches cz's ~2K partial-hash. Files smaller than this
    /// value short-read to their actual size.
    #[arg(long, value_name = "BYTES", default_value = "4K")]
    pub tier1_bytes: String,

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

    /// Write results to file instead of stdout.
    #[arg(long, short, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Follow reparse points / symbolic links.
    #[arg(long)]
    pub follow_links: bool,

    /// Permit scanning system-critical paths (Windows, Program Files, ...).
    #[arg(long)]
    pub allow_system_paths: bool,

    /// #81 — Master toggle for the safe-defaults exclusion filter
    /// (the preset packs that skip system DLLs, .git internals,
    /// OS-protected paths, and AV signature databases). Default ON
    /// for v0.2.7+. Set `--exclusions off` to disable entirely
    /// (e.g. for a pre-#81 scan-everything baseline).
    #[arg(long, value_enum, value_name = "STATE", default_value_t = ExclusionsToggle::On)]
    pub exclusions: ExclusionsToggle,

    /// #81 — Disable a specific preset pack that would otherwise be
    /// active. Repeatable. Example: `--exclusion-pack-disable
    /// system-libraries` to allow scanning .dll/.sys files even
    /// with the master toggle on. Valid IDs: see
    /// `--list-exclusion-packs` (or the Settings → Exclusions tab).
    #[arg(long = "exclusion-pack-disable", value_name = "ID")]
    pub exclusion_pack_disable: Vec<String>,

    /// #81 — Activate an additional preset pack beyond the safe-
    /// defaults. Repeatable. Example: `--exclusion-pack
    /// package-manager-caches` to also skip .cargo/registry,
    /// .npm/_cacache, etc.
    #[arg(long = "exclusion-pack", value_name = "ID")]
    pub exclusion_pack_enable: Vec<String>,

    /// #81 — Print every preset pack with its content (extension
    /// list + path-pattern list) and exit. Useful when deciding
    /// which packs to enable. Bypasses the scan entirely.
    #[arg(long)]
    pub list_exclusion_packs: bool,

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

    /// Similarity mode. `exact` (default) is byte-identical dedup
    /// — the T0–T3 pipeline. `image` enables Tier-4 perceptual
    /// image grouping (T1.2, #25). `audio` is a placeholder for
    /// T1.3 (#26) — parses today but falls through to exact.
    /// Per Mick directive: single shared dropdown across image +
    /// audio modes.
    #[arg(long, value_enum, default_value_t = ScanMode::Exact)]
    pub mode: ScanMode,

    /// Hamming-distance threshold for `--mode image`. Pairs of
    /// images whose perceptual hashes are within this many bits of
    /// one another group together. Default 5 paired with the default
    /// `--image-hash-algorithm dhash` is the cross-corpus operating
    /// point: dhash's row-pair difference signal stays
    /// discriminative across variant-augmented photo corpora where
    /// phash's DCT-coefficient signal overlaps between same-source
    /// and different-source pairs.
    ///
    /// History: v0.2.11 #87 retuned to phash+τ=10 based on a 27-photo
    /// no-variant retest (the recall lift was real on that subset).
    /// testdesign's 2026-05-26 variant-augmented spec caught 100% AT2
    /// false-positives on phash at every threshold tested
    /// (τ=10 / 7 / 5), so v0.2.13 reverts to the pre-v0.2.11 default.
    /// If your corpus is photo-heavy AND single-variant, the
    /// `--image-hash-algorithm phash --image-similarity-threshold 10`
    /// combination still works — it just doesn't generalize to corpora
    /// with per-source variant fan-out.
    ///
    /// Accepts an integer (e.g. `5`) for a fixed threshold OR the
    /// keyword `auto` for E3 (#78): the engine scales the default
    /// threshold down by `floor(log10(n/100))` based on the count
    /// of decoded image fingerprints (floor at 3). On a 100-image
    /// corpus auto = default (5); 1k = 4; 10k = 3. Reduces the
    /// random-collision FP rate at scale without losing recall on
    /// small corpora. Ship-as-opt-in first; promotion to default
    /// gated on field results.
    #[arg(
        long,
        value_name = "BITS|auto",
        default_value_t = ImageSimilarityThresholdArg::Fixed(5),
    )]
    pub image_similarity_threshold: ImageSimilarityThresholdArg,

    /// Perceptual-hash algorithm used by `--mode image` (#45).
    /// Defaults to `dhash` (row-pair Difference Hash) — cross-corpus
    /// reliable: discriminative on both photo and procedural content,
    /// and resilient to variant-augmented corpora where phash's
    /// DCT-coefficient signal overlaps between same- and
    /// different-source pairs. Paired with the default
    /// `--image-similarity-threshold 5`. `phash` (DCT-based) had a
    /// recall edge on small no-variant photo retests but fails AT2
    /// (false-positive flood) on variant-augmented corpora at every
    /// tested threshold — see history note on
    /// `--image-similarity-threshold`. `ahash` (mean) is cheaper but
    /// less discriminative; useful as a prefilter only.
    #[arg(long, value_enum, default_value_t = ImageHashAlgoArg::Dhash)]
    pub image_hash_algorithm: ImageHashAlgoArg,

    /// Average per-chunk Hamming-distance threshold for `--mode audio`.
    /// The Chromaprint fingerprint is a sequence of 32-bit chunks; we
    /// average the per-chunk bit-flip counts across the alignment
    /// window and group pairs whose average is ≤ this value. Default
    /// 5.0 matches czkawka's calibrated default + the v1 spec §3.
    /// Distinct unit from `--image-similarity-threshold` (which is
    /// a flat 64-bit-hash bit count); the flags are intentionally
    /// separate per testrunner's GH #53.
    #[arg(long, value_name = "BITS", default_value_t = 5.0)]
    pub audio_similarity_threshold: f64,
}

/// #81 — Master toggle for the safe-defaults exclusion filter.
/// Surfaced as `--exclusions on|off`.
#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq, Default)]
#[clap(rename_all = "lower")]
pub enum ExclusionsToggle {
    /// Apply the safe-defaults preset packs (system libraries,
    /// VCS internals, OS system trees, AV signature databases).
    /// Recommended for v0.2.7+ scans on user machines.
    #[default]
    On,
    /// Pre-#81 behaviour: scan every file regardless of class.
    /// Useful for baselining or for scans that intentionally
    /// target system files.
    Off,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum HashAlgoArg {
    Blake3,
    #[clap(alias = "ddh128", alias = "river128")]
    River5,
}

/// CLI shape for the image-perceptual-hash algorithm. Mirrors
/// [`crate::pipeline::image_hash::Algorithm`] but kept separate
/// so the pipeline module doesn't grow a `clap` dependency.
#[derive(Copy, Clone, Debug, ValueEnum, Default, PartialEq, Eq)]
#[clap(rename_all = "lower")]
pub enum ImageHashAlgoArg {
    /// Average hash — cheap, prefilter-only.
    Ahash,
    /// Difference hash — robust on procedural / screenshot content;
    /// pair with `--image-similarity-threshold 5`. This is the default:
    /// `#[default]` is kept index-aligned with the clap
    /// `default_value_t` on `--image-hash-algorithm` so non-clap callers
    /// (e.g. the GUI via `ImageHashAlgoArg::default()`) resolve the same
    /// default as CLI parsing. #127 flipped the clap default to dhash but
    /// left this on phash, diverging the two paths (F-CLI-1).
    #[default]
    Dhash,
    /// Double-gradient hash (slug retained as "phash" for back-compat
    /// with existing scan JSON + cache rows). Despite the historical
    /// name, this is NOT a true DCT pHash — it's `image_hasher`'s
    /// `HashAlg::DoubleGradient`, an extension of dHash that adds a
    /// vertical-gradient pass. Real photo accuracy is good; dHash is
    /// usually competitive at a fraction of the cost. See
    /// `pipeline::image_hash::Algorithm::DoubleGradient` for the full
    /// story.
    Phash,
}

#[cfg(feature = "similar-images")]
impl From<ImageHashAlgoArg> for crate::pipeline::image_hash::Algorithm {
    fn from(v: ImageHashAlgoArg) -> Self {
        match v {
            ImageHashAlgoArg::Ahash => Self::AverageHash,
            ImageHashAlgoArg::Dhash => Self::DifferenceHash,
            ImageHashAlgoArg::Phash => Self::DoubleGradient,
        }
    }
}

/// #78 / E3 — `--image-similarity-threshold` CLI value.
///
/// Accepts either an integer (e.g. `5`) for a fixed threshold OR
/// the keyword `auto`, in which case the engine scales the default
/// threshold (5) down by `floor(log10(n/100))` based on the count
/// of decoded image fingerprints (floored at 3). The auto formula
/// keeps the random-collision FP rate bounded as corpus size grows
/// without losing recall on small corpora.
///
/// Resolution to a concrete `u32` happens inside
/// `pipeline::image_hash::tau_for_n` after Step 1 hashing, when
/// `n` is known. See [`Self::resolve`] for the resolution helper.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ImageSimilarityThresholdArg {
    /// Engine scales τ from corpus size n at scan time.
    Auto,
    /// User-supplied fixed Hamming-distance threshold in bits.
    Fixed(u32),
}

impl ImageSimilarityThresholdArg {
    /// Resolve to a concrete bit count given the corpus size.
    /// `Auto` → `tau_for_n(default_tau, n)`; `Fixed(b)` → `b`.
    #[cfg(feature = "similar-images")]
    pub fn resolve(self, default_tau: u32, n: u64) -> u32 {
        match self {
            Self::Auto => crate::pipeline::image_hash::tau_for_n(default_tau, n),
            Self::Fixed(b) => b,
        }
    }

    /// Non-feature-gated variant — used by callers that can't pull
    /// in the `similar-images` feature but still need to thread the
    /// CLI value through to a downstream consumer.
    pub fn resolve_no_auto(self, default_when_auto: u32) -> u32 {
        match self {
            Self::Auto => default_when_auto,
            Self::Fixed(b) => b,
        }
    }
}

impl std::fmt::Display for ImageSimilarityThresholdArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => write!(f, "auto"),
            Self::Fixed(b) => write!(f, "{b}"),
        }
    }
}

impl std::str::FromStr for ImageSimilarityThresholdArg {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse::<u32>()
            .map(Self::Fixed)
            .map_err(|e| format!("expected an integer (e.g. `5`) or the keyword `auto`: {e}"))
    }
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

    /// Which file in each group to keep. Smart (default) applies a
    /// scored heuristic that prefers files in organised locations
    /// (avoid Recycle Bin / temp / Downloads), prefers files
    /// without "Copy of …" / "(1)" suffix markers, and breaks ties
    /// on the newest mtime. Same picker the GUI uses for its
    /// destructive actions — keeping the CLI default aligned
    /// eliminates the data-loss divergence where
    /// `superdeduper dedupe --action recycle` (no `--strategy`)
    /// could keep a different file than the GUI's "Recycle"
    /// button on the same group. Per Mick approval 2026-05-25
    /// (F1 in design's quality sweep).
    #[arg(long, value_enum, default_value_t = KeepStrategy::Smart)]
    pub strategy: KeepStrategy,

    /// What to do with the losers in each group.
    #[arg(long, value_enum, default_value_t = DedupeAction::Recycle)]
    pub action: DedupeAction,

    /// Similarity mode. `exact` (default) is byte-identical dedup.
    /// `image` runs the Tier-4 perceptual-image pipeline (#25
    /// shipped v0.2.x; engine-side similar-images feature must be
    /// on); `audio` runs Tier-4 acoustic similarity (#26 shipped
    /// v0.2.x; similar-audio feature must be on). When the binary
    /// is built without the relevant feature, non-`exact` modes
    /// log a stderr warning + fall through to exact behaviour. Per
    /// Mick directive: single shared dropdown across image + audio
    /// modes.
    #[arg(long, value_enum, default_value_t = ScanMode::Exact)]
    pub mode: ScanMode,

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

    /// Emit a structured NDJSON action receipt for every action
    /// attempted (one JSON object per line). Receipts carry
    /// pre-/post-action inode IDs, hardlink-count deltas, recycle
    /// bin metadata, and an outcome enum so integration test
    /// harnesses can assert containment ("did this action affect
    /// EXACTLY what it was told to and NOTHING else"). Schema
    /// `superdeduper.action_receipt.v1`; see
    /// `~/sd-bench-local/testdesign/specs/behavior-containment-integration-spec.md` §7.
    ///
    /// Output goes to stdout unless `--receipt-file <path>` is set.
    #[arg(long)]
    pub integration_test_mode: bool,

    /// Redirect `--integration-test-mode` receipts to this file
    /// instead of stdout. Each receipt is one NDJSON line; the
    /// file is opened in append mode + truncated at start of the
    /// run so re-running overwrites prior output.
    #[arg(long, value_name = "PATH", requires = "integration_test_mode")]
    pub receipt_file: Option<PathBuf>,
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
    /// Markdown-friendly one-page summary: total files / bytes,
    /// group count, reclaimable bytes (path-aware + inode-aware),
    /// top-10 largest groups by reclaimable, and a one-liner on
    /// how to apply destructive actions. Aimed at pasting into
    /// issues or chat. fclones-style.
    Report,
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
    /// F-CLI-6 — GUI-only: the GUI duplicate-group table IS the
    /// interactive keeper surface. `#[value(skip)]` keeps the variant
    /// for the GUI while removing it from the CLI `--strategy` choices,
    /// so the CLI no longer advertises (in `--help`) a strategy that
    /// errors at runtime ("interactive is not yet implemented").
    #[value(skip)]
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

/// G-track CLI args for `superdeduper register`.
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

    /// #58 follow-up — print the captcha-setup URL that registration
    /// WOULD open in the browser, then exit 0. Doesn't bind the
    /// loopback server, doesn't open a browser, doesn't run any PoW.
    /// Useful for:
    ///
    /// * testdesign's AT-captcha-* per-channel acceptance tests
    ///   (shell + grep stdout instead of plumbing browser-open
    ///   interception)
    /// * user-side debug ("why is this URL wrong for my channel?")
    ///   without committing to the captcha flow
    ///
    /// Mirrors the existing `--integration-test-mode` /
    /// `--receipt-file` "describe-what-you'd-do" pattern.
    #[arg(long)]
    pub print_captcha_url: bool,
}

/// G-track CLI subcommands for `superdeduper achievements`. Minimum-viable
/// triage surface: `list` + `refetch`. Fuller surface (show, verify,
/// diff, anchor) lands as v0.1.9 per design's plan.
#[cfg(feature = "telemetry")]
#[derive(Debug, Subcommand)]
pub enum AchievementsCommand {
    /// Print the install's granted achievements as a table (default)
    /// or JSON. Reads from the local cache populated by the most
    /// recent fetch. Run `superdeduper achievements refetch` first to ensure
    /// the printout reflects current server state.
    List {
        /// Output format. `text` (default, human) or `json` (machine).
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// Include ungranted entries with their unlock criterion.
        /// Default lists only granted achievements.
        #[arg(long)]
        all: bool,
    },

    /// Force a fresh GET /api/v1/profile/{install_id} and overwrite
    /// the local cache. Use this after a Submit if the GUI's badge
    /// wall is showing stale state.
    Refetch {
        /// Suppress stdout output (returns exit code 0 / 1 only).
        #[arg(long)]
        quiet: bool,
    },

    /// Print an audit of the install's granted achievements
    /// (timestamps + per-row provenance) and bump the local
    /// invocation counter. Each call adds one toward the
    /// `verify-veteran` predicate (grants at 10 invocations).
    Verify {
        /// Output format. `text` (default, human) or `json` (machine).
        #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
        format: OutputFormat,
        /// Suppress stdout output (still bumps the counter; returns
        /// exit code 0 / 1 only).
        #[arg(long)]
        quiet: bool,
    },
}

/// G-track CLI subcommands for `superdeduper config`.
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

/// Scan-mode dropdown per #25 + #26. User picks ONE mode per scan
/// per Mick's 2026-05-24 directive.
///
/// All three variants drive the pipeline today. `Image` requires the
/// `similar-images` cargo feature; `Audio` requires `similar-audio`.
/// Both are on in the shipped release binaries. Source builds without
/// the matching feature hard-error at scan start per the build-gotcha
/// chore (#97 v2 follow-up) rather than silently fall through.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, ValueEnum)]
pub enum ScanMode {
    /// Byte-identical dedup — today's behaviour. Default.
    #[default]
    Exact,
    /// Perceptual image similarity (T1.2, #25). Tier-4 dHash/aHash/pHash
    /// over JPEG / PNG / WebP / GIF / BMP / TIFF / ICO files. Threshold
    /// via `--image-similarity-threshold`; algorithm via
    /// `--image-hash-algorithm`. Requires the `similar-images` feature.
    Image,
    /// Acoustic audio fingerprinting (T1.3, #26). Tier-4 chromaprint
    /// over MP3 / FLAC / WAV / OGG / M4A / AAC files. Threshold via
    /// `--audio-similarity-threshold` (avg Hamming bits/chunk). Files
    /// shorter than chromaprint's ~30s floor process via byte-identical
    /// tier only and are reported in the scan-finish summary. Requires
    /// the `similar-audio` feature.
    Audio,
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

    /// F-CLI-1 guard: the `#[derive(Default)]` variant of
    /// `ImageHashAlgoArg` must equal the clap `default_value_t` on
    /// `--image-hash-algorithm` (see the `#[arg(... default_value_t =
    /// ImageHashAlgoArg::Dhash)]` field). When they diverge, non-clap
    /// callers (the GUI, which uses `ImageHashAlgoArg::default()`) get a
    /// different default than CLI parsing — the F37-class bug #127 left
    /// behind (clap flipped to dhash; the derived Default stayed phash).
    #[test]
    fn image_hash_algo_default_matches_clap_default() {
        assert_eq!(ImageHashAlgoArg::default(), ImageHashAlgoArg::Dhash);
    }

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
