//! Engine-side stub for leaderboard + telemetry submissions.
//!
//! The backend at `https://api.superdeduper.com` is built by the
//! **superdeduper-backend** agent and the UI is built by the
//! **superdeduper-website** agent. This module is the engine's
//! contract for what data goes in submissions and how it's signed.
//!
//! See `docs/leaderboard-spec.md` and `docs/preflight-spec.md` for the
//! full design. This file ships intentionally non-functional (no-op
//! submit) until the backend lands, but the shapes here are the wire
//! format the backend will receive.

use std::path::PathBuf;

use serde::Serialize;

/// The full submission body for the leaderboard endpoint.
/// `POST https://api.superdeduper.com/v1/leaderboard-submit`
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardSubmission {
    pub schema: &'static str,
    pub run_uuid: String,
    pub sd_version: String,
    pub sd_build_hash: String,
    pub timestamp_unix: i64,

    pub hardware: HardwareInfo,
    pub workload: WorkloadShape,
    pub results: ScanResults,
    pub attestation: AttestationFields,
    pub anti_cheat: AntiCheatBlob,
}

#[derive(Debug, Clone, Serialize)]
pub struct HardwareInfo {
    pub cpu_model: String,
    pub cpu_threads: usize,
    /// Backend-defined coarse bucket — `x86_64-modern-high`,
    /// `x86_64-modern-mid`, `x86_64-legacy`, `arm64-modern`, etc.
    pub cpu_class: String,
    pub ram_gb_tier: u32,
    pub drive_class: String,
    pub drive_seq_read_mbps_observed: f64,
    pub os: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkloadShape {
    /// `<1k`, `1k-10k`, `10k-100k`, `100k-1M`, `>1M`.
    pub file_count_bucket: String,
    /// `<1GB`, `1-10GB`, `10-100GB`, `100-1000GB`, `>1TB`.
    pub total_size_bucket_gb: String,
    /// `<10KB`, `10-100KB`, …, `>100MB`.
    pub avg_file_size_bucket: String,
    /// `0-10%`, `10-25%`, `25-50%`, `50-100%`.
    pub dup_density_pct_bucket: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScanResults {
    pub wall_clock_ms: u64,
    pub bytes_hashed: u64,
    pub peak_rss_mb: u64,
    pub reclaimable_inode_bytes: u64,
    pub dup_groups: u32,
    pub hash_algo: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AttestationFields {
    pub defender_rtp_state_pre: Option<bool>,
    pub defender_rtp_state_post: Option<bool>,
    /// One of `"purged"`, `"unknown"`, `"warm"`.
    pub cache_state_pre: String,
    /// SHA-256 of the preflight diagnose JSON, when one was run.
    pub preflight_report_hash: Option<String>,
    /// Hash of the workload's file-size distribution (no paths or
    /// content). Two users scanning the same canonical corpus produce
    /// the same value here.
    pub corpus_signature_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AntiCheatBlob {
    /// Base64 of an engine-signed attestation. The backend verifies
    /// against the release public key list. Key rotation per release;
    /// any modified binary won't have a valid signing key.
    pub engine_attestation_blob: String,
    /// SHA-256 of internal diagnostic counters (bytes_read per tier,
    /// elapsed_ms per stage, etc.). Used as a cross-check against the
    /// declared `wall_clock_ms` / `bytes_hashed`.
    pub scan_log_proof_hash: String,
}

/// Preflight submission — separate, opt-in, anonymous. No auth.
/// `POST https://api.superdeduper.com/v1/preflight-submit`
#[derive(Debug, Clone, Serialize)]
pub struct PreflightSubmission {
    pub schema: &'static str,
    pub timestamp_unix: i64,
    /// Stable irreversible hash of (cpu_model, cpu_threads, ram_gb,
    /// drive_class, drive_serial_hash). Does NOT identify the user.
    pub machine_identity_hash: String,
    pub diagnose_report_json: serde_json::Value,
    /// Bucketed workload shape (same as LeaderboardSubmission's).
    pub workload: WorkloadShape,
    pub measured_wall_clock_ms: u64,
}

/// Inputs to `build_submission`. Holds the engine state that the
/// caller (GUI or CLI flag) wants to turn into a wire payload.
pub struct SubmissionContext {
    pub run_uuid: String,
    pub sd_version: String,
    pub sd_build_hash: String,
    pub hardware: HardwareInfo,
    pub workload: WorkloadShape,
    pub results: ScanResults,
    pub attestation: AttestationFields,
}

/// Build a leaderboard payload from a completed scan. Caller supplies
/// everything; this function just packages it into the wire format
/// and includes the anti-cheat blob.
pub fn build_submission(ctx: SubmissionContext) -> LeaderboardSubmission {
    let anti_cheat = sign_attestation(&ctx);
    LeaderboardSubmission {
        schema: "superdeduper.leaderboard-submit.v1",
        run_uuid: ctx.run_uuid,
        sd_version: ctx.sd_version,
        sd_build_hash: ctx.sd_build_hash,
        timestamp_unix: now_unix(),
        hardware: ctx.hardware,
        workload: ctx.workload,
        results: ctx.results,
        attestation: ctx.attestation,
        anti_cheat,
    }
}

/// Stub: produces a deterministic but unverifiable blob.
///
/// Real implementation pulls the release signing key (ed25519
/// recommended; small and fast) baked into the binary at build time,
/// signs a tuple of `(run_uuid, wall_clock_ms, corpus_signature_hash,
/// defender_state_pre, preflight_report_hash)` with it, base64-encodes
/// the signature.
///
/// Backend verifies with the matching public key, also baked into the
/// release list it knows about. New release → new key; old releases'
/// submissions stay verifiable.
pub fn sign_attestation(_ctx: &SubmissionContext) -> AntiCheatBlob {
    // TODO(post-backend-ship): real ed25519 signing here.
    AntiCheatBlob {
        engine_attestation_blob: "STUB_NOT_FOR_PRODUCTION".to_string(),
        scan_log_proof_hash: "STUB_NOT_FOR_PRODUCTION".to_string(),
    }
}

/// POST to the backend. No-op stub. Real implementation:
/// 1. Send via `ureq` (already a transitive dependency for some
///    crates; check before adding a direct dep).
/// 2. Handle 4xx (validation rejection → log + return error) and 5xx
///    (transient → retry with backoff).
/// 3. Cache failed submissions to disk; retry next launch.
#[allow(dead_code)]
pub fn submit(_submission: LeaderboardSubmission) -> std::io::Result<()> {
    // TODO(post-backend-ship): real HTTP submission here.
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "telemetry submission stub — backend not yet implemented",
    ))
}

/// Auth token storage. The GUI's Google OAuth flow obtains a JWT and
/// hands it here for persistence. Same storage location for CLI use
/// (future `--submit-leaderboard` flag reads this).
#[allow(dead_code)]
pub fn token_path() -> Option<PathBuf> {
    // %LOCALAPPDATA%\superdeduper\auth.json (Windows)
    // $XDG_DATA_HOME/superdeduper/auth.json    (Linux)
    // ~/Library/Application Support/superdeduper/auth.json (macOS)
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(|s| PathBuf::from(s).join("superdeduper").join("auth.json"))
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME").map(|s| {
            PathBuf::from(s)
                .join("Library")
                .join("Application Support")
                .join("superdeduper")
                .join("auth.json")
        })
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(|s| PathBuf::from(s).join("superdeduper").join("auth.json"))
            .or_else(|| {
                std::env::var_os("HOME").map(|s| {
                    PathBuf::from(s)
                        .join(".local")
                        .join("share")
                        .join("superdeduper")
                        .join("auth.json")
                })
            })
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================
// Bucket helpers — pure functions that map raw scan values into
// the leaderboard spec's coarse buckets. The web agent uses the
// same string vocabulary on the backend; bumping a bucket
// definition is a versioned schema change.
// ============================================================

pub fn bucket_file_count(n: u64) -> &'static str {
    match n {
        0..=999 => "<1k",
        1_000..=9_999 => "1k-10k",
        10_000..=99_999 => "10k-100k",
        100_000..=999_999 => "100k-1M",
        _ => ">1M",
    }
}

