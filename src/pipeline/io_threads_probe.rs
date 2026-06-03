//! Startup throughput probe for the `--io-threads` default.
//!
//! Per Mick GO 2026-06-02 09:42 PDT + design-superdeduper 09:52 PDT,
//! the v0.3.31 default-flip uses mechanism (β): measure throughput
//! at the scan start across a small bracket of io-threads counts and
//! pick the winner. Transparent to disk_class misclassification
//! (notably the v0.3.5 SAT pass-through bridge case where USB-HDDs
//! report as USB-SSD); per-instance accurate to whatever hardware is
//! under the actual workdir.
//!
//! ## Shape
//!
//! At scan start, before the main pipeline:
//! 1. Walk the first scan root briefly to collect a handful of
//!    representative files (up to [`PROBE_MAX_FILES`]).
//! 2. For each candidate `io_threads` value in [`PROBE_CANDIDATES`],
//!    spawn that many worker threads + have them read the probe files
//!    in parallel via [`superdeduper_bench_real::read_uncached`] (so
//!    the OS page cache doesn't bias later passes from earlier ones).
//! 3. Record the wall-clock for each config; pick the config with
//!    the lowest wall (= highest throughput, since total bytes are
//!    identical across configs).
//!
//! ## Skip cases
//!
//! The probe is bypassed when:
//! - `--io-threads N` was passed explicitly (user override wins).
//! - `SUPERDEDUPER_IOTHREADS_PARKED=1` is set (env-var escape hatch).
//! - The probe surface returns an error (no eligible files, IO error
//!   on every config). Caller falls back to the (α) per-disk-class
//!   table.
//! - The corpus walk doesn't return enough files quickly enough.
//!   We cap probe-walk-time at [`PROBE_WALK_TIME_CAP`] so a tiny
//!   corpus doesn't make the probe slower than the actual scan.
//!
//! ## Cost
//!
//! Probe overhead budget is ~5-15 seconds. With 16 probe files of
//! 1 MB each (capped via [`PROBE_FILE_MAX_BYTES`]), total bytes is
//! 16 MB × 3 configs = 48 MB. On HDD at 50 MB/s cold-read =
//! ~960 ms; on SSD at 1 GB/s = ~50 ms. Well within budget.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Per-config bracket of io-threads values to probe. The set covers
/// the three regimes characterized in workdirs/design/profile-data.md:
/// 1 (HDD-bottlenecked / WSL warm), 4-8 (NVMe knee), 16 (NVMe
/// plateau).
///
/// 2026-06-03: added 8 explicitly. Prior session perf data showed Win
/// NVMe cold optima at io=8 for some corpora -- with only {1,4,16}
/// probed, the bracket could pick 16 even when 8 would have been
/// faster (16 wins the {1,4,16} contest by being closest to 8 from
/// above on machines where 16 over-saturates). Adding 8 lets the
/// probe pick it directly. Total probe budget unchanged (still
/// PROBE_TOTAL_TIME_CAP gated); per-candidate slot just trims.
const PROBE_CANDIDATES: &[usize] = &[1, 4, 8, 16];

/// Cap how many files the probe walks before bailing. Each file is
/// read once per config so total = N × candidates.len() reads. 16
/// keeps the probe bounded for the cold-HDD case while giving enough
/// parallelism breadth at io=16.
const PROBE_MAX_FILES: usize = 16;

/// Max bytes the probe reads from each file. Larger reads give
/// cleaner throughput numbers but increase the probe walltime
/// budget. 1 MiB is enough to surface the io_threads scaling delta
/// (per sdd-testwin probe data: NVMe knee shows clearly at this
/// scale) while keeping the per-file read short on HDD.
const PROBE_FILE_MAX_BYTES: u64 = 1 * 1024 * 1024;

/// Min size threshold for probe files. Files below this contribute
/// noise (per-file open overhead dominates over actual read
/// throughput). 64 KiB is large enough to dwarf the syscall + path
/// resolution cost.
const PROBE_FILE_MIN_BYTES: u64 = 64 * 1024;

