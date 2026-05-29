//! T-BENCH-ME shared `--bench-me` / GUI-button orchestration. ONE
//! implementation called by BOTH the CLI (`main::run_bench_me`) and the GUI
//! "Run Canonical Bench" button -> parity by construction (no CLI/GUI drift).
//!
//! Flow: POST /bench/start -> download + untar the corpus -> real full-content
//! dedupe (reads every byte: ranked wall + I/O signal) -> answer the server's
//! possession challenge (tag-0x02 hashes of the downloaded bytes) -> HMAC submit
//! (scope=canonical-bench). The server verifies result + challenge directly.
#![cfg(feature = "telemetry")]

use anyhow::Context;
use std::collections::HashMap;
use std::path::Path;

use super::{bench_client, hardware, install, submission};

/// Structured result of a bench run (for the CLI to print + the GUI to render).
pub struct BenchOutcome {
    pub bench_run_id: String,
    pub corpus_version: String,
    pub dup_groups: usize,
    pub bytes_scanned: u64,
    pub files_scanned: u64,
    pub dedupe_secs: f64,
    pub result_digest: String,
    /// True when EVERY timed candidate read bypassed the OS cache (cold-enforce,
    /// #106 pt2). False if any read fell back to buffered (unsupported
    /// platform/fs) — the throughput then isn't a trustworthy cold number.
    pub cold_enforced: bool,
    /// The server outcome when submitted; `None` for a run-locally-only run.
    pub submit: Option<submission::SubmitOutcome>,
    /// The assembled submission inputs, retained so a run-locally result can be
    /// submitted later ([Submit now]) without re-running the whole bench.
    pub inputs: submission::SubmissionInputs,
}

/// Cancelled before completion (checked between stages). The GUI surfaces this
/// as a clean abort with no partial submit.
#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "benchmark cancelled")
    }
}
impl std::error::Error for Cancelled {}

