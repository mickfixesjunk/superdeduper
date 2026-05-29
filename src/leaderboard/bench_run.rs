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

    // 3. real full-content dedupe (reads EVERY byte: ranked wall + I/O signal)
    check_cancel()?;
    progress("deduping (full-content)");
    let t = std::time::Instant::now();
    let (dupsets, bytes_scanned, files_scanned) = full_content_dedup(&corpus_dir)?;
    let dedupe_secs = t.elapsed().as_secs_f64();
    progress(&format!("deduped {dedupe_secs:.2}s: {files_scanned} files, {} dup groups", dupsets.len()));

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

/// Full-content exact-dedupe: read EVERY file fully (bytes_scanned == corpus
/// total, the server's I/O cross-check), BLAKE3-group byte-identical files,
/// return canonical dup sets (path_index lists sorted within + across), total
/// bytes, and file count.
fn full_content_dedup(dir: &Path) -> anyhow::Result<(Vec<Vec<u64>>, u64, u64)> {
    let mut by_hash: HashMap<[u8; 32], Vec<u64>> = HashMap::new();
    let mut bytes = 0u64;
    let mut count = 0u64;
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
        let data = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        bytes += data.len() as u64;
        count += 1;
        by_hash.entry(*blake3::hash(&data).as_bytes()).or_default().push(pi);
    }
    let mut dupsets: Vec<Vec<u64>> = by_hash
        .into_values()
        .filter(|v| v.len() >= 2)
        .map(|mut v| {
            v.sort_unstable();
            v
        })
        .collect();
    dupsets.sort();
    Ok((dupsets, bytes, count))
}