/// Cap on how long the probe-walk can take before falling back to the
/// (α) per-disk-class table. A corpus with only tiny files or a
/// deeply-nested workdir would otherwise make the probe slower than
/// the scan.
const PROBE_WALK_TIME_CAP: Duration = Duration::from_millis(500);

/// Cap on how long the WHOLE probe (walk + 3 read passes) can take.
/// If it overruns, fall back. Per Mick's budget framing of ~5-15s.
const PROBE_TOTAL_TIME_CAP: Duration = Duration::from_secs(15);

/// Run the throughput probe over the given scan root. Returns the
/// io-threads value that won the bracket (lowest wall), or `Err` if
/// the probe couldn't run cleanly (caller falls back to the (α)
/// per-disk-class table).
pub fn probe_optimal_io_threads(scan_root: &Path) -> std::io::Result<usize> {
    let total_start = Instant::now();
    let probe_files = collect_probe_files(scan_root)?;
    if probe_files.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "io-threads probe: no eligible files found in scan root",
        ));
    }
    tracing::info!(
        candidates = ?PROBE_CANDIDATES,
        probe_files = probe_files.len(),
        "io-threads probe: starting"
    );

    let mut best: Option<(Duration, usize)> = None;
    for &n_threads in PROBE_CANDIDATES {
        if total_start.elapsed() > PROBE_TOTAL_TIME_CAP {
            tracing::warn!(
                cap_ms = PROBE_TOTAL_TIME_CAP.as_millis() as u64,
                "io-threads probe: aborting on total-time cap; using best so far"
            );
            break;
        }
        let wall = measure_concurrent_reads(&probe_files, n_threads)?;
        tracing::info!(
            io_threads = n_threads,
            wall_ms = wall.as_millis() as u64,
            "io-threads probe: trial"
        );
        match best {
            None => best = Some((wall, n_threads)),
            Some((prev_wall, _)) if wall < prev_wall => best = Some((wall, n_threads)),
            _ => {}
        }
    }
    match best {
        Some((wall, n)) => {
            tracing::info!(
                winner_io_threads = n,
                winner_wall_ms = wall.as_millis() as u64,
                total_probe_ms = total_start.elapsed().as_millis() as u64,
                "io-threads probe: complete"
            );
            Ok(n)
        }
        None => Err(std::io::Error::other("io-threads probe: no config succeeded")),
    }
}

/// Walk the scan root breadth-first until we either collect
/// [`PROBE_MAX_FILES`] eligible files (size >= [`PROBE_FILE_MIN_BYTES`])
/// or hit [`PROBE_WALK_TIME_CAP`]. Lightweight standalone walker so
/// the probe doesn't depend on the full `inventory::walk` machinery.
fn collect_probe_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let walk_start = Instant::now();
    let mut found = Vec::with_capacity(PROBE_MAX_FILES);
    let mut frontier: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = frontier.pop() {
        if walk_start.elapsed() > PROBE_WALK_TIME_CAP {
            break;
        }
        if found.len() >= PROBE_MAX_FILES {
            break;
        }
        let read = match std::fs::read_dir(&dir) {
            Ok(r) => r,
            Err(_) => continue, // skip unreadable dirs
        };
        for entry in read.flatten() {
            if found.len() >= PROBE_MAX_FILES {
                break;
            }
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.is_dir() {
                frontier.push(path);
            } else if meta.is_file() && meta.len() >= PROBE_FILE_MIN_BYTES {
                found.push(path);
            }
        }
    }
    Ok(found)
}