/// Run the full canonical-bench loop. `progress(msg)` is called at each stage
/// (CLI prints to stderr; GUI pushes to its status channel). `workroot` picks
/// where the corpus downloads (default temp; pass a real disk if temp is
/// RAM-backed). The corpus dir is removed unless `keep`.
pub fn run(
    state: &install::InstallState,
    corpus_version: &str,
    tier: &str,
    workroot: Option<&Path>,
    fresh: bool,
    submit: bool,
    cancel: &std::sync::atomic::AtomicBool,
    mut progress: impl FnMut(&str),
) -> anyhow::Result<BenchOutcome> {
    use std::sync::atomic::Ordering;
    let check_cancel = || -> anyhow::Result<()> {
        if cancel.load(Ordering::Relaxed) {
            anyhow::bail!(Cancelled);
        }
        Ok(())
    };
    anyhow::ensure!(state.registered, "install not registered; run `superdeduper register`");
    let base = state.server_url.trim_end_matches('/').to_string();

    // 1. POST /bench/start
    progress(&format!("requesting bench run ({corpus_version}, {tier})"));
    let start: serde_json::Value = ureq::post(&format!("{base}/api/v1/bench/start"))
        .send_json(serde_json::json!({
            "install_id": state.install_id,
            "corpus_version": corpus_version,
            "tier": tier,
        }))
        .context("POST /bench/start failed")?
        .into_json()
        .context("parsing /bench/start response")?;
    let getstr = |k: &str| -> anyhow::Result<String> {
        start.get(k).and_then(|v| v.as_str()).map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("/bench/start response missing string field '{k}'"))
    };
    let bench_run_id = getstr("bench_run_id")?;
    let download_url = getstr("download_url")?;
    let protocol_version = getstr("protocol_version")?;
    let corpus_version = getstr("corpus_version")?;
    let tier = getstr("tier")?;
    let challenges: Vec<bench_client::ChallengePosition> =
        serde_json::from_value(start.get("challenges").cloned().unwrap_or_default())
            .context("parsing challenges[]")?;

    // 2. download + untar the corpus -- CACHED per corpus_version so repeat
    // runs reuse the bytes instead of re-pulling 100MB+ every time. The corpus
    // is deterministic for a given corpus_version, so reuse is safe (the server
    // still issues fresh challenges each run; a stale/wrong cache would fail the
    // possession check). `fresh` forces a re-download.
    let workroot = workroot.map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
    let slug: String = corpus_version
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let corpus_dir = workroot.join(format!("sd-bench-corpus-{slug}"));
    let complete = corpus_dir.join(".sd-bench-complete");
    check_cancel()?;
    if fresh || !complete.exists() {
        let _ = std::fs::remove_dir_all(&corpus_dir);
        std::fs::create_dir_all(&corpus_dir)?;
        progress(&format!("downloading + extracting corpus ({} challenges)", challenges.len()));
        let resp = ureq::get(&download_url).call().context("GET download_url failed")?;
        // Wrap the download stream so Cancel aborts mid-pull (the 100MB
        // download is the long pole; checking only at stage boundaries
        // would leave Cancel unresponsive for the whole download). The
        // reader returns an io error the instant `cancel` is set;
        // afterwards we re-check and surface it as a clean `Cancelled`
        // rather than a corrupt-tar error (spec §9 G2).
        let reader = CancelReader { inner: resp.into_reader(), cancel };
        if let Err(e) = tar::Archive::new(reader).unpack(&corpus_dir) {
            check_cancel()?; // cancel mid-download -> clean Cancelled, not a tar error
            let _ = std::fs::remove_dir_all(&corpus_dir); // don't leave a half-extracted cache
            return Err(anyhow::Error::new(e).context("extracting corpus tar"));
        }
        std::fs::write(&complete, corpus_version.as_bytes()).context("writing cache sentinel")?;
    } else {
        progress(&format!("reusing cached corpus at {} ({} challenges)", corpus_dir.display(), challenges.len()));
    }

    // 3. real size-grouped exact dedupe (the ranked wall + I/O signal). The
    // timed window covers enumerate + size-group + hash-candidates;
    // bytes_scanned = candidate bytes hashed (the throughput numerator).
    check_cancel()?;
    progress("deduping (size-grouped, cold-enforced reads)");
    let t = std::time::Instant::now();
    let (dupsets, bytes_scanned, files_scanned, cold_enforced) = full_content_dedup(&corpus_dir)?;
    let dedupe_secs = t.elapsed().as_secs_f64();
    progress(&format!(
        "deduped {dedupe_secs:.2}s: {files_scanned} files enumerated, {} dup groups, cold-enforced={cold_enforced}",
        dupsets.len()
    ));

    // 4. answer the possession challenge
    let (answers, _read) = bench_client::answer_challenge_from_dir(&corpus_dir, &challenges)
        .context("answering challenge from disk")?;
    let result_digest = bench_client::result_digest(&dupsets);

    // 5. assemble + 6. submit
    let bench = bench_client::to_canonical_bench(
        &protocol_version, &corpus_version, &tier, &bench_run_id, &answers, &dupsets,
    );
    let largest = dupsets.iter().map(|g| g.len() as u64).max().unwrap_or(0);
    let inputs = submission::SubmissionInputs {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        run_uuid: uuid::Uuid::new_v4().to_string(),
        scan_id: None,
        bench: Some(bench),
        hardware: hardware::detect_with_root_hint(Some(&corpus_dir)),
        run_shape: submission::RunShape {
            wall_clock_seconds: dedupe_secs,
            bytes_scanned,
            files_scanned,
            hash_algorithm: "blake3".to_string(),
            walker_variant: "walker".to_string(),
            scope: "canonical-bench".to_string(),
            features_used_bitmap: 0,
            corpus_kind: "canonical-bench".to_string(),
            cache_hit_ratio: None,
            easter_egg_hits: Vec::new(),
            zero_byte_group_max: None,
            max_hardlink_count_in_scan: None,
            name_collision_count: None,
            share_count_in_scope: None,
            dry_run: None,
            groups_reviewed_count: None,
        },
        result_summary: submission::ResultSummary {
            duplicate_groups: dupsets.len() as u64,
            duplicate_bytes_reclaimable: 0,
            largest_single_group_bytes: largest,
            actions_taken_summary: std::collections::BTreeMap::new(),
            placeholder_skip_count: None,
            placeholder_skip_bytes: None,
            client_found_dupsets: Some(dupsets.clone()),
        },
    };
    // 6. submit (only on Run & submit; Run-locally returns inputs for later).
    let submit_outcome = if submit {
        check_cancel()?;
        progress("submitting");
        Some(submission::submit(state, &inputs))
    } else {
        progress("done (local only -- not submitted)");
        None
    };

    // corpus_dir is the persistent cache -- intentionally NOT removed, so the
    // next run reuses it (pass `fresh` to force a re-download).
    Ok(BenchOutcome {
        bench_run_id,
        corpus_version,
        dup_groups: dupsets.len(),
        bytes_scanned,
        files_scanned,
        dedupe_secs,
        result_digest,
        cold_enforced,
        submit: submit_outcome,
        inputs,
    })
}

