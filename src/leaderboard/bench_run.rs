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
    // `Send`: the live-progress poller runs on a scoped thread that owns
    // `progress` for the duration of the dedup. Both callers (CLI -> stderr,
    // GUI -> status channel) are Send.
    mut progress: impl FnMut(&str) + Send,
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

    // 1. POST /bench/start — HMAC-authed per the G fix coupled with #4(a)
    // (web dev rev 584089d + the cap-table swap). Pre-G the endpoint was
    // unsigned; benchmarker hit 401 on v0.2.44 against the upgraded server
    // because the signed-body / X-Sd-Install-Id headers were missing.
    // Sign with install_key over the canonical body; matches every other
    // authed endpoint (submission, action_submission, etc.).
    progress(&format!("requesting bench run ({corpus_version}, {tier})"));
    let install_key = state
        .install_key()
        .ok_or_else(|| anyhow::anyhow!("install_key_hex malformed; re-register"))?;
    let start_payload = serde_json::json!({
        "install_id": state.install_id,
        "corpus_version": corpus_version,
        "tier": tier,
    });
    let start_body = super::hmac_signer::canonical_body(&start_payload);
    let start_signature = super::hmac_signer::sign(&install_key, &start_body);
    let start: serde_json::Value = ureq::post(&format!("{base}/api/v1/bench/start"))
        .set("Content-Type", "application/json")
        .set("X-Sd-Install-Id", &state.install_id)
        .set("X-Sd-Signature", &start_signature)
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&start_body)
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
    // #4(a) — V2 server-bound challenge: /bench/start MAY now return a
    // top-level `server_challenge_blob` (32 random bytes, base64-std,
    // 44 chars). Present => V2 verifier path on the server (tag 0x03 +
    // tcorpus-result-v2); absent => V1 fallback during the transition
    // window (web's BENCH_SERVER_BLOB_REQUIRED flag, default false until
    // engine ships V2). Auto-detect rather than force a mode on the
    // client, so a single engine binary speaks both protocols.
    let server_blob: Option<[u8; 32]> = start
        .get("server_challenge_blob")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(s).ok()
        })
        .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok());
    if server_blob.is_some() {
        crate::log_info!(
            "bench: #4(a) V2 detected — server_challenge_blob present; using tag-0x03 challenge_hash + tcorpus-result-v2 + bench_proof.challenge_blob_echo"
        );
    } else {
        crate::log_info!(
            "bench: #4(a) V1 fallback — no server_challenge_blob in /bench/start response (legacy/transition path)"
        );
    }

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
    // Live progress while the parallel reads run: workers bump `files_done`;
    // a scoped poller thread owns `progress` and reports done/total every
    // ~400ms. Only the poller touches `progress` inside the scope (the dedup
    // body never does), so the &mut borrow is exclusive. `cancel` is honoured
    // per file inside the dedup, so the user is never trapped on a long run.
    let files_done = std::sync::atomic::AtomicU64::new(0);
    let total_candidates = std::sync::atomic::AtomicU64::new(0);
    let dedup_finished = std::sync::atomic::AtomicBool::new(false);
    let dedup_result = std::thread::scope(|s| {
        s.spawn(|| {
            while !dedup_finished.load(Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let total = total_candidates.load(Ordering::Relaxed);
                if total > 0 && !dedup_finished.load(Ordering::Relaxed) {
                    let done = files_done.load(Ordering::Relaxed);
                    progress(&format!(
                        "deduping {done}/{total} candidates (cold-enforced reads)"
                    ));
                }
            }
        });
        let r = full_content_dedup(&corpus_dir, cancel, &files_done, &total_candidates);
        dedup_finished.store(true, Ordering::Relaxed);
        r
    });
    let (dupsets, bytes_scanned, files_scanned, cold_enforced) = dedup_result?;
    let dedupe_secs = t.elapsed().as_secs_f64();
    progress(&format!(
        "deduped {dedupe_secs:.2}s: {files_scanned} files enumerated, {} dup groups, cold-enforced={cold_enforced}",
        dupsets.len()
    ));

    // 4. answer the possession challenge — V2 (tag 0x03 + appended blob) when
    // /bench/start returned a server_challenge_blob; else V1 (tag 0x02).
    let (answers, _read) = bench_client::answer_challenge_from_dir_v(
        &corpus_dir,
        &challenges,
        server_blob.as_ref(),
    )
    .context("answering challenge from disk")?;
    let result_digest = match server_blob.as_ref() {
        Some(b) => bench_client::result_digest_v2(&dupsets, b),
        None => bench_client::result_digest(&dupsets),
    };

    // 5. assemble + 6. submit — bench_proof carries challenge_blob_echo when V2.
    let bench = bench_client::to_canonical_bench_v(
        &protocol_version, &corpus_version, &tier, &bench_run_id, &answers, &dupsets,
        cold_enforced,
        server_blob.as_ref(),
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
/// Whether O_DIRECT actually BYPASSES the page cache in this environment.
/// O_DIRECT is advisory: WSL2 (ext4-on-vhdx / drvfs) ACCEPTS the flag but
/// silently serves from the Windows host cache — `open(O_DIRECT)` succeeds with
/// no buffered fallback, yet the read is WARM. Reporting `cold=true` there is a
/// false positive that would let a warm run land on the competitive cold board
/// (testrunner's WSL finding — the 125x-leverage forge hole achievements
/// flagged). Fail closed: detect WSL via /proc/version and treat it as not
/// bypass-capable so `cold_enforced` honestly becomes false (-> casual board).
/// Real Linux, where O_DIRECT is honoured, stays reliable. tmpfs/overlay
/// already fail honestly via the `open` rejection -> buffered -> cold=false.
#[cfg(target_os = "linux")]
fn cold_bypass_reliable() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| match std::fs::read_to_string("/proc/version") {
        Ok(v) => {
            let v = v.to_ascii_lowercase();
            !(v.contains("microsoft") || v.contains("wsl"))
        }
        // /proc/version unreadable (unusual) -> assume a real kernel that
        // honours O_DIRECT; WSL is the targeted marker case.
        Err(_) => true,
    })
}

