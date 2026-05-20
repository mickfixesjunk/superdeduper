//! Self-debugging diagnostics log written per-scan.
//!
//! Every scan opens a fresh `diagnostics/report-<uuid>.txt` next to
//! the working directory and appends structured single-line entries:
//!
//! * Stage transitions with wall-clock elapsed
//! * Drive detection results (HDD/SSD + seek_penalty + model)
//! * Inventory summary stats (no per-file noise)
//! * Hash failure samples (first 50, then total count)
//! * **Periodic engine-state snapshots every 10 seconds** so a remote
//!   agent reading the file can compare engine truth vs. whatever the
//!   user reports seeing in the UI
//! * Final scan summary
//!
//! Format is deliberately grep-friendly:
//!
//! ```text
//!   123ms [SCAN-START] roots=2 settings_min_size=4096
//!   126ms [DRIVE] id=0 path="D:\\..." model="Samsung 980" hdd=false
//!   ...
//!   8543ms [STATE] n=2000 candidates=29996 failed=841 dups=12 bytes_read=18MiB
//! ```

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// Snapshot of the engine's internal counters at a moment in time.
/// The snapshot thread reads this every 10 s and writes a one-line
/// summary to the report. Per-tier counters live alongside the
/// top-line totals so the report shows the funnel narrowing.
#[derive(Default)]
pub struct EngineCounters {
    pub stage: AtomicU64, // OverallStage as u64 for cheap read
    pub files_hashed: Arc<AtomicU64>,
    pub bytes_hashed: Arc<AtomicU64>,
    pub hash_failures: Arc<AtomicU64>,
    pub candidates: AtomicU64,
    pub confirmed_dups: AtomicU64,
    pub reclaimable_bytes: AtomicU64,
    /// Per-tier attempt counts (success+cache+failure). Index 0..=3
    /// correspond to Tier 0 / Tier 1 / Tier 2 / Tier 3.
    pub tier_counts: [Arc<AtomicU64>; 4],
}

pub struct DiagnosticsLog {
    inner: Mutex<DiagnosticsInner>,
    started_at: Instant,
}

struct DiagnosticsInner {
    file: BufWriter<File>,
    failure_samples_emitted: u64,
}

impl DiagnosticsLog {
    /// Create a fresh report file in `diagnostics/` under the working
    /// directory. Failure to create the dir or file is non-fatal —
    /// returns `None` and the engine carries on without diagnostics.
    pub fn open() -> Option<Arc<Self>> {
        let dir = std::env::var_os("SUPERDUPE_DIAGNOSTICS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("diagnostics"));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("diagnostics dir create failed: {e}");
            return None;
        }
        let uuid = quick_uuid();
        let path = dir.join(format!("report-{uuid}.txt"));
        let file = match OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("diagnostics file open failed at {}: {e}", path.display());
                return None;
            }
        };
        let mut bw = BufWriter::new(file);
        // Header so we can identify the run when reading multiple
        // reports later.
        let _ = writeln!(
            bw,
            "# superdupe diagnostics report\n\
             # uuid={uuid}\n\
             # started={}\n\
             # path={}",
            chrono_like_now(),
            path.display(),
        );
        let _ = bw.flush();
        Some(Arc::new(Self {
            inner: Mutex::new(DiagnosticsInner {
                file: bw,
                failure_samples_emitted: 0,
            }),
            started_at: Instant::now(),
        }))
    }

    /// Write a structured entry. `tag` is uppercase-kebab, fields
    /// are key=value pairs (caller supplies an already-built suffix
    /// to keep allocation churn low).
    pub fn log(&self, tag: &str, fields: impl std::fmt::Display) {
        let elapsed = self.started_at.elapsed().as_millis();
        let mut g = self.inner.lock();
        let _ = writeln!(g.file, "{elapsed:>8}ms [{tag}] {fields}");
        let _ = g.file.flush();
    }

    /// Record a hash failure — log the first `SAMPLE_LIMIT` in full
    /// and then drop the rest with a count. Used by live.rs from the
    /// per-file callback so the report doesn't explode on a folder
    /// full of locked / cloud-only files.
    pub fn log_hash_failure(&self, path: &std::path::Path, error: &str) {
        const SAMPLE_LIMIT: u64 = 50;
        let mut g = self.inner.lock();
        let n = g.failure_samples_emitted;
        if n < SAMPLE_LIMIT {
            let elapsed = self.started_at.elapsed().as_millis();
            let _ = writeln!(
                g.file,
                "{elapsed:>8}ms [HASH-FAIL] path={:?} error={error:?}",
                path.display().to_string()
            );
            let _ = g.file.flush();
        } else if n == SAMPLE_LIMIT {
            let elapsed = self.started_at.elapsed().as_millis();
            let _ = writeln!(
                g.file,
                "{elapsed:>8}ms [HASH-FAIL-SUPPRESSED] further per-file entries hidden (see [STATE] failed=… counter)"
            );
            let _ = g.file.flush();
        }
        g.failure_samples_emitted = n + 1;
    }
}

