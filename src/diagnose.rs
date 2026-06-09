//! Block P — `superdeduper diagnose` subcommand.
//!
//! Probes the user's machine + a scratch path to answer "where is THIS
//! scan likely to be bound?" Produces both a human-readable text report
//! and a structured JSON form (consumed by the GUI preflight modal —
//! the "credit-report" UX).
//!
//! Probes:
//! * **hash compute throughput** — hashes a cached 256 MiB buffer with
//!   each algorithm. Reports per-thread MB/s. Pure in-memory; no IO.
//! * **Tier 1 syscall throughput** — creates 200 × 4 KiB scratch files
//!   in parallel, then opens + reads + closes them all. Reports files-
//!   per-second. The bottleneck on small-file-dense workloads.
//! * **Tier 3 sequential throughput** — creates 1 × 256 MiB scratch
//!   file, reads it all once. Reports MB/s. Compare against the hash
//!   compute throughput to see whether disk or hash compute is the
//!   ceiling on large files. Skipped with `--skip-io`.
//!
//! Detections:
//! * Defender Real-Time Protection state (Windows).
//! * CPU thread count.
//! * Available RAM (when discoverable).
//! * Hash algorithm impl identity (river5 v15 / blake3 crate).
//!
//! Output:
//! * Text report by default; `--format json` for the structured form.
//! * `--output <file>` writes to file instead of stdout.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;

use crate::cli::DiagnoseArgs;
use crate::pipeline::hash::{algo, HashAlgo};

/// Top-level diagnose report. Schema is the wire format the GUI
/// preflight modal consumes — bumping it is a UI contract change.
#[derive(Debug, Serialize)]
pub struct DiagnoseReport {
    pub schema: &'static str,
    pub timestamp_unix: i64,
    /// All scan-target paths the user is about to scan. Each maps to
    /// exactly one drive in `drives` via [`drive_identifier`].
    pub target_paths: Vec<String>,
    pub system: SystemInfo,
    pub hash: HashProbeResult,
    /// One result per unique drive across the scan targets. Drives
    /// that couldn't be probed (read-only, no writable scratch path)
    /// still appear here so the modal can surface them, but with
    /// `tier1`/`tier3` set to `None`.
    pub drives: Vec<DriveProbeResult>,
    pub defender: DefenderState,
    pub profile: MachineProfile,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Debug, Serialize)]
pub struct DriveProbeResult {
    /// `"D:"`, `"\\\\server\\share"`, or platform-equivalent. Stable
    /// per drive so the modal can dedup if the user has multiple
    /// roots under the same drive.
    pub identifier: String,
    /// The scan-target paths that live on this drive.
    pub paths: Vec<String>,
    /// Where we actually ran the disk probes. `None` ⇒ no writable
    /// scratch location was found anywhere on this drive (read-only).
    pub scratch_path: Option<String>,
    pub tier1: Option<Tier1ProbeResult>,
    pub tier3: Option<Tier3ProbeResult>,
    /// Set when `tier1` / `tier3` are `None` to explain why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// #139 (sdd-testwin 2026-05-30): per-drive disk_class derived via the
    /// same workdir-aware probe the bench-me submission uses. Closes the
    /// parity gap where the bench fingerprint carried a per-volume bus
    /// type but `diagnose --format json` was silent. Schema string is the
    /// HardwareFingerprint enum: NVMe-Gen{3,4,5}|SATA-SSD|HDD|USB-SSD|
    /// USB-HDD|mixed|network. Omitted when the workdir-aware probe falls
    /// back (caller can re-derive from the legacy system-disk default if
    /// needed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_class: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub cpu_threads: usize,
    pub os: String,
    /// Identifier strings for the hash backends, e.g. `"river5-aesni-v15"`.
    pub river5_impl: String,
    pub blake3_impl: String,
    /// #217: CPU brand string as the engine emits it on submission
    /// (post-`normalize_cpu_brand`). Available on builds with the
    /// `telemetry` feature where the hardware probe exists; absent
    /// otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model_string: Option<String>,
    /// #217: bracket id from the vendored cpu-brackets-catalog
    /// snapshot. Render-side maps to the bracket's `display_name`
    /// via the bundled catalog. Telemetry-only (same gate as
    /// `cpu_model_string`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_bracket: Option<String>,
    /// #217: public reference page describing how brackets are
    /// defined. Stable URL. Telemetry-only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_bracket_reference_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct HashProbeResult {
    pub buffer_bytes: u64,
    pub iterations: u32,
    pub river5_aggregate_mbps: f64,
    pub river5_per_thread_mbps: f64,
    pub blake3_aggregate_mbps: f64,
    pub blake3_per_thread_mbps: f64,
    pub river5_single_thread_mbps: f64,
    pub blake3_single_thread_mbps: f64,
}

#[derive(Debug, Serialize)]
pub struct Tier1ProbeResult {
    pub files_count: u32,
    pub bytes_per_file: u64,
    pub wall_ms: u64,
    pub files_per_sec_aggregate: f64,
    pub files_per_sec_per_thread: f64,
}

#[derive(Debug, Serialize)]
pub struct Tier3ProbeResult {
    pub file_bytes: u64,
    pub wall_ms: u64,
    pub aggregate_mbps: f64,
}