pub fn bucket_total_size_gb(total_bytes: u64) -> &'static str {
    let gb = total_bytes / (1024 * 1024 * 1024);
    match gb {
        0 => "<1GB",
        1..=9 => "1-10GB",
        10..=99 => "10-100GB",
        100..=999 => "100-1000GB",
        _ => ">1TB",
    }
}

pub fn bucket_avg_file_size(avg_bytes: u64) -> &'static str {
    let kb = avg_bytes / 1024;
    match kb {
        0..=9 => "<10KB",
        10..=99 => "10-100KB",
        100..=999 => "100KB-1MB",
        1_000..=9_999 => "1MB-10MB",
        10_000..=99_999 => "10MB-100MB",
        _ => ">100MB",
    }
}

pub fn bucket_dup_density(pct: f64) -> &'static str {
    let p = pct.clamp(0.0, 100.0);
    if p < 10.0 {
        "0-10%"
    } else if p < 25.0 {
        "10-25%"
    } else if p < 50.0 {
        "25-50%"
    } else {
        "50-100%"
    }
}

pub fn bucket_ram_tier_gb(gb: u32) -> u32 {
    match gb {
        0..=16 => 16,
        17..=32 => 32,
        33..=64 => 64,
        _ => 128,
    }
}

/// Coarse CPU classification from the model string + thread count.
/// Backend bucket: x86_64-modern-high / x86_64-modern-mid /
/// x86_64-legacy / x86_64-low / arm64-modern. Heuristic only —
/// the canonical list is versioned + maintained on the backend;
/// the engine ships a best-effort lookup based on the patterns
/// most likely to appear in `cpu_model` on Windows.
pub fn classify_cpu(model: &str, threads: usize) -> &'static str {
    let m = model.to_ascii_lowercase();
    // ARM (Apple M-series, Snapdragon Elite).
    if m.contains("apple m") || m.contains("snapdragon") || m.contains("aarch64") {
        return "arm64-modern";
    }
    // Modern high — Ryzen 9, i9 11th+, Threadripper, Xeon W modern.
    let modern_high = m.contains("ryzen 9")
        || m.contains("threadripper")
        || m.contains("xeon w")
        || (m.contains("i9") && (m.contains("11") || m.contains("12") || m.contains("13") || m.contains("14")));
    if modern_high && threads >= 16 {
        return "x86_64-modern-high";
    }
    // Modern mid — Ryzen 5/7, i5/i7 10th-14th.
    let modern_mid = m.contains("ryzen 5")
        || m.contains("ryzen 7")
        || (m.contains("i5") && threads >= 8)
        || (m.contains("i7") && threads >= 8);
    if modern_mid {
        return "x86_64-modern-mid";
    }
    // Low-end — Celeron / Atom / Pentium / N-series.
    if m.contains("celeron") || m.contains("atom") || m.contains("pentium") {
        return "x86_64-low";
    }
    // Default = legacy.
    "x86_64-legacy"
}