#[cfg(target_os = "linux")]
fn read_uncached(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::FromRawFd;
    // WSL accepts O_DIRECT but serves warm -> a false cold=true. Fail closed:
    // buffered read + cold=false so the run cannot claim the competitive cold
    // board on a bypass-incapable fs.
    if !cold_bypass_reliable() {
        return Ok((std::fs::read(path)?, false));
    }
    const ALIGN: usize = 4096;
    const CHUNK: usize = 1 << 20;
    let size = std::fs::metadata(path)?.len() as usize;
    let cpath = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: cpath is a valid NUL-terminated C string for the duration.
    let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDONLY | libc::O_DIRECT) };
    if fd < 0 {
        // O_DIRECT unsupported on this fs (tmpfs/overlay/drvfs) -> buffered.
        return Ok((std::fs::read(path)?, false));
    }
    // O_DIRECT needs sector-aligned offset + count, so cold-read the
    // sector-aligned BULK and read the sub-sector TAIL via a separate
    // buffered handle (the tail is < 1 sector -> negligible cache warming).
    // This makes cold-enforce work for ARBITRARY file sizes, not just
    // 4KiB-multiples (the realistic corpus has varied sizes — benchmarker's
    // r4 blocker). Sub-sector files (size < ALIGN) have an empty bulk -> the
    // whole read is the buffered tail -> NOT cold (honest: cold=false).
    let bulk = size - (size % ALIGN);
    // SAFETY: fd is a freshly-opened, owned descriptor; File takes ownership.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut storage = vec![0u8; CHUNK + ALIGN];
    let pad = (ALIGN - (storage.as_ptr() as usize % ALIGN)) % ALIGN;
    let mut out = Vec::with_capacity(size);
    let mut remaining = bulk;
    while remaining > 0 {
        // `want` stays sector-aligned: CHUNK is a sector multiple and
        // `remaining` starts sector-aligned (== bulk) and only decreases by
        // O_DIRECT reads (which return sector multiples below EOF).
        let want = remaining.min(CHUNK);
        let n = file.read(&mut storage[pad..pad + want])?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&storage[pad..pad + n]);
        remaining -= n;
    }
    drop(file); // close the O_DIRECT fd before the buffered tail read
    if out.len() < size {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut bf = std::fs::File::open(path)?;
        bf.seek(SeekFrom::Start(out.len() as u64))?;
        let mut tail = vec![0u8; size - out.len()];
        bf.read_exact(&mut tail)?;
        out.extend_from_slice(&tail);
    }
    // Cold-enforced iff we cold-read at least one full sector of bulk.
    Ok((out, bulk > 0))
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
    // NO_BUFFERING needs sector-aligned offset + count, so cold-read the
    // sector-aligned BULK and read the sub-sector TAIL via a buffered handle
    // (< 1 sector -> negligible warming). Works for ARBITRARY file sizes, not
    // just 4KiB-multiples (the realistic corpus has varied sizes —
    // benchmarker's r4 blocker). Sub-sector files (size < SECTOR) -> empty
    // bulk -> whole read is the buffered tail -> NOT cold (honest cold=false).
    let bulk = size - (size % SECTOR);
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
    let guard = Closer(handle);
    let mut out = Vec::with_capacity(size);
    let mut remaining = bulk;
    while remaining > 0 {
        // `want` stays sector-aligned: CHUNK is a sector multiple and
        // `remaining` (== bulk initially) decreases by sector-multiple reads
        // (all below EOF since bulk <= size).
        let want = remaining.min(CHUNK);
        let mut read_bytes: u32 = 0;
        // SAFETY: buf is sector-aligned, `want` is a sector multiple, handle open.
        unsafe {
            if ReadFile(
                handle,
                Some(&mut storage[pad..pad + want]),
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
        remaining -= read_bytes as usize;
    }
    drop(guard); // close the NO_BUFFERING handle before the buffered tail read
    if out.len() < size {
        use std::io::{Read as _, Seek as _, SeekFrom};
        let mut bf = std::fs::File::open(path)?;
        bf.seek(SeekFrom::Start(out.len() as u64))?;
        let mut tail = vec![0u8; size - out.len()];
        bf.read_exact(&mut tail)?;
        out.extend_from_slice(&tail);
    }
    // Cold-enforced iff we cold-read at least one full sector of bulk.
    Ok((out, bulk > 0))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn read_uncached(path: &Path) -> std::io::Result<(Vec<u8>, bool)> {
    // No portable cache-bypass on this platform -> buffered (not cold).
    Ok((std::fs::read(path)?, false))
}

/// Per-candidate diff report for `debug_dedup_diff`. One entry per
/// candidate whose three reads (parallel cold-bypass, serial cold-bypass,
/// serial buffered) don't all agree on `blake3(content)`.
pub struct DebugDedupDiff {
    pub path_index: u64,
    pub path: std::path::PathBuf,
    pub size: u64,
    pub parallel_hash: [u8; 32],
    pub parallel_cold: bool,
    pub serial_hash: [u8; 32],
    pub serial_cold: bool,
    pub buffered_hash: [u8; 32],
}

/// Aggregate report from `debug_dedup_diff`. The three dup-group counts
/// should agree on a correct dedup; any divergence is the bug.
pub struct DebugDedupDiffReport {
    pub files_enumerated: u64,
    pub candidate_count: u64,
    pub parallel_dup_groups: usize,
    pub serial_dup_groups: usize,
    pub buffered_dup_groups: usize,
    /// candidates where the three reads disagree, sorted by path_index.
    pub diffs: Vec<DebugDedupDiff>,
}

/// DEBUG helper for the cert digest mismatch (#116): dedup a corpus dir
/// THREE WAYS — production parallel cold-bypass, serial cold-bypass, and
/// serial buffered — and report any per-candidate hash divergence. Runs the
/// PARALLEL pass FIRST so the OS page cache is coldest for the suspect path.
/// Cheap on a cached corpus (a few minutes for 1M files), no submission, no
/// network. Telemetry-only.
///
/// Use to isolate the SPECIFIC file(s) whose parallel-cold read returns
/// different bytes than serial/buffered — turns the speculative "parallel x
/// web-staged layout" bug into a concrete file + hash mismatch we can chase
/// down to the actual read backend issue.
pub fn debug_dedup_diff(dir: &Path) -> anyhow::Result<DebugDedupDiffReport> {
    use rayon::prelude::*;

    // Pass 1: enumerate (matches full_content_dedup exactly).
    let mut by_size: HashMap<u64, Vec<(u64, std::path::PathBuf)>> = HashMap::new();
    let mut files_enumerated: u64 = 0;
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

    // Build the same candidates list the production parallel Pass-2 uses.
    let candidates: Vec<(u64, std::path::PathBuf, u64)> = by_size
        .into_iter()
        .filter(|(_size, group)| group.len() >= 2)
        .flat_map(|(size, group)| group.into_iter().map(move |(pi, path)| (pi, path, size)))
        .collect();
    let candidate_count = candidates.len() as u64;

    // ---- PARALLEL cold-bypass (runs FIRST — coldest cache, matches prod) ----
    let cpu_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let io_threads = cpu_threads.saturating_mul(3).max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(io_threads)
        .thread_name(|i| format!("sd-diff-par-{i}"))
        .build()
        .map_err(|e| anyhow::anyhow!("rayon pool: {e}"))?;
    let par_hashed: Vec<(u64, [u8; 32], bool, u64)> = pool.install(|| {
        candidates
            .par_iter()
            .map(|(pi, path, size)| {
                let (data, cold) = read_uncached(path)
                    .with_context(|| format!("parallel read {}", path.display()))?;
                let hash = *blake3::hash(&data).as_bytes();
                Ok::<_, anyhow::Error>((*pi, hash, cold, *size))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;
    anyhow::ensure!(
        par_hashed.len() == candidate_count as usize,
        "parallel pass: rayon collect produced {} != {} candidates",
        par_hashed.len(),
        candidate_count
    );

    // ---- SERIAL cold-bypass (caches now likely warm; still uses NO_BUFFERING) ----
    let mut ser_hashed: Vec<(u64, [u8; 32], bool)> = Vec::with_capacity(candidates.len());
    for (pi, path, _size) in &candidates {
        let (data, cold) = read_uncached(path)
            .with_context(|| format!("serial read {}", path.display()))?;
        let hash = *blake3::hash(&data).as_bytes();
        ser_hashed.push((*pi, hash, cold));
    }

    // ---- BUFFERED serial reference (always-cached path, ground truth) ----
    let mut buf_hashed: Vec<(u64, [u8; 32])> = Vec::with_capacity(candidates.len());
    for (pi, path, _size) in &candidates {
        let data = std::fs::read(path)
            .with_context(|| format!("buffered read {}", path.display()))?;
        let hash = *blake3::hash(&data).as_bytes();
        buf_hashed.push((*pi, hash));
    }

    // Map per pi for diffing.
    let par_map: HashMap<u64, ([u8; 32], bool, u64)> = par_hashed
        .iter()
        .map(|(pi, h, c, s)| (*pi, (*h, *c, *s)))
        .collect();
    let ser_map: HashMap<u64, ([u8; 32], bool)> = ser_hashed
        .iter()
        .map(|(pi, h, c)| (*pi, (*h, *c)))
        .collect();
    let buf_map: HashMap<u64, [u8; 32]> = buf_hashed.iter().copied().collect();

    let mut diffs: Vec<DebugDedupDiff> = Vec::new();
    for (pi, path, size) in &candidates {
        let (par_h, par_cold, _) = par_map[pi];
        let (ser_h, ser_cold) = ser_map[pi];
        let buf_h = buf_map[pi];
        if par_h != ser_h || par_h != buf_h || ser_h != buf_h {
            diffs.push(DebugDedupDiff {
                path_index: *pi,
                path: path.clone(),
                size: *size,
                parallel_hash: par_h,
                parallel_cold: par_cold,
                serial_hash: ser_h,
                serial_cold: ser_cold,
                buffered_hash: buf_h,
            });
        }
    }
    diffs.sort_by_key(|d| d.path_index);

    // Aggregate dup-group counts per read path.
    fn count_dup_groups<I: Iterator<Item = [u8; 32]>>(iter: I) -> usize {
        let mut m: HashMap<[u8; 32], usize> = HashMap::new();
        for h in iter {
            *m.entry(h).or_insert(0) += 1;
        }
        m.values().filter(|v| **v >= 2).count()
    }
    let parallel_dup_groups = count_dup_groups(par_hashed.iter().map(|(_, h, _, _)| *h));
    let serial_dup_groups = count_dup_groups(ser_hashed.iter().map(|(_, h, _)| *h));
    let buffered_dup_groups = count_dup_groups(buf_hashed.iter().map(|(_, h)| *h));

    Ok(DebugDedupDiffReport {
        files_enumerated,
        candidate_count,
        parallel_dup_groups,
        serial_dup_groups,
        buffered_dup_groups,
        diffs,
    })
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
fn full_content_dedup(
    dir: &Path,
    cancel: &std::sync::atomic::AtomicBool,
    files_done: &std::sync::atomic::AtomicU64,
    total_out: &std::sync::atomic::AtomicU64,
) -> anyhow::Result<(Vec<Vec<u64>>, u64, u64, bool)> {
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
    // Flatten the candidates (size-groups of >=2) into one worklist so the
    // reads can be issued concurrently. A single serial loop here is
    // pathological on the full tier (1M tiny files): single-thread O_DIRECT is
    // latency-bound on per-file open/read/close (~4k files/s), NOT device-
    // bound — which manufactured benchmarker's r3-FULL 250s cold / 125x spread
    // AND undersold us vs czkawka. The production hasher (pipeline/hash.rs)
    // reads on a bounded rayon io_pool; the bench must mirror that for fidelity.
    use rayon::prelude::*;
    let candidates: Vec<(u64, std::path::PathBuf)> = by_size
        .into_iter()
        .filter(|(_size, group)| group.len() >= 2)
        .flat_map(|(_size, group)| group.into_iter())
        .collect();
    let any_candidate = !candidates.is_empty();
    total_out.store(candidates.len() as u64, std::sync::atomic::Ordering::Relaxed);

    // Dedicated, bounded pool that MIRRORS the production hasher's IO policy:
    // config.rs defaults io_threads = cpu_threads * 3 (sd deliberately
    // OVERSUBSCRIBES IO). For the synchronous O_DIRECT tiny-file workload each
    // worker blocks on the device per read, so >cores threads keeps the device
    // queue deep — 1x-cores undersubscribed it and left throughput on the table
    // (codex-review note on 07b6907). Matching cpu*3 = fidelity with the shipping
    // engine + the band measures what sd actually does. Each worker reads
    // cold-bypassing + blake3-hashes; only the 49-byte (hash,pi,len,cold) tuple
    // survives the closure, so peak memory stays at ~io_threads files.
    let cpu_threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let io_threads = cpu_threads.saturating_mul(3).max(1);
    let io_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(io_threads)
        .thread_name(|i| format!("superdeduper-bench-io-{i}"))
        .build()
        .map_err(|e| anyhow::anyhow!("bench io pool build: {e}"))?;
    let hashed: Vec<([u8; 32], u64, u64, bool)> = io_pool.install(|| {
        candidates
            .into_par_iter()
            .map(|(pi, path)| {
                // Cancel mid-run so a long full-tier dedup isn't an
                // uninterruptible trap (checked per file — a Relaxed load is
                // free next to the read). rayon stops scheduling on first Err;
                // run() downcasts Cancelled to a clean abort, no partial submit.
                if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                    return Err(anyhow::Error::new(Cancelled));
                }
                let (data, cold) = read_uncached(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                files_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok::<_, anyhow::Error>((
                    *blake3::hash(&data).as_bytes(),
                    pi,
                    data.len() as u64,
                    cold,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()
    })?;

    // Serial fold (cheap; the cost was the reads). cold_enforced is the AND
    // across every worker — one buffered fallback taints the whole run, which
    // is load-bearing: the 125x cold/warm spread means a silent warm read on
    // the full tier would be an enormous score advantage (testdesign's
    // leverage point). result_digest is unaffected by read order: dupsets are
    // sorted below and blake3 is content-addressed.
    let mut by_hash: HashMap<[u8; 32], Vec<u64>> = HashMap::new();
    let mut candidate_bytes = 0u64;
    let mut cold_enforced = true;
    for (hash, pi, len, cold) in hashed {
        cold_enforced &= cold;
        candidate_bytes += len;
        by_hash.entry(hash).or_default().push(pi);
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
        let write = |name: &str, len: usize| -> std::path::PathBuf {
            let p = dir.join(name);
            let content: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            std::fs::write(&p, &content).unwrap();
            p
        };
        let on_ext4 = {
            // Detect whether the cache-bypass path actually engages here.
            let probe = write("probe.bin", 40960);
            let (_g, c) = read_uncached(&probe).unwrap();
            c
        };
        // Byte-correctness across sizes: aligned (40 KiB), NON-aligned (5000 -
        // benchmarker's r4 blocker: must NOT fall back to warm), and a clean
        // 1 MiB. cold must hold for every file >= 1 sector.
        for (name, len) in [("aligned.bin", 40960usize), ("nonaligned.bin", 5000), ("mib.bin", 1 << 20)] {
            let p = write(name, len);
            let want: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let (got, cold) = read_uncached(&p).unwrap();
            assert_eq!(got, want, "{name}: uncached read must be byte-identical");
            if on_ext4 {
                // bulk >= 1 sector -> cold-enforced true even for the 5000-byte
                // file (4096 cold bulk + 904 buffered tail). This is the fix:
                // non-aligned sizes no longer fall back wholesale to warm.
                assert!(cold, "{name}: must be cold-enforced (>= 1 sector) on a bypass-capable fs");
            }
        }
        // Sub-sector file: can't be cold-read by O_DIRECT/NO_BUFFERING -> honest cold=false.
        let tiny = write("tiny.bin", 100);
        let (got, cold) = read_uncached(&tiny).unwrap();
        assert_eq!(got.len(), 100, "tiny: byte-correct");
        assert!(!cold, "tiny (< 1 sector): honestly NOT cold-enforced");
        eprintln!("read_uncached cache-bypass engaged on this fs = {on_ext4}");
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

        let cancel = std::sync::atomic::AtomicBool::new(false);
        let done = std::sync::atomic::AtomicU64::new(0);
        let total = std::sync::atomic::AtomicU64::new(0);
        let (dupsets, candidate_bytes, files_enumerated, _cold) =
            full_content_dedup(&dir, &cancel, &done, &total).unwrap();
        // The progress counter saw every candidate (4 same-size files); the
        // unique-size file was never read.
        assert_eq!(total.load(std::sync::atomic::Ordering::Relaxed), 4);
        assert_eq!(done.load(std::sync::atomic::Ordering::Relaxed), 4);

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

    /// DISK-loop end-to-end reproducer for benchmarker's 295,772 cert miss.
    /// Writes the FULL tier corpus to a tempdir via `bench_corpus::write_corpus`
    /// (engine's OWN materializer, byte-for-byte matching `content_bytes_at`
    /// by construction), then runs the production `full_content_dedup` against
    /// it. This exercises every disk-IO path the real bench takes — including
    /// `read_uncached` — but on a corpus that CANNOT be off-by-one by
    /// materialization (it's our own writer's output).
    ///
    /// Expected: 295,773 dup groups. If it returns 295,772, the bug reproduces
    /// in our own write_corpus + full_content_dedup loop and the parallel
    /// Pass-2 + read_uncached interaction is the cause. On WSL the
    /// `cold_bypass_reliable()` fail-closed forces the buffered fallback path,
    /// so this exercises buffered+parallel; on Windows native NTFS it exercises
    /// FILE_FLAG_NO_BUFFERING+parallel — running on both isolates which backend.
    ///
    /// HEAVY: writes ~6.25 GB to disk + reads it back. 1M tiny files stresses
    /// NTFS/ext4 directory limits. ~10-30 min depending on disk. #[ignore]'d.
    /// Run with: cargo test --release --features 'telemetry similar-images
    /// similar-audio' --lib full_tier_engine_self_loop_disk -- --ignored --nocapture.
    /// Override target dir via SD_BENCH_DISKLOOP_DIR.
    #[test]
    #[ignore = "writes ~6.25 GB; run only post-bench-done to avoid contending the cold timing"]
    fn full_tier_engine_self_loop_disk_yields_295773_dup_groups() {
        use super::super::bench;
        use super::super::bench_corpus::{full_tier, plan_corpus, write_corpus};

        let seed = [0x42u8; 32];
        let (k_content, _) = bench::corpus_keys(&seed);
        let plan = plan_corpus(&full_tier());

        let dir = match std::env::var("SD_BENCH_DISKLOOP_DIR") {
            Ok(d) => std::path::PathBuf::from(d),
            Err(_) => std::env::temp_dir()
                .join(format!("sd-bench-diskloop-{}", uuid::Uuid::new_v4())),
        };
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("[diskloop] writing corpus to {}", dir.display());
        let t0 = std::time::Instant::now();
        let written = write_corpus(&dir, &k_content, &plan).expect("write_corpus");
        eprintln!(
            "[diskloop] wrote {} bytes in {:.2}s",
            written,
            t0.elapsed().as_secs_f64()
        );
        assert_eq!(written, plan.total_bytes, "bytes written == planned total");

        // Production parallel Pass-2 against the materialized engine-written corpus.
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let done = std::sync::atomic::AtomicU64::new(0);
        let total = std::sync::atomic::AtomicU64::new(0);
        let t1 = std::time::Instant::now();
        let (dupsets, _bytes, files, _cold) =
            full_content_dedup(&dir, &cancel, &done, &total).expect("full_content_dedup");
        eprintln!(
            "[diskloop] deduped {} files, {} dup groups in {:.2}s",
            files,
            dupsets.len(),
            t1.elapsed().as_secs_f64()
        );

        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            dupsets.len(),
            295_773,
            "production full_content_dedup against engine-written corpus must yield 295,773"
        );
    }

    #[test]
    fn debug_dedup_diff_reports_no_divergence_on_known_correct_corpus() {
        // Self-validates the diff helper: on a corpus the engine wrote itself
        // (byte-correct by construction), all three reads must agree on every
        // candidate -> zero diffs. Uses sample_tier (35 MB, ~1k files) so the
        // test is fast.
        use super::super::bench;
        use super::super::bench_corpus::{plan_corpus, sample_tier, write_corpus};

        let seed = [0x33u8; 32];
        let (k_content, _) = bench::corpus_keys(&seed);
        let plan = plan_corpus(&sample_tier());
        let dir = std::env::temp_dir().join(format!("sd-diff-self-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        write_corpus(&dir, &k_content, &plan).expect("write corpus");

        let report = debug_dedup_diff(&dir).expect("debug_dedup_diff");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            report.diffs.len(),
            0,
            "no per-file divergence on self-written corpus (got {} diffs)",
            report.diffs.len()
        );
        assert_eq!(
            report.parallel_dup_groups, report.serial_dup_groups,
            "parallel and serial agree on dup-group count for self-written corpus"
        );
        assert_eq!(
            report.serial_dup_groups, report.buffered_dup_groups,
            "serial and buffered agree on dup-group count for self-written corpus"
        );
    }

    #[test]
    fn dedup_aborts_when_cancel_preset() {
        let dir = std::env::temp_dir().join(format!("sd-bench-canceltest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        write_file(&dir, 1, &[0xAA; 100]);
        write_file(&dir, 2, &[0xAA; 100]);

        // Cancel already set -> the first candidate read short-circuits to a
        // Cancelled error rather than completing the dedup.
        let cancel = std::sync::atomic::AtomicBool::new(true);
        let done = std::sync::atomic::AtomicU64::new(0);
        let total = std::sync::atomic::AtomicU64::new(0);
        let err = full_content_dedup(&dir, &cancel, &done, &total)
            .expect_err("preset cancel must abort the dedup");
        assert!(
            err.downcast_ref::<Cancelled>().is_some(),
            "expected Cancelled, got: {err:#}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