#[derive(Debug, Serialize)]
pub struct DefenderState {
    /// Real-Time Protection enabled. `None` if we couldn't detect
    /// (non-Windows, or PowerShell failed).
    pub rtp_enabled: Option<bool>,
    /// Free-form note: how we detected it, what platform, etc.
    pub detection_method: String,
}

/// Categorical machine profile derived from the probe ratios. Used by
/// the recommendations engine and (eventually) the leaderboard's
/// hardware-class bucketing.
#[derive(Debug, Serialize)]
pub enum MachineProfile {
    /// Hash compute heavily exceeds disk read rate. Bottleneck on
    /// most workloads is disk IO, not hash compute.
    FastCpuFastNvme,
    /// Hash compute roughly matches disk read rate. Either can be
    /// the gate depending on workload.
    BalancedCpuDisk,
    /// Disk read rate exceeds aggregate hash compute. Hash compute
    /// is the gate on large-file workloads.
    SlowCpuFastDisk,
    /// Both compute and IO are slow. Wall-clock dominated by both.
    SlowCpuSlowDisk,
    /// IO probe was skipped; can't determine the ratio.
    Indeterminate,
}

#[derive(Debug, Serialize)]
pub struct Recommendation {
    pub impact: RecommendationImpact,
    pub title: String,
    pub detail: String,
    /// If a concrete command/action exists, surface it. Otherwise
    /// `None` and the message is informational only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

#[derive(Debug, Serialize)]
pub enum RecommendationImpact {
    High,
    Medium,
    Low,
    Informational,
}

const HASH_BUFFER_SIZE: usize = 256 * 1024 * 1024; // 256 MiB
const HASH_PROBE_ITERATIONS: u32 = 4;
const TIER1_FILE_COUNT: u32 = 200;
const TIER1_FILE_BYTES: u64 = 4 * 1024;
const TIER3_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// Run all probes against `target_paths` and return the populated
/// report. This is the library-level entry point — used by the CLI
/// subcommand (`run(args)`) and by the GUI preflight modal, which
/// calls this on a background thread and renders the result.
///
/// `target_paths` are deduped by drive identifier (drive letter on
/// Windows, share root for UNC, mount root on Linux) and each unique
/// drive is probed separately. The hash + system + defender probes
/// are machine-wide and run once.
pub fn run_probes(target_paths: Vec<PathBuf>, skip_io: bool) -> anyhow::Result<DiagnoseReport> {
    let target_paths = if target_paths.is_empty() {
        vec![std::env::temp_dir()]
    } else {
        target_paths
    };

    // Per-probe-stage debug log so a future hang is diagnosable from
    // disk without rerunning instrumented builds. Best-effort: if we
    // can't open it, just skip — the probes still run.
    let mut log = PreflightLog::open();
    log.line(
        "preflight-start",
        &format!("targets={}", target_paths.len()),
    );
    for p in &target_paths {
        log.line("target", &p.display().to_string());
    }

    let drive_groups = group_by_drive(&target_paths);
    log.line("drive-groups", &format!("{}", drive_groups.len()));

    let mut drives = Vec::with_capacity(drive_groups.len());
    for group in &drive_groups {
        let started = std::time::Instant::now();
        log.line("drive-probe-start", &group.identifier);
        let r = probe_drive(group, skip_io);
        log.line(
            "drive-probe-end",
            &format!(
                "{} elapsed_ms={} measured={}",
                group.identifier,
                started.elapsed().as_millis(),
                r.tier3.is_some()
            ),
        );
        drives.push(r);
    }

    log.line("system-probe-start", "");
    let system = probe_system();
    log.line("system-probe-end", "");

    log.line("hash-probe-start", "");
    let hash_started = std::time::Instant::now();
    let hash = probe_hash_throughput();
    log.line(
        "hash-probe-end",
        &format!("elapsed_ms={}", hash_started.elapsed().as_millis()),
    );

    log.line("defender-probe-start", "");
    let defender_started = std::time::Instant::now();
    let defender = probe_defender();
    log.line(
        "defender-probe-end",
        &format!(
            "elapsed_ms={} method={:?}",
            defender_started.elapsed().as_millis(),
            defender.detection_method
        ),
    );

    let report = DiagnoseReport {
        schema: "superdeduper.diagnose.v2",
        timestamp_unix: crate::time::now_unix_i64(),
        target_paths: target_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect(),
        system,
        hash,
        drives,
        defender,
        profile: MachineProfile::Indeterminate,
        recommendations: Vec::new(),
    };

    // Two-pass: classify + recommend after probes are in hand so
    // recommendations can reference real numbers.
    let profile = classify_profile(&report);
    let recommendations = build_recommendations(&report, &profile);
    log.line(
        "preflight-end",
        &format!("profile={:?} recs={}", profile, recommendations.len()),
    );
    Ok(DiagnoseReport {
        profile,
        recommendations,
        ..report
    })
}

/// Best-effort plain-text log of pre-flight progress. Lives at
/// `%LOCALAPPDATA%\superdeduper\preflight.log` (or `$XDG_CACHE_HOME/superdeduper/preflight.log`).
/// Append-only across runs so a hang on the Nth probe leaves the
/// N-1 lines that completed visible — useful for diagnosing user
/// reports of "pre-flight hangs on my old Windows 10 machine".
struct PreflightLog {
    file: Option<std::fs::File>,
}

impl PreflightLog {
    fn open() -> Self {
        let file = crate::cache::default_cache_path().ok().and_then(|mut p| {
            p.set_file_name("preflight.log");
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .ok()
        });
        Self { file }
    }
    fn line(&mut self, tag: &str, body: &str) {
        if let Some(f) = self.file.as_mut() {
            use std::io::Write;
            let ts = crate::time::now_unix_i64();
            let _ = writeln!(f, "{ts}  {tag:<20}  {body}");
            let _ = f.flush();
        }
    }
}

/// Internal: one drive's tier1 + tier3 probe under a scratch dir.
/// Drives without a writable scratch location are returned with
/// `tier1`/`tier3` = `None` and an `error` explaining why. The probe
/// never escapes the drive — falling back to system temp would
/// silently measure the system drive, which misleads the user.
fn probe_drive(group: &DriveGroup, skip_io: bool) -> DriveProbeResult {
    let paths_str: Vec<String> = group
        .paths
        .iter()
        .map(|p| p.display().to_string())
        .collect();
    // #139 — fingerprint the drive class for this group's representative
    // path using the same workdir-aware probe bench-me uses. Falls back
    // gracefully to None when the platform-specific lookup can't resolve
    // (caller's existing behavior degrades back to the legacy default).
    //
    // Cfg-gated on `telemetry` because the probe lives in
    // `crate::leaderboard::hardware`, which is itself feature-gated
    // (`#[cfg(feature = "telemetry")] pub mod leaderboard;`). On
    // `--no-default-features` builds the disk_class field stays None
    // and downstream `DriveProbeResult` consumers degrade exactly as
    // they already do when the probe returns `"mixed"` -- no behavior
    // regression on builds that DO opt into telemetry.
    #[cfg(feature = "telemetry")]
    let disk_class: Option<String> = group
        .paths
        .first()
        .map(|p| crate::leaderboard::hardware::detect_with_root_hint(Some(p)).disk_class)
        .filter(|s| s != "mixed");
    #[cfg(not(feature = "telemetry"))]
    let disk_class: Option<String> = None;
    let scratch = find_writable_scratch_on_drive(group);
    let scratch_path = match scratch {
        Some(p) => p,
        None => {
            return DriveProbeResult {
                identifier: group.identifier.clone(),
                paths: paths_str,
                scratch_path: None,
                tier1: None,
                tier3: None,
                error: Some("no writable scratch location on this drive".to_string()),
                disk_class: disk_class.clone(),
            };
        }
    };
    let _guard = ScratchGuard {
        path: scratch_path.clone(),
    };
    let tier1 = match probe_tier1(&scratch_path) {
        Ok(r) => Some(r),
        Err(e) => {
            return DriveProbeResult {
                identifier: group.identifier.clone(),
                paths: paths_str,
                scratch_path: Some(scratch_path.display().to_string()),
                tier1: None,
                tier3: None,
                error: Some(format!("tier1 probe failed: {}", e)),
                disk_class: disk_class.clone(),
            };
        }
    };
    let tier3 = if skip_io {
        None
    } else {
        match probe_tier3(&scratch_path) {
            Ok(r) => Some(r),
            Err(e) => {
                return DriveProbeResult {
                    identifier: group.identifier.clone(),
                    paths: paths_str,
                    scratch_path: Some(scratch_path.display().to_string()),
                    tier1,
                    tier3: None,
                    error: Some(format!("tier3 probe failed: {}", e)),
                    disk_class: disk_class.clone(),
                };
            }
        }
    };
    DriveProbeResult {
        identifier: group.identifier.clone(),
        paths: paths_str,
        scratch_path: Some(scratch_path.display().to_string()),
        tier1,
        tier3,
        error: None,
        disk_class,
    }
}

struct DriveGroup {
    identifier: String,
    paths: Vec<PathBuf>,
    /// Best guess at the volume root for `<drive_root>/.superdeduper-...`
    /// fallback when the per-root scratch is not writable.
    drive_root: PathBuf,
}

fn group_by_drive(paths: &[PathBuf]) -> Vec<DriveGroup> {
    let mut groups: Vec<DriveGroup> = Vec::new();
    for p in paths {
        let (ident, root) = drive_identifier(p);
        if let Some(existing) = groups.iter_mut().find(|g| g.identifier == ident) {
            existing.paths.push(p.clone());
        } else {
            groups.push(DriveGroup {
                identifier: ident,
                paths: vec![p.clone()],
                drive_root: root,
            });
        }
    }
    groups
}

/// Returns `(identifier, drive_root)` for a path. On Windows, the
/// identifier is `"D:"` (drive letter) or `"\\\\server\\share"` (UNC).
/// On Linux, the identifier is the first path component and the
/// drive_root is `/` — Linux mounts aren't really comparable to
/// Windows volumes for sd's purposes.
fn drive_identifier(path: &Path) -> (String, PathBuf) {
    let s = path.to_string_lossy();
    // Strip `\\?\` verbatim prefix if present.
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    // UNC path: \\server\share\...
    if let Some(rest) = s.strip_prefix(r"\\") {
        let mut parts = rest.splitn(3, ['\\', '/']);
        let server = parts.next().unwrap_or("");
        let share = parts.next().unwrap_or("");
        if !server.is_empty() && !share.is_empty() {
            let ident = format!(r"\\{}\{}", server, share);
            let root = PathBuf::from(format!(r"\\{}\{}\", server, share));
            return (ident, root);
        }
    }
    // Drive letter: X:\... or X:/...
    let mut chars = s.chars();
    let first = chars.next();
    let second = chars.next();
    if let (Some(letter), Some(':')) = (first, second) {
        if letter.is_ascii_alphabetic() {
            let upper = letter.to_ascii_uppercase();
            let ident = format!("{}:", upper);
            let root = PathBuf::from(format!("{}:\\", upper));
            return (ident, root);
        }
    }
    // Linux / non-Windows: identify by the first non-root component.
    let comp = path.components().nth(1);
    let ident = match comp {
        Some(c) => format!("/{}", c.as_os_str().to_string_lossy()),
        None => "/".to_string(),
    };
    (ident, PathBuf::from("/"))
}

/// Locate a writable scratch path on the drive identified by `group`.
/// Tries the first scan-target path, then the drive root. Returns
/// `None` if neither location lets us write — meaning the drive is
/// effectively read-only and we should skip the disk probes rather
/// than falling back to a different drive (which would mislead the
/// user about throughput).
fn find_writable_scratch_on_drive(group: &DriveGroup) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(first) = group.paths.first() {
        if first.is_dir() {
            candidates.push(first.join(".superdeduper-diagnose-scratch"));
        }
    }
    candidates.push(group.drive_root.join(".superdeduper-diagnose-scratch"));

    for candidate in candidates {
        if candidate.exists() {
            std::fs::remove_dir_all(&candidate).ok();
        }
        if std::fs::create_dir_all(&candidate).is_ok() {
            let probe = candidate.join(".write-probe");
            if std::fs::write(&probe, b"ok").is_ok() {
                let _ = std::fs::remove_file(&probe);
                return Some(candidate);
            }
            let _ = std::fs::remove_dir_all(&candidate);
        }
    }
    None
}

pub fn run(args: DiagnoseArgs) -> anyhow::Result<()> {
    let target_path = args.path.clone().unwrap_or_else(std::env::temp_dir);
    let report = run_probes(vec![target_path], args.skip_io)?;

    use std::io::Write;
    // #137 — shared writer-dispatch helper (was a hand-rolled stanza here
    // + the file branch at 3 run_scan sites in main.rs). Plain file-or-stdout
    // (diagnose has no quiet-aware console variant).
    let mut writer = crate::output::open_writer(args.output.as_deref()).map_err(|e| {
        let where_to = match &args.output {
            Some(p) => format!("creating {}", p.display()),
            None => "opening stdout writer".to_string(),
        };
        anyhow::anyhow!("{where_to}: {e}")
    })?;
    match args.format {
        crate::cli::OutputFormat::Json => {
            serde_json::to_writer_pretty(&mut writer, &report)
                .map_err(|e| anyhow::anyhow!("serializing report: {}", e))?;
            writeln!(&mut writer)?;
        }
        _ => {
            write_text_report(&mut writer, &report)?;
        }
    }
    writer.flush()?;
    Ok(())
}

struct ScratchGuard {
    path: PathBuf,
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn probe_system() -> SystemInfo {
    // #217: when the telemetry feature is on, surface the same
    // cpu_model_string the bench submission would emit + the bracket
    // it classifies into. Telemetry-off builds skip this — the
    // hardware-fingerprint probe lives behind the same gate.
    #[cfg(feature = "telemetry")]
    let (cpu_model_string, cpu_bracket, cpu_bracket_reference_url) = {
        let brand = crate::leaderboard::hardware::detect().cpu_model_string;
        let bracket = crate::leaderboard::cpu_brackets::classify_cpu(&brand);
        (
            Some(brand),
            Some(bracket.as_str().to_string()),
            Some(BRACKET_REFERENCE_URL.to_string()),
        )
    };
    #[cfg(not(feature = "telemetry"))]
    let (cpu_model_string, cpu_bracket, cpu_bracket_reference_url) = (None, None, None);

    SystemInfo {
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        os: std::env::consts::OS.to_string(),
        river5_impl: river5::impl_name().to_string(),
        blake3_impl: "blake3 (rust crate)".to_string(),
        cpu_model_string,
        cpu_bracket,
        cpu_bracket_reference_url,
    }
}

/// #217 public reference page for bracket definitions. Stable URL.
#[cfg(feature = "telemetry")]
const BRACKET_REFERENCE_URL: &str = "https://superdeduper.io/brackets";

/// #217 resolver: render-side mapping from bracket id (wire) to its
/// display name. Falls back to the id verbatim when the bundled
/// catalog doesn't carry the id (e.g. `"unknown"` — kept as `Unknown`
/// title-cased rather than the bare id). Telemetry-gated like the
/// rest of the bracket plumbing.
#[cfg(feature = "telemetry")]
fn bracket_display_name_resolved(id: &str) -> String {
    if let Some(name) = crate::leaderboard::cpu_brackets::bracket_display_name(id) {
        return name.to_string();
    }
    if id == "unknown" {
        return "Unknown".to_string();
    }
    id.to_string()
}

fn probe_hash_throughput() -> HashProbeResult {
    // One shared buffer of pseudo-random bytes. xorshift-style fill
    // so we don't pull in a real RNG.
    let mut buf = vec![0u8; HASH_BUFFER_SIZE];
    let mut x: u32 = 0x9E37_79B9;
    for chunk in buf.chunks_mut(4) {
        x ^= x.wrapping_shl(13);
        x ^= x.wrapping_shr(17);
        x ^= x.wrapping_shl(5);
        let b = x.to_le_bytes();
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = b[i.min(3)];
        }
    }

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let measure = |algo: HashAlgo| -> (f64, f64) {
        let t = Instant::now();
        let total_bytes = Arc::new(AtomicU64::new(0));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap();
        pool.install(|| {
            (0..threads).into_par_iter().for_each(|_| {
                for _ in 0..HASH_PROBE_ITERATIONS {
                    let _digest = algo::hash_oneshot(algo, &buf);
                    total_bytes.fetch_add(buf.len() as u64, Ordering::Relaxed);
                }
            });
        });
        let elapsed = t.elapsed().as_secs_f64();
        let bytes = total_bytes.load(Ordering::Relaxed) as f64;
        let aggregate_mbps = if elapsed > 0.0 {
            (bytes / elapsed) / 1_048_576.0
        } else {
            0.0
        };
        let per_thread_mbps = aggregate_mbps / threads as f64;
        (aggregate_mbps, per_thread_mbps)
    };

    // Single-thread reference measurement. Tells us the per-stream rate
    // without contention — important for diagnosing workloads where file
    // count < worker count (only a fraction of cores active during
    // Tier 3) versus workloads where memory bandwidth caps aggregate.
    let measure_single = |algo: HashAlgo| -> f64 {
        let t = Instant::now();
        let mut bytes: u64 = 0;
        for _ in 0..HASH_PROBE_ITERATIONS {
            let _digest = algo::hash_oneshot(algo, &buf);
            bytes += buf.len() as u64;
        }
        let elapsed = t.elapsed().as_secs_f64();
        if elapsed > 0.0 {
            (bytes as f64 / elapsed) / 1_048_576.0
        } else {
            0.0
        }
    };

    let (river5_agg, river5_pt) = measure(HashAlgo::River5);
    let (blake3_agg, blake3_pt) = measure(HashAlgo::Blake3);
    let river5_single = measure_single(HashAlgo::River5);
    let blake3_single = measure_single(HashAlgo::Blake3);

    HashProbeResult {
        buffer_bytes: HASH_BUFFER_SIZE as u64,
        iterations: HASH_PROBE_ITERATIONS,
        river5_aggregate_mbps: river5_agg,
        river5_per_thread_mbps: river5_pt,
        blake3_aggregate_mbps: blake3_agg,
        blake3_per_thread_mbps: blake3_pt,
        river5_single_thread_mbps: river5_single,
        blake3_single_thread_mbps: blake3_single,
    }
}

fn probe_tier1(scratch: &Path) -> anyhow::Result<Tier1ProbeResult> {
    // Create N scratch files (sequentially — write isn't what we're
    // probing).
    let pattern: Vec<u8> = (0..TIER1_FILE_BYTES)
        .map(|i| (i as u8).wrapping_mul(31))
        .collect();
    let paths: Vec<PathBuf> = (0..TIER1_FILE_COUNT)
        .map(|i| scratch.join(format!("t1-{:04}.bin", i)))
        .collect();
    for p in &paths {
        std::fs::write(p, &pattern)?;
    }

    // Parallel open + read + close — the throughput we want to measure.
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let t = Instant::now();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap();
    pool.install(|| {
        paths.par_iter().for_each(|p| {
            if let Ok(mut f) = std::fs::File::open(p) {
                let mut buf = vec![0u8; TIER1_FILE_BYTES as usize];
                let _ = std::io::Read::read(&mut f, &mut buf);
            }
        });
    });
    let wall_ms = t.elapsed().as_millis() as u64;
    let elapsed = t.elapsed().as_secs_f64();
    let files_per_sec_aggregate = if elapsed > 0.0 {
        TIER1_FILE_COUNT as f64 / elapsed
    } else {
        0.0
    };
    let files_per_sec_per_thread = files_per_sec_aggregate / threads as f64;
    Ok(Tier1ProbeResult {
        files_count: TIER1_FILE_COUNT,
        bytes_per_file: TIER1_FILE_BYTES,
        wall_ms,
        files_per_sec_aggregate,
        files_per_sec_per_thread,
    })
}

fn probe_tier3(scratch: &Path) -> anyhow::Result<Tier3ProbeResult> {
    let big = scratch.join("t3-big.bin");
    {
        // Write a 256 MiB file with non-trivial content (don't let
        // the FS short-circuit zeroed pages).
        let mut buf = vec![0u8; 1 << 20];
        let mut x: u32 = 0x9E37_79B9;
        for byte in buf.iter_mut() {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            *byte = (x >> 24) as u8;
        }
        let mut f = std::fs::File::create(&big)?;
        let stamps = TIER3_FILE_BYTES / buf.len() as u64;
        for _ in 0..stamps {
            std::io::Write::write_all(&mut f, &buf)?;
        }
        std::io::Write::flush(&mut f)?;
        // Commit pages to the device. Without this, the OS holds the
        // file in writeback cache and the subsequent read (even with
        // FILE_FLAG_NO_BUFFERING on Windows) may not reflect what the
        // drive can actually deliver from media.
        f.sync_all()?;
    }
    let (total, elapsed) = read_for_disk_throughput(&big)?;
    let wall_ms = (elapsed * 1000.0) as u64;
    let aggregate_mbps = if elapsed > 0.0 {
        (total as f64 / elapsed) / 1_048_576.0
    } else {
        0.0
    };
    Ok(Tier3ProbeResult {
        file_bytes: total,
        wall_ms,
        aggregate_mbps,
    })
}

/// Read the entire file and return `(bytes_read, elapsed_secs)`.
/// On Windows, opens with `FILE_FLAG_NO_BUFFERING` so the read
/// **bypasses the OS page cache** and goes to the underlying
/// device — without this, the just-written file sits in RAM and the
/// "read" measures memory bandwidth, making SSDs and HDDs look
/// identical (or worse, ranking them backwards due to noise).
#[cfg(windows)]
fn read_for_disk_throughput(path: &Path) -> anyhow::Result<(u64, f64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN,
        FILE_GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
    };

    // 4 KiB is the standard NTFS sector size on modern volumes. We
    // also require buffer + length to be a multiple of this; 1 MiB
    // chunks (262144 × 4 KiB) and a 256 MiB total file satisfy both.
    const SECTOR: usize = 4096;
    const CHUNK: usize = 1 << 20;

    // Build a sector-aligned 1 MiB slice. Vec allocation is at least
    // word-aligned but not sector-aligned in general; reserve extra
    // space and slice into the aligned region.
    let mut storage = vec![0u8; CHUNK + SECTOR];
    let base = storage.as_ptr() as usize;
    let pad = (SECTOR - (base % SECTOR)) % SECTOR;
    let buf = &mut storage[pad..pad + CHUNK];

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is null-terminated and outlives the call. The
    // flag combination is documented as compatible with synchronous
    // ReadFile.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
            HANDLE::default(),
        )
        .map_err(|e| anyhow::anyhow!("CreateFileW({}): {e}", path.display()))?
    };
    // Tiny RAII so we close on every return path.
    struct Closer(HANDLE);
    impl Drop for Closer {
        fn drop(&mut self) {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
    let _guard = Closer(handle);

    let t = Instant::now();
    let mut total = 0u64;
    loop {
        let mut read_bytes: u32 = 0;
        // SAFETY: `buf` is sector-aligned, length is a sector
        // multiple, handle is open. `read_bytes` is a stable u32.
        unsafe {
            ReadFile(handle, Some(buf), Some(&mut read_bytes as *mut u32), None)
                .map_err(|e| anyhow::anyhow!("ReadFile: {e}"))?;
        }
        if read_bytes == 0 {
            break;
        }
        total += read_bytes as u64;
    }
    Ok((total, t.elapsed().as_secs_f64()))
}

#[cfg(not(windows))]
fn read_for_disk_throughput(path: &Path) -> anyhow::Result<(u64, f64)> {
    // No portable equivalent of FILE_FLAG_NO_BUFFERING. This branch
    // is only exercised by Linux dev builds and the measurement is
    // approximate — Linux production isn't a target platform.
    let t = Instant::now();
    let mut f = std::fs::File::open(path)?;
    let mut total = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    Ok((total, t.elapsed().as_secs_f64()))
}

pub fn probe_defender() -> DefenderState {
    #[cfg(windows)]
    {
        // Shell out to PowerShell Get-MpComputerStatus and parse
        // RealTimeProtectionEnabled.
        //
        // CREATE_NO_WINDOW (0x08000000) suppresses the PowerShell
        // console window that would otherwise flash in front of the
        // GUI on every pre-flight.
        //
        // Hard timeout: on older Windows 10 builds where the Defender
        // service is unresponsive (or Get-MpComputerStatus is missing
        // entirely), the cmdlet hangs forever and the pre-flight
        // modal blocks on it. Spawn + watchdog-kill at 5s — better
        // to lose the Defender signal than hang the whole probe.
        use std::os::windows::process::CommandExt;
        use std::time::Duration;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

        let mut child = match std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-MpComputerStatus).RealTimeProtectionEnabled",
            ])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                return DefenderState {
                    rtp_enabled: None,
                    detection_method: "powershell.exe could not be spawned".to_string(),
                };
            }
        };

        let started = std::time::Instant::now();
        // Poll try_wait every 50ms; bail at PROBE_TIMEOUT.
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    return match child.wait_with_output() {
                        Ok(o) if status.success() => {
                            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                            let rtp = match s.as_str() {
                                "True" => Some(true),
                                "False" => Some(false),
                                _ => None,
                            };
                            DefenderState {
                                rtp_enabled: rtp,
                                detection_method: "powershell Get-MpComputerStatus".to_string(),
                            }
                        }
                        _ => DefenderState {
                            rtp_enabled: None,
                            detection_method: "powershell Get-MpComputerStatus FAILED".to_string(),
                        },
                    };
                }
                Ok(None) => {
                    if started.elapsed() >= PROBE_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        return DefenderState {
                            rtp_enabled: None,
                            detection_method: format!(
                                "powershell Get-MpComputerStatus timed out after {}s — \
                                 Defender service may be unresponsive on this Windows build",
                                PROBE_TIMEOUT.as_secs()
                            ),
                        };
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => {
                    return DefenderState {
                        rtp_enabled: None,
                        detection_method: "powershell try_wait failed".to_string(),
                    };
                }
            }
        }
    }
    #[cfg(not(windows))]
    {
        DefenderState {
            rtp_enabled: None,
            detection_method: "non-Windows; no Defender concept".to_string(),
        }
    }
}

