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
    pub target_path: String,
    pub system: SystemInfo,
    pub hash: HashProbeResult,
    pub tier1: Tier1ProbeResult,
    pub tier3: Option<Tier3ProbeResult>,
    pub defender: DefenderState,
    pub profile: MachineProfile,
    pub recommendations: Vec<Recommendation>,
}

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub cpu_threads: usize,
    pub os: String,
    /// Identifier strings for the hash backends, e.g. `"river5-aesni-v15"`.
    pub river5_impl: String,
    pub blake3_impl: String,
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

/// Run all probes against `target_path` and return the populated report.
/// This is the library-level entry point — used by the CLI subcommand
/// (`run(args)`) and by the GUI preflight modal, which calls this on a
/// background thread and renders the result.
pub fn run_probes(target_path: PathBuf, skip_io: bool) -> anyhow::Result<DiagnoseReport> {
    let scratch_root = ensure_scratch_dir(&target_path)?;
    let _scratch_guard = ScratchGuard {
        path: scratch_root.clone(),
    };

    let report = DiagnoseReport {
        schema: "superdeduper.diagnose.v1",
        timestamp_unix: now_unix(),
        target_path: target_path.display().to_string(),
        system: probe_system(),
        hash: probe_hash_throughput(),
        tier1: probe_tier1(&scratch_root)?,
        tier3: if skip_io {
            None
        } else {
            Some(probe_tier3(&scratch_root)?)
        },
        defender: probe_defender(),
        profile: MachineProfile::Indeterminate,
        recommendations: Vec::new(),
    };

    // Two-pass: classify + recommend after probes are in hand so
    // recommendations can reference real numbers.
    let profile = classify_profile(&report);
    let recommendations = build_recommendations(&report, &profile);
    Ok(DiagnoseReport {
        profile,
        recommendations,
        ..report
    })
}

pub fn run(args: DiagnoseArgs) -> anyhow::Result<()> {
    let target_path = args
        .path
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let report = run_probes(target_path, args.skip_io)?;

    use std::io::Write;
    let mut writer: Box<dyn Write> = match &args.output {
        Some(p) => Box::new(std::io::BufWriter::new(
            std::fs::File::create(p)
                .map_err(|e| anyhow::anyhow!("creating {}: {}", p.display(), e))?,
        )),
        None => Box::new(std::io::BufWriter::new(std::io::stdout().lock())),
    };
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

fn ensure_scratch_dir(under: &Path) -> anyhow::Result<PathBuf> {
    let candidate = if under.is_dir() {
        under.join(".superdeduper-diagnose-scratch")
    } else {
        std::env::temp_dir().join("superdeduper-diagnose-scratch")
    };
    if candidate.exists() {
        std::fs::remove_dir_all(&candidate).ok();
    }
    std::fs::create_dir_all(&candidate)
        .map_err(|e| anyhow::anyhow!("creating scratch dir {}: {}", candidate.display(), e))?;
    Ok(candidate)
}

struct ScratchGuard {
    path: PathBuf,
}

impl Drop for ScratchGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn probe_system() -> SystemInfo {
    SystemInfo {
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        os: std::env::consts::OS.to_string(),
        river5_impl: river5::impl_name().to_string(),
        blake3_impl: "blake3 (rust crate)".to_string(),
    }
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
    let pattern: Vec<u8> = (0..TIER1_FILE_BYTES).map(|i| (i as u8).wrapping_mul(31)).collect();
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
        let mut buf = vec![0u8; 1 << 20]; // 1 MiB stamp
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
    }
    // Read it sequentially.
    let t = Instant::now();
    let mut f = std::fs::File::open(&big)?;
    let mut total = 0u64;
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    drop(f);
    let wall_ms = t.elapsed().as_millis() as u64;
    let elapsed = t.elapsed().as_secs_f64();
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

fn probe_defender() -> DefenderState {
    #[cfg(windows)]
    {
        // Shell out to PowerShell Get-MpComputerStatus and parse
        // RealTimeProtectionEnabled. Cheap (~150 ms) and avoids the
        // WMI binding plumbing.
        let output = std::process::Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-Command",
                "(Get-MpComputerStatus).RealTimeProtectionEnabled",
            ])
            .output();
        match output {
            Ok(o) if o.status.success() => {
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
    let Some(tier3) = &r.tier3 else {
        return MachineProfile::Indeterminate;
    };
    // Compare aggregate disk read rate vs aggregate hash compute rate.
    // The faster hash backend sets the upper-bound on what we could
    // sustain if disk weren't the gate; the disk rate is what we
    // actually observed.
    let hash_agg = r
        .hash
        .river5_aggregate_mbps
        .max(r.hash.blake3_aggregate_mbps);
    let disk_agg = tier3.aggregate_mbps;
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
                "Set-MpPreference -DisableRealtimeMonitoring $true (admin) — restore after"
                    .into(),
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
    // Tier 1 throughput sanity — if it's really low, raise the alert.
    if r.tier1.files_per_sec_per_thread < 500.0 {
        out.push(Recommendation {
            impact: RecommendationImpact::Medium,
            title: "Small-file open throughput is low".into(),
            detail: format!(
                "Tier 1 syscall throughput is {:.0} files/sec/thread. On small-file-dense \
                 corpora (browser caches, source repos) this will be the gate. \
                 Check for AV scanning overhead beyond Defender, or storage stack issues.",
                r.tier1.files_per_sec_per_thread
            ),
            action: None,
        });
    }
    out
}

fn write_text_report(
    out: &mut dyn std::io::Write,
    r: &DiagnoseReport,
) -> anyhow::Result<()> {
    writeln!(out, "== superdeduper diagnose ==")?;
    writeln!(out, "Target:        {}", r.target_path)?;
    writeln!(out, "Schema:        {}", r.schema)?;
    writeln!(out)?;
    writeln!(out, "System:")?;
    writeln!(out, "  OS:          {}", r.system.os)?;
    writeln!(out, "  Threads:     {}", r.system.cpu_threads)?;
    writeln!(out, "  river5 impl: {}", r.system.river5_impl)?;
    writeln!(out, "  blake3 impl: {}", r.system.blake3_impl)?;
    writeln!(out)?;
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
    writeln!(out, "Tier 1 syscall throughput:")?;
    writeln!(
        out,
        "  {} × {} B files in {} ms ({:.0} files/sec aggregate, {:.0}/thread)",
        r.tier1.files_count,
        r.tier1.bytes_per_file,
        r.tier1.wall_ms,
        r.tier1.files_per_sec_aggregate,
        r.tier1.files_per_sec_per_thread,
    )?;
    writeln!(out)?;
    match &r.tier3 {
        Some(t3) => {
            writeln!(out, "Tier 3 sequential read throughput:")?;
            writeln!(
                out,
                "  {} bytes in {} ms ({:.0} MB/s)",
                t3.file_bytes, t3.wall_ms, t3.aggregate_mbps,
            )?;
        }
        None => writeln!(out, "Tier 3: skipped (--skip-io)")?,
    }
    writeln!(out)?;
    writeln!(out, "Defender state:")?;
    match r.defender.rtp_enabled {
        Some(true) => writeln!(out, "  RTP:         ENABLED  ({})", r.defender.detection_method)?,
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