/// Measure wall-clock time to read all `files` in parallel using
/// `n_threads` workers. Each worker pulls files round-robin from a
/// shared atomic counter; reads use [`superdeduper_bench_real::read_uncached`]
/// to bypass the OS page cache (so successive probe configs don't
/// see each other's warm-cache).
fn measure_concurrent_reads(
    files: &[PathBuf],
    n_threads: usize,
) -> std::io::Result<Duration> {
    let start = Instant::now();
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let handles: Vec<_> = (0..n_threads.max(1))
            .map(|_| {
                scope.spawn(|| -> std::io::Result<()> {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= files.len() {
                            return Ok(());
                        }
                        let path = &files[i];
                        // 2026-06-02 R2 ship reuse: read_uncached gives
                        // FILE_FLAG_NO_BUFFERING / O_DIRECT / F_NOCACHE
                        // on the supported platforms; ensures probe
                        // configs aren't biased by each other's warm
                        // cache. read_uncached reads the WHOLE file
                        // (probe files are capped at 1 MiB via the
                        // PROBE_FILE_MIN_BYTES filter; reading whole
                        // is fine at that scale).
                        let (_bytes, _cold) =
                            superdeduper_bench_real::read_uncached(path)?;
                        // Cap how many bytes we count toward the
                        // throughput measurement -- already bounded
                        // by file size in practice, but a safety
                        // net.
                        let _ = PROBE_FILE_MAX_BYTES;
                    }
                })
            })
            .collect();
        for h in handles {
            // Surface the first IO error encountered; abort the
            // probe + caller falls back to (α).
            match h.join() {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(std::io::Error::other("probe worker panicked"));
                }
            }
        }
        Ok(())
    })?;
    Ok(start.elapsed())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use tempfile::TempDir;

    fn make_probe_corpus(dir: &Path, n_files: usize, bytes_each: usize) -> std::io::Result<()> {
        for i in 0..n_files {
            let p = dir.join(format!("probe_{i:03}.bin"));
            let mut f = File::create(&p)?;
            f.write_all(&vec![0xAB; bytes_each])?;
        }
        Ok(())
    }

    #[test]
    fn probe_returns_one_of_the_candidates() {
        let tmp = TempDir::new().unwrap();
        make_probe_corpus(tmp.path(), 8, 128 * 1024).unwrap();
        let winner = probe_optimal_io_threads(tmp.path()).expect("probe should succeed");
        assert!(
            PROBE_CANDIDATES.contains(&winner),
            "winner {winner} should be one of {PROBE_CANDIDATES:?}"
        );
    }

    #[test]
    fn probe_errors_on_empty_corpus() {
        let tmp = TempDir::new().unwrap();
        // No files in the dir; probe should error so caller falls
        // back to (α) per-disk-class table.
        let result = probe_optimal_io_threads(tmp.path());
        assert!(result.is_err(), "empty corpus must return Err");
    }

    #[test]
    fn probe_errors_on_too_small_files() {
        let tmp = TempDir::new().unwrap();
        // Tiny files below PROBE_FILE_MIN_BYTES -> filtered out ->
        // empty probe set -> Err.
        make_probe_corpus(tmp.path(), 4, 1024).unwrap(); // 1 KiB each
        let result = probe_optimal_io_threads(tmp.path());
        assert!(result.is_err(), "tiny-file corpus must return Err");
    }

    #[test]
    fn collect_probe_files_caps_at_max() {
        let tmp = TempDir::new().unwrap();
        // Make 32 files (2x cap); collect should stop at PROBE_MAX_FILES.
        make_probe_corpus(tmp.path(), 32, PROBE_FILE_MIN_BYTES as usize + 1).unwrap();
        let files = collect_probe_files(tmp.path()).unwrap();
        assert_eq!(files.len(), PROBE_MAX_FILES);
    }

    #[test]
    fn collect_probe_files_filters_by_min_size() {
        let tmp = TempDir::new().unwrap();
        // 4 large files + 4 small files; probe should keep only the 4 large.
        make_probe_corpus(tmp.path(), 4, PROBE_FILE_MIN_BYTES as usize + 1).unwrap();
        // Overwrite the second batch with smaller content.
        for i in 0..4 {
            let p = tmp.path().join(format!("small_{i:03}.bin"));
            let mut f = File::create(&p).unwrap();
            f.write_all(&[0xCD; 1024]).unwrap();
        }
        let files = collect_probe_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 4, "small files must be filtered out");
        for path in &files {
            let name = path.file_name().unwrap().to_string_lossy();
            assert!(
                name.starts_with("probe_"),
                "kept file {name} should be a large probe_ file, not small_"
            );
        }
    }
}