/// A `Read` adapter that aborts the instant a cancel flag is set, so a
/// long download can be interrupted mid-stream (not just at stage
/// boundaries). On cancel it returns an `Interrupted` io error; the
/// caller re-checks the flag and reports a clean `Cancelled`.
struct CancelReader<'a, R> {
    inner: R,
    cancel: &'a std::sync::atomic::AtomicBool,
}
impl<R: std::io::Read> std::io::Read for CancelReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "benchmark cancelled during download",
            ));
        }
        self.inner.read(buf)
    }
}

/// Read a file fully, BYPASSING the OS page cache, so the bench's timed
/// dedup reflects cold-from-disk throughput (cold-enforce, #106 pt2 — the
/// client half of the anti-forge closure: in-memory recovery can't fake the
/// real read). Returns `(bytes, was_cold)`; `was_cold == false` means this
/// platform/filesystem couldn't bypass the cache and we fell back to a
/// normal buffered read (still byte-correct, just not cold-enforced).
///
/// * Linux: `O_DIRECT` + sector-aligned chunked reads (falls back when the fs
///   rejects O_DIRECT, e.g. tmpfs/overlay, or the file isn't sector-aligned).
/// * Windows: `FILE_FLAG_NO_BUFFERING` (same aligned scheme as `diagnose`).
/// * macOS: `F_NOCACHE` (no alignment constraint).
/// * other: buffered fallback (`was_cold == false`).
#[cfg(target_os = "linux")]
fn read_uncached(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    const ALIGN: usize = 4096;
    const CHUNK: usize = 1 << 20;
    let size = std::fs::metadata(path)?.len() as usize;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: cpath is a valid NUL-terminated C string for the duration.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECT) };
    // O_DIRECT unsupported on this fs, or file not sector-aligned -> buffered.
    if fd < 0 || size % ALIGN != 0 {
        if fd >= 0 {
            // SAFETY: fd is a valid open descriptor we are closing exactly once.
            unsafe { libc::close(fd) };
        }
        return Ok((std::fs::read(path)?, false));
    }
    // SAFETY: fd is a freshly-opened, owned descriptor; File takes ownership.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    // Sector-aligned CHUNK buffer (Vec is only word-aligned; slice into the
    // aligned region). O_DIRECT requires buffer + count sector-aligned.
    let mut storage = vec![0u8; CHUNK + ALIGN];
    let pad = (ALIGN - (storage.as_ptr() as usize % ALIGN)) % ALIGN;
    let mut out = Vec::with_capacity(size);
    loop {
        let n = file.read(&mut storage[pad..pad + CHUNK])?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&storage[pad..pad + n]);
    }
    Ok((out, true))
}