fn classify_profile(r: &DiagnoseReport) -> MachineProfile {
    // Use the average disk read rate across measured drives. Drives
    // with no tier3 result (read-only / skip-io) are excluded — they
    // didn't measure the IO subsystem and shouldn't drag the average.
    let measured: Vec<f64> = r
        .drives
        .iter()
        .filter_map(|d| d.tier3.as_ref().map(|t| t.aggregate_mbps))
        .collect();
    if measured.is_empty() {
        return MachineProfile::Indeterminate;
    }
    let disk_agg = measured.iter().sum::<f64>() / measured.len() as f64;
    let hash_agg = r
        .hash
        .river5_aggregate_mbps
        .max(r.hash.blake3_aggregate_mbps);
    if hash_agg > 4.0 * disk_agg {
        MachineProfile::FastCpuFastNvme
    } else if hash_agg > 1.5 * disk_agg {
        MachineProfile::BalancedCpuDisk
    } else if hash_agg > 0.5 * disk_agg {
        MachineProfile::SlowCpuFastDisk
    } else {
        MachineProfile::SlowCpuSlowDisk
    }
}

fn build_recommendations(r: &DiagnoseReport, p: &MachineProfile) -> Vec<Recommendation> {
    let mut out = Vec::new();
    if r.defender.rtp_enabled == Some(true) {
        out.push(Recommendation {
            impact: RecommendationImpact::High,
            title: "Windows Defender Real-Time Protection is enabled".into(),
            detail: "Defender scans each file the engine opens, adding 50–200 ms per open. \
                     Disabling RTP for the scan window can speed up scans 2–3x. \
                     Only do this if you trust the corpus you're scanning."
                .into(),
            action: Some(
                "Set-MpPreference -DisableRealtimeMonitoring $true (admin) — restore after".into(),
            ),
        });
    }
    match p {
        MachineProfile::SlowCpuFastDisk => {
            out.push(Recommendation {
                impact: RecommendationImpact::Medium,
                title: "Hash compute may bottleneck large-file scans".into(),
                detail: "Your CPU's aggregate hash throughput is close to your disk's read rate. \
                         On corpora with many large files, hash compute will be the gate. \
                         Consider --hash-algo river5 (default) over blake3 for ~2x faster compute."
                    .into(),
                action: None,
            });
        }
        MachineProfile::FastCpuFastNvme => {
            out.push(Recommendation {
                impact: RecommendationImpact::Informational,
                title: "You're disk-bound, not hash-bound".into(),
                detail: "Your CPU can hash much faster than the disk can supply bytes. \
                         Hash algorithm choice has no measurable effect on wall-clock. \
                         Scan-speed gains will come from reducing IO (cache, fewer reads) \
                         or moving to a faster disk."
                    .into(),
                action: None,
            });
        }
        _ => {}
    }
    // Tier 1 throughput sanity — if any measured drive is really low,
    // raise the alert (with the offending drive identifier so the
    // user knows which volume to investigate).
    for d in &r.drives {
        if let Some(t1) = &d.tier1 {
            if t1.files_per_sec_per_thread < 500.0 {
                out.push(Recommendation {
                    impact: RecommendationImpact::Medium,
                    title: format!("Small-file open throughput is low on {}", d.identifier),
                    detail: format!(
                        "Tier 1 syscall throughput on {} is {:.0} files/sec/thread. On \
                         small-file-dense corpora (browser caches, source repos) this will \
                         be the gate. Check for AV scanning overhead beyond Defender, or \
                         storage stack issues on that volume.",
                        d.identifier, t1.files_per_sec_per_thread
                    ),
                    action: None,
                });
            }
        }
    }
    // Read-only drives: surface them so the user knows the disk
    // measurement is partial.
    let unmeasured: Vec<&str> = r
        .drives
        .iter()
        .filter(|d| d.tier3.is_none() && d.error.is_some())
        .map(|d| d.identifier.as_str())
        .collect();
    if !unmeasured.is_empty() {
        out.push(Recommendation {
            impact: RecommendationImpact::Informational,
            title: format!("{} drive(s) could not be measured", unmeasured.len()),
            detail: format!(
                "Skipped disk probes on: {}. The drive(s) are read-only or refused \
                 our scratch directory. Your scan will still work — superdeduper only \
                 needs to read these — but the disk score above doesn't include them.",
                unmeasured.join(", ")
            ),
            action: None,
        });
    }
    out
}