/// Spawn a background thread that polls `counters` every `period`
/// seconds and writes a one-line `[STATE]` snapshot to the log. The
/// thread exits when `stop` flips to `true`.
pub fn spawn_state_sampler(
    log: Arc<DiagnosticsLog>,
    counters: Arc<EngineCounters>,
    period: Duration,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("superdupe-diag-sampler".into())
        .spawn(move || loop {
            // Sleep in small chunks so we can react quickly to `stop`.
            for _ in 0..(period.as_millis() / 100).max(1) {
                if stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let n = counters.files_hashed.load(Ordering::Relaxed);
            let b = counters.bytes_hashed.load(Ordering::Relaxed);
            let f = counters.hash_failures.load(Ordering::Relaxed);
            let cand = counters.candidates.load(Ordering::Relaxed);
            let dups = counters.confirmed_dups.load(Ordering::Relaxed);
            let recl = counters.reclaimable_bytes.load(Ordering::Relaxed);
            let stage = counters.stage.load(Ordering::Relaxed);
            let t0 = counters.tier_counts[0].load(Ordering::Relaxed);
            let t1 = counters.tier_counts[1].load(Ordering::Relaxed);
            let t2 = counters.tier_counts[2].load(Ordering::Relaxed);
            let t3 = counters.tier_counts[3].load(Ordering::Relaxed);
            log.log(
                "STATE",
                format_args!(
                    "stage={stage} n={n} candidates={cand} failed={f} dups={dups} \
                     t0={t0} t1={t1} t2={t2} t3={t3} \
                     bytes_hashed={b} reclaimable={recl}",
                ),
            );
        })
        .expect("spawn diagnostics sampler thread")
}

/// Best-effort UUID built from `(unix_nanos, pid, thread_rng-ish)`.
/// Not RFC-4122 compliant; just unique enough to dedupe report file
/// names within a session.
fn quick_uuid() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    // Cheap mix.
    let mut x = now ^ pid.rotate_left(17);
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    format!("{:016x}-{:08x}", now, x as u32)
}

/// Stand-in for chrono::Utc::now().to_rfc3339() that doesn't require
/// pulling in a chrono dependency. Format matches enough of ISO 8601
/// to be human-readable.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Convert unix seconds → YYYY-MM-DDThh:mm:ssZ via a tiny gmtime.
    // We avoid pulling chrono; this is good enough for a log header.
    let (year, month, day, h, m, s) = unix_to_ymdhms(secs);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Minimal civil-from-days; enough for log headers. From Howard
/// Hinnant's algorithm.
fn unix_to_ymdhms(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let days = (secs / 86_400) as i64;
    let h = ((secs % 86_400) / 3600) as u32;
    let m = ((secs % 3600) / 60) as u32;
    let s = (secs % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (y + if mo <= 2 { 1 } else { 0 }) as i32;
    (year, mo, d, h, m, s)
}