#[cfg(target_os = "macos")]
fn read_uncached(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    let mut file = std::fs::File::open(path)?;
    // F_NOCACHE: bypass the unified buffer cache for this descriptor's reads.
    // No alignment constraint (unlike O_DIRECT). Best-effort: if it fails we
    // still read, just not cold.
    // SAFETY: valid open fd; F_NOCACHE is a documented fcntl with arg 1.
    let cold = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) } == 0;
    let mut out = Vec::new();
    file.read_to_end(&mut out)?;
    Ok((out, cold))
}

#[cfg(windows)]
fn read_uncached(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, ReadFile, FILE_FLAG_NO_BUFFERING, FILE_FLAG_SEQUENTIAL_SCAN,
        FILE_GENERIC_READ, FILE_SHARE_READ, OPEN_EXISTING,
    };
    const SECTOR: usize = 4096;
    const CHUNK: usize = 1 << 20;
    let size = std::fs::metadata(path)?.len() as usize;
    // NO_BUFFERING needs the file sector-aligned; corpus files are. Else buffered.
    if size % SECTOR != 0 {
        return Ok((std::fs::read(path)?, false));
    }
    let mut storage = vec![0u8; CHUNK + SECTOR];
    let pad = (SECTOR - (storage.as_ptr() as usize % SECTOR)) % SECTOR;
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: wide is NUL-terminated + outlives the call; flags are valid for sync ReadFile.
    let handle = unsafe {
        match CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_SEQUENTIAL_SCAN,
            HANDLE::default(),
        ) {
            Ok(h) => h,
            Err(_) => return Ok((std::fs::read(path)?, false)), // can't open uncached -> buffered
        }
    };
    struct Closer(HANDLE);
    impl Drop for Closer {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    let _guard = Closer(handle);
    let mut out = Vec::with_capacity(size);
    loop {
        let mut read_bytes: u32 = 0;
        // SAFETY: buf is sector-aligned, len is a sector multiple, handle open.
        unsafe {
            if ReadFile(
                handle,
                Some(&mut storage[pad..pad + CHUNK]),
                Some(&mut read_bytes as *mut u32),
                None,
            )
            .is_err()
            {
                return Err(std::io::Error::last_os_error());
            }
        }
        if read_bytes == 0 {
            break;
        }
        out.extend_from_slice(&storage[pad..pad + read_bytes as usize]);
    }
    Ok((out, true))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_uncached(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    // No portable cache-bypass on this platform -> buffered (not cold).
    Ok((std::fs::read(path)?, false))
}

/// Size-grouped exact-dedupe — the work a REAL dedup actually does. A
/// byte-identical pair necessarily shares a size, so size is a sound
/// prefilter: we only need to HASH files that share a size with >=1 other
/// file (a size-group of >=2). Files with a unique size can't have a
/// duplicate, so they are never read. This yields the IDENTICAL canonical
/// dup sets (and thus identical `result_digest`) as a full-content hash —
/// correctness-neutral, independently confirmed by testrunner (95CBwzmO
/// unchanged) — while reading only the CANDIDATE bytes (~0.889 GiB on
/// corpus-v2-quick, testrunner-canonical), NOT the full corpus.
///
/// Returns `(dupsets, candidate_bytes_hashed, files_enumerated)`:
/// `candidate_bytes_hashed` is the timed I/O work + the leaderboard
/// throughput numerator (must equal the server's per-corpus_version bytes
/// constant per the quality forge-fix); `files_enumerated` is the total
/// corpus file count seen (the scan scope), not just candidates.
fn full_content_dedup(dir: &Path) -> anyhow::Result<(Vec<Vec<u64>>, u64, u64, bool)> {
    // Pass 1: enumerate + stat (size only, no content reads). Group
    // path_indices by file size.
    let mut by_size: HashMap<u64, Vec<(u64, std::path::PathBuf)>> = HashMap::new();
    let mut files_enumerated = 0u64;
    for entry in std::fs::read_dir(dir).context("reading corpus dir")? {
        let entry = entry?;
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let pi = match super::bench_corpus::parse_corpus_path_index(&name) {
            Some(p) => p,
            None => continue,
        };
        let size = entry
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        files_enumerated += 1;
        by_size.entry(size).or_default().push((pi, path));
    }

    // Pass 2: hash ONLY candidates — files in a size-group of >=2. A
    // unique-size file cannot be part of any duplicate group, so skipping
    // it changes neither the dup sets nor the result_digest. Reads are
    // CACHE-BYPASSING (cold-enforce, #106 pt2); `cold_enforced` tracks
    // whether EVERY candidate read actually went cold (false if any fell
    // back to a buffered read on an unsupported platform/fs).
    let mut by_hash: HashMap<[u8; 32], Vec<u64>> = HashMap::new();
    let mut candidate_bytes = 0u64;
    let mut cold_enforced = true;
    let mut any_candidate = false;
    for (_size, group) in by_size {
        if group.len() < 2 {
            continue;
        }
        for (pi, path) in group {
            let (data, cold) =
                read_uncached(&path).with_context(|| format!("reading {}", path.display()))?;
            cold_enforced &= cold;
            any_candidate = true;
            candidate_bytes += data.len() as u64;
            by_hash
                .entry(*blake3::hash(&data).as_bytes())
                .or_default()
                .push(pi);
        }
    }
    // A corpus with no size-collisions read nothing -> not meaningfully cold.
    let cold_enforced = cold_enforced && any_candidate;
    let mut dupsets: Vec<Vec<u64>> = by_hash
        .into_values()
        .filter(|v| v.len() >= 2)
        .map(|mut v| {
            v.sort_unstable();
            v
        })
        .collect();
    dupsets.sort();
    Ok((dupsets, candidate_bytes, files_enumerated, cold_enforced))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, idx: u64, content: &[u8]) {
        let p = dir.join(format!("f{idx:06}.bin"));
        std::fs::File::create(&p).unwrap().write_all(content).unwrap();
    }

    #[test]
    fn read_uncached_returns_correct_bytes() {
        // Byte-correctness of the cache-bypass read across whatever path the
        // platform/fs takes (O_DIRECT / NO_BUFFERING / F_NOCACHE / fallback).
        // Write under ./target (ext4 on the dev box) so the Linux O_DIRECT
        // path actually engages, not just the tmpfs fallback.
        let dir = std::path::Path::new("target").join(format!("sd-uncached-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f0000000001.bin");
        // 40 KiB, a clean 4KiB multiple so O_DIRECT/NO_BUFFERING can read it.
        let content: Vec<u8> = (0..40960u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &content).unwrap();
        let (got, cold) = read_uncached(&p).unwrap();
        assert_eq!(got, content, "uncached read must return byte-identical content");
        eprintln!("read_uncached cold-enforced on this fs = {cold}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn size_group_dedup_is_correctness_neutral_and_skips_unique_sizes() {
        let dir = std::env::temp_dir().join(format!("sd-bench-sgtest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // idx 1,2: identical 100-byte content -> the one dup group.
        write_file(&dir, 1, &[0xAA; 100]);
        write_file(&dir, 2, &[0xAA; 100]);
        // idx 3,4: same size (100) but different content -> candidates
        // (read + hashed) but NOT a dup group.
        write_file(&dir, 3, &[0xBB; 100]);
        write_file(&dir, 4, &[0xCC; 100]);
        // idx 5: unique size (50) -> cannot be a duplicate, so it is NEVER
        // read (the size-group skip) and contributes 0 candidate bytes.
        write_file(&dir, 5, &[0xDD; 50]);

        let (dupsets, candidate_bytes, files_enumerated, _cold) = full_content_dedup(&dir).unwrap();

        // Correctness-neutral: the dup set is exactly {1,2} — identical to
        // what a full-content hash of every file would produce.
        assert_eq!(dupsets, vec![vec![1u64, 2u64]]);
        // Candidate bytes = the four 100-byte same-size files only; the
        // unique-size 50-byte file is skipped (not read). 4 * 100 = 400.
        assert_eq!(candidate_bytes, 400);
        // All five files are enumerated (the scan scope), candidates or not.
        assert_eq!(files_enumerated, 5);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