/// Compute a stable per-corpus signature: a BLAKE3 hash of the
/// sorted file-size-bucket histogram. Two users scanning the same
/// corpus produce the same signature (modulo skipped files); useful
/// to detect "user is benching the canonical superdeduper test
/// corpus" vs random data. Path-free + content-free.
pub fn corpus_signature_hash(sizes: &[u64]) -> String {
    // Histogram by avg-file-size bucket (same vocabulary as
    // bucket_avg_file_size).
    let mut counts: hashbrown::HashMap<&'static str, u64> = hashbrown::HashMap::new();
    for &s in sizes {
        *counts.entry(bucket_avg_file_size(s)).or_insert(0) += 1;
    }
    let mut entries: Vec<(&'static str, u64)> = counts.into_iter().collect();
    entries.sort_by_key(|(k, _)| *k);
    let mut hasher = blake3::Hasher::new();
    for (bucket, count) in entries {
        hasher.update(bucket.as_bytes());
        hasher.update(b":");
        hasher.update(&count.to_le_bytes());
        hasher.update(b"\n");
    }
    format!("sha256:{}", hasher.finalize().to_hex())
}

/// Generate a v4-style 128-bit identifier from system entropy. The
/// leaderboard's idempotency key — only needs to be unique per
/// scan-on-this-machine, not cryptographically unguessable.
pub fn new_run_uuid() -> String {
    // 128 bits of time + xorshift jitter. Not RFC-4122 compliant
    // (no version/variant bits); the backend just treats it as an
    // opaque idempotency key.
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut seed = now_nanos ^ 0x9E37_79B9_7F4A_7C15;
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        bytes[i] = (seed >> 56) as u8;
    }
    let h = bytes;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7], h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_count_buckets_cover_boundaries() {
        assert_eq!(bucket_file_count(0), "<1k");
        assert_eq!(bucket_file_count(999), "<1k");
        assert_eq!(bucket_file_count(1_000), "1k-10k");
        assert_eq!(bucket_file_count(10_000), "10k-100k");
        assert_eq!(bucket_file_count(100_000), "100k-1M");
        assert_eq!(bucket_file_count(1_000_000), ">1M");
    }

    #[test]
    fn size_buckets_cover_boundaries() {
        assert_eq!(bucket_total_size_gb(0), "<1GB");
        assert_eq!(bucket_total_size_gb(2 * 1024 * 1024 * 1024), "1-10GB");
        assert_eq!(bucket_total_size_gb(50 * 1024 * 1024 * 1024), "10-100GB");
        assert_eq!(bucket_total_size_gb(2 * 1024_u64.pow(4)), ">1TB");
    }

    #[test]
    fn cpu_classes_route_correctly() {
        assert_eq!(classify_cpu("AMD Ryzen 9 9950X3D", 32), "x86_64-modern-high");
        assert_eq!(classify_cpu("Intel Core i9-13900K", 24), "x86_64-modern-high");
        assert_eq!(classify_cpu("AMD Ryzen 7 5800X", 16), "x86_64-modern-mid");
        assert_eq!(classify_cpu("Intel Core i5-12400", 12), "x86_64-modern-mid");
        assert_eq!(classify_cpu("Intel Celeron N4020", 2), "x86_64-low");
        assert_eq!(classify_cpu("Apple M3 Pro", 12), "arm64-modern");
        assert_eq!(classify_cpu("Some Old Xeon", 4), "x86_64-legacy");
    }

    #[test]
    fn corpus_signature_is_deterministic_and_size_only() {
        let sizes_a: Vec<u64> = vec![1024, 1024, 5_000_000, 1024];
        let sizes_b: Vec<u64> = vec![5_000_000, 1024, 1024, 1024];
        // Same files, different order → same hash.
        assert_eq!(corpus_signature_hash(&sizes_a), corpus_signature_hash(&sizes_b));
        // Different sizes → different hash.
        let sizes_c: Vec<u64> = vec![1024, 1024, 1024, 1024];
        assert_ne!(corpus_signature_hash(&sizes_a), corpus_signature_hash(&sizes_c));
    }

    #[test]
    fn run_uuid_is_unique_per_call() {
        let a = new_run_uuid();
        let b = new_run_uuid();
        assert_ne!(a, b, "two calls must produce different uuids");
        // Format: 8-4-4-4-12 hex chars.
        assert_eq!(a.len(), 36);
        assert_eq!(a.chars().filter(|&c| c == '-').count(), 4);
    }
}