fn write_text_report(out: &mut dyn std::io::Write, r: &DiagnoseReport) -> anyhow::Result<()> {
    writeln!(out, "== superdeduper diagnose ==")?;
    writeln!(out, "Targets:")?;
    for p in &r.target_paths {
        writeln!(out, "  {}", p)?;
    }
    writeln!(out, "Schema:        {}", r.schema)?;
    writeln!(out)?;
    writeln!(out, "System:")?;
    writeln!(out, "  OS:          {}", r.system.os)?;
    writeln!(out, "  Threads:     {}", r.system.cpu_threads)?;
    writeln!(out, "  river5 impl: {}", r.system.river5_impl)?;
    writeln!(out, "  blake3 impl: {}", r.system.blake3_impl)?;
    writeln!(out)?;
    // #217: surface CPU model + leaderboard bracket. Only when the
    // telemetry feature is on (the hardware probe + bracket catalog
    // both live behind the same gate). Bracket id is mapped to its
    // catalog display_name via the vendored cpu-brackets snapshot.
    #[cfg(feature = "telemetry")]
    if let Some(model) = &r.system.cpu_model_string {
        writeln!(out, "Hardware (leaderboard bracket):")?;
        writeln!(out, "  CPU model:   {}", model)?;
        if let Some(bracket_id) = &r.system.cpu_bracket {
            let display = bracket_display_name_resolved(bracket_id);
            writeln!(out, "  Bracket:     {}", display)?;
        }
        if let Some(url) = &r.system.cpu_bracket_reference_url {
            writeln!(out, "  Reference:   {}", url)?;
        }
        writeln!(out)?;
    }
    writeln!(out, "Hash compute throughput (in-memory):")?;
    writeln!(
        out,
        "  river5:      {:>10.0} MB/s aggregate  ({:>7.0} MB/s/thread)  ({:>7.0} MB/s single-thread)",
        r.hash.river5_aggregate_mbps,
        r.hash.river5_per_thread_mbps,
        r.hash.river5_single_thread_mbps
    )?;
    writeln!(
        out,
        "  blake3:      {:>10.0} MB/s aggregate  ({:>7.0} MB/s/thread)  ({:>7.0} MB/s single-thread)",
        r.hash.blake3_aggregate_mbps,
        r.hash.blake3_per_thread_mbps,
        r.hash.blake3_single_thread_mbps
    )?;
    writeln!(out)?;
    writeln!(out, "Per-drive disk throughput:")?;
    for d in &r.drives {
        writeln!(out, "  {} ({} root(s))", d.identifier, d.paths.len())?;
        match (&d.tier1, &d.tier3, &d.error) {
            (Some(t1), Some(t3), _) => {
                writeln!(
                    out,
                    "    Tier 1:    {} × {} B in {} ms ({:.0} files/sec aggregate, {:.0}/thread)",
                    t1.files_count,
                    t1.bytes_per_file,
                    t1.wall_ms,
                    t1.files_per_sec_aggregate,
                    t1.files_per_sec_per_thread,
                )?;
                writeln!(
                    out,
                    "    Tier 3:    {} bytes in {} ms ({:.0} MB/s)",
                    t3.file_bytes, t3.wall_ms, t3.aggregate_mbps,
                )?;
            }
            (Some(t1), None, _) => {
                writeln!(
                    out,
                    "    Tier 1:    {:.0} files/sec/thread",
                    t1.files_per_sec_per_thread
                )?;
                writeln!(out, "    Tier 3:    skipped")?;
            }
            (None, _, Some(err)) => {
                writeln!(out, "    NOT MEASURED: {}", err)?;
            }
            _ => {
                writeln!(out, "    NOT MEASURED")?;
            }
        }
    }
    writeln!(out)?;
    writeln!(out, "Defender state:")?;
    match r.defender.rtp_enabled {
        Some(true) => writeln!(
            out,
            "  RTP:         ENABLED  ({})",
            r.defender.detection_method
        )?,
        Some(false) => writeln!(
            out,
            "  RTP:         disabled ({})",
            r.defender.detection_method
        )?,
        None => writeln!(
            out,
            "  RTP:         unknown  ({})",
            r.defender.detection_method
        )?,
    }
    writeln!(out)?;
    writeln!(out, "Profile: {:?}", r.profile)?;
    writeln!(out)?;
    if !r.recommendations.is_empty() {
        writeln!(out, "Recommendations:")?;
        for rec in &r.recommendations {
            writeln!(out, "  [{:?}] {}", rec.impact, rec.title)?;
            writeln!(out, "    {}", rec.detail)?;
            if let Some(action) = &rec.action {
                writeln!(out, "    Action: {}", action)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_recognized() {
        let (id, root) = drive_identifier(Path::new(r"D:\Studio\Projects"));
        assert_eq!(id, "D:");
        assert_eq!(root, PathBuf::from(r"D:\"));
    }

    #[test]
    fn drive_letter_lowercase_canonicalised() {
        let (id, _) = drive_identifier(Path::new(r"c:\Users\NeoMatrix"));
        assert_eq!(id, "C:");
    }

    #[test]
    fn drive_letter_with_forward_slashes() {
        let (id, _) = drive_identifier(Path::new("E:/foo/bar"));
        assert_eq!(id, "E:");
    }

    #[test]
    fn verbatim_prefix_stripped() {
        let (id, _) = drive_identifier(Path::new(r"\\?\D:\Studio"));
        assert_eq!(id, "D:");
    }

    #[test]
    fn unc_share_identified() {
        let (id, root) = drive_identifier(Path::new(r"\\fileserver\public\dir\sub"));
        assert_eq!(id, r"\\fileserver\public");
        assert_eq!(root, PathBuf::from(r"\\fileserver\public\"));
    }

    #[test]
    fn group_by_drive_dedups() {
        let groups = group_by_drive(&[
            PathBuf::from(r"D:\foo"),
            PathBuf::from(r"E:\bar"),
            PathBuf::from(r"D:\baz"),
        ]);
        assert_eq!(groups.len(), 2);
        let d = groups.iter().find(|g| g.identifier == "D:").unwrap();
        assert_eq!(d.paths.len(), 2);
        assert!(d.paths.iter().any(|p| p == &PathBuf::from(r"D:\foo")));
        assert!(d.paths.iter().any(|p| p == &PathBuf::from(r"D:\baz")));
    }
}
