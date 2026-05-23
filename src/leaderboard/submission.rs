//! Submit a completed scan's results to the leaderboard backend.
//!
//! Flow per client-spec §6:
//! 1. Build the payload from scan results + hardware fingerprint
//! 2. Canonicalize to JSON (sorted keys via [`hmac_signer::canonical_body`])
//! 3. HMAC-sign with the install_key
//! 4. POST to `/api/v1/submit` with `X-Sd-Signature` header
//! 5. On 5xx / network failure: enqueue to disk for next-launch retry
//!    (50-submission cap per spec §6.5)
//! 6. On 200: surface rank + achievements to the caller
//! 7. On 409 (`duplicate_submission`): treat as neutral "no change"
//!
//! The wire payload shape is not yet locked against the live JSON
//! schema at `https://api.superdeduper.io/api/v1/submit/schema.json` —
//! follow-up commit will regenerate the struct via `typify`. For now
//! the body is built as a `serde_json::Value` so field additions
//! don't churn a Rust type.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::hardware::HardwareFingerprint;
use super::hmac_signer;
use super::install::InstallState;

/// Inputs to the submission builder. Caller provides everything the
/// engine knows about this scan; this module wraps it into the wire
/// payload + signs + posts.
pub struct SubmissionInputs {
    pub run_uuid: String,
    pub sd_version: String,
    pub hardware: HardwareFingerprint,
    pub scan: ScanResults,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResults {
    pub files_scanned: u64,
    pub bytes_scanned: u64,
    pub wall_clock_ms: u64,
    pub duplicate_groups: u64,
    pub reclaimable_inode_bytes: u64,
    pub hash_algo: String,
    pub defender_rtp_state_pre: Option<bool>,
    pub defender_rtp_state_post: Option<bool>,
    pub corpus_signature_hash: String,
}

#[derive(Debug)]
pub enum SubmitOutcome {
    /// 200 OK — backend accepted and ranked.
    Accepted {
        submission_id: String,
        ranks: Vec<RankEntry>,
        achievements_unlocked: Vec<String>,
        profile_url: Option<String>,
    },
    /// 409 — same payload hash already on file. Neutral status.
    DuplicateNoChange,
    /// 4xx (other than 409). Caller should surface the reason; don't
    /// retry (the payload is wrong, not the network).
    Rejected { status: u16, reason: String },
    /// 5xx / network. Caller should enqueue to disk for next launch.
    /// (Persisting is done by `submit_with_queue` — see below.)
    Transient { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankEntry {
    pub category: String,
    pub bracket: String,
    pub rank: u64,
    pub bucket_size: u64,
}

/// Build the canonical JSON request body. Pure function so tests can
/// snapshot the exact wire bytes without spinning a server.
pub fn build_payload(inputs: &SubmissionInputs) -> serde_json::Value {
    serde_json::json!({
        "schema_version": "v1",
        "run_uuid": inputs.run_uuid,
        "sd_version": inputs.sd_version,
        "submitted_at_unix": now_unix(),
        "hardware": inputs.hardware,
        "scan": inputs.scan,
    })
}

/// Build + sign + POST a submission. Network errors surface as
/// `Transient`; the caller decides whether to enqueue for retry.
/// This function never panics on network failure; it returns an
/// outcome.
pub fn submit(state: &InstallState, inputs: &SubmissionInputs) -> SubmitOutcome {
    if !state.registered {
        return SubmitOutcome::Rejected {
            status: 0,
            reason: "install not registered — call `sd register` first".to_string(),
        };
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return SubmitOutcome::Rejected {
                status: 0,
                reason: "install_key_hex malformed".to_string(),
            };
        }
    };
    let payload = build_payload(inputs);
    let body = hmac_signer::canonical_body(&payload);
    let signature = hmac_signer::sign(&install_key, &body);

    let url = format!("{}/api/v1/submit", state.server_url.trim_end_matches('/'));
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&body);

    match response {
        Ok(resp) => parse_ok(resp),
        Err(ureq::Error::Status(code, resp)) => parse_error(code, resp),
        Err(ureq::Error::Transport(t)) => SubmitOutcome::Transient {
            reason: format!("transport: {t}"),
        },
    }
}

fn parse_ok(resp: ureq::Response) -> SubmitOutcome {
    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return SubmitOutcome::Transient {
                reason: format!("200 OK but body parse failed: {e}"),
            };
        }
    };
    let submission_id = body
        .get("submission_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let ranks = body
        .get("current_ranks")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|r| serde_json::from_value::<RankEntry>(r.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let achievements_unlocked = body
        .get("achievements_unlocked")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| a.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let profile_url = body
        .get("profile_url")
        .and_then(|v| v.as_str())
        .map(String::from);
    SubmitOutcome::Accepted {
        submission_id,
        ranks,
        achievements_unlocked,
        profile_url,
    }
}

fn parse_error(code: u16, resp: ureq::Response) -> SubmitOutcome {
    let body_text = resp.into_string().unwrap_or_default();
    if code == 409 {
        return SubmitOutcome::DuplicateNoChange;
    }
    if (500..600).contains(&code) {
        return SubmitOutcome::Transient {
            reason: format!("{code}: {body_text}"),
        };
    }
    let reason = serde_json::from_str::<serde_json::Value>(&body_text)
        .ok()
        .and_then(|v| {
            v.get("reason")
                .and_then(|r| r.as_str())
                .map(String::from)
        })
        .unwrap_or(body_text);
    SubmitOutcome::Rejected { status: code, reason }
}

// ============================================================
// Offline queue: 50-submission cap, drain-on-startup per §6.5.
// ============================================================

/// Directory under the install data dir where transient-failed
/// submissions are persisted.
pub fn queue_dir() -> std::io::Result<PathBuf> {
    let mut p = super::install::install_path()?;
    p.set_file_name("submission-queue");
    Ok(p)
}

/// Persist a payload + signature pair so the next launch can retry.
/// Filename includes a timestamp + the first 8 hex chars of the
/// payload hash for de-dup.
pub fn enqueue(inputs: &SubmissionInputs, signature: &str) -> std::io::Result<PathBuf> {
    let dir = queue_dir()?;
    std::fs::create_dir_all(&dir)?;
    // Cap at 50 entries — oldest gets evicted first.
    prune_queue(&dir, 50)?;
    let payload = build_payload(inputs);
    let body = hmac_signer::canonical_body(&payload);
    let body_hash_prefix = blake3::hash(&body).to_hex();
    let filename = format!(
        "{}-{}.json",
        now_unix(),
        &body_hash_prefix.as_str()[..8]
    );
    let path = dir.join(filename);
    let stored = QueuedSubmission {
        body: String::from_utf8_lossy(&body).into_owned(),
        signature: signature.to_string(),
        enqueued_at_unix: now_unix(),
    };
    std::fs::write(&path, serde_json::to_vec_pretty(&stored)?)?;
    Ok(path)
}

/// Keep the queue at most `cap` entries by deleting the oldest.
fn prune_queue(dir: &std::path::Path, cap: usize) -> std::io::Result<()> {
    let mut entries: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)?
        .filter_map(Result::ok)
        .filter_map(|e| {
            let p = e.path();
            let m = e.metadata().ok()?.modified().ok()?;
            Some((m, p))
        })
        .collect();
    if entries.len() <= cap {
        return Ok(());
    }
    entries.sort_by_key(|(t, _)| *t);
    let drop_count = entries.len() - cap;
    for (_, p) in entries.into_iter().take(drop_count) {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct QueuedSubmission {
    body: String,
    signature: String,
    enqueued_at_unix: i64,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaderboard::hardware::HardwareFingerprint;

    fn sample_inputs() -> SubmissionInputs {
        SubmissionInputs {
            run_uuid: "9d4a0000-0000-0000-0000-000000000001".into(),
            sd_version: "0.1.7-test".into(),
            hardware: HardwareFingerprint {
                schema_version: 1,
                cpu_model_string: "Test CPU".into(),
                cpu_threads: 8,
                cpu_isa_flags: vec!["sse4_2".into(), "avx2".into()],
                ram_gb_total: Some(32),
                os_family: "linux".into(),
                os_edition: None,
            },
            scan: ScanResults {
                files_scanned: 1234,
                bytes_scanned: 9_876_543,
                wall_clock_ms: 5678,
                duplicate_groups: 42,
                reclaimable_inode_bytes: 12345,
                hash_algo: "river5-test".into(),
                defender_rtp_state_pre: Some(true),
                defender_rtp_state_post: Some(true),
                corpus_signature_hash: "sha256:deadbeef".into(),
            },
        }
    }

    #[test]
    fn build_payload_contains_required_keys() {
        let p = build_payload(&sample_inputs());
        assert!(p.get("schema_version").is_some());
        assert!(p.get("run_uuid").is_some());
        assert!(p.get("hardware").is_some());
        assert!(p.get("scan").is_some());
    }

    #[test]
    fn canonical_body_is_deterministic_across_inputs() {
        let p1 = build_payload(&sample_inputs());
        let p2 = build_payload(&sample_inputs());
        let b1 = hmac_signer::canonical_body(&p1);
        let b2 = hmac_signer::canonical_body(&p2);
        assert_eq!(b1, b2);
        // Sorted keys at the top level. Note `schema_version` also
        // appears nested inside `hardware`; use rfind() for the outer
        // one which appears AFTER `scan` in the canonical output.
        let s = String::from_utf8(b1).unwrap();
        let idx_outer_schema = s.rfind("\"schema_version\"").unwrap();
        let idx_scan = s.find("\"scan\"").unwrap();
        let idx_hw = s.find("\"hardware\"").unwrap();
        let idx_run = s.find("\"run_uuid\"").unwrap();
        assert!(idx_hw < idx_run);
        assert!(idx_run < idx_scan);
        assert!(
            idx_scan < idx_outer_schema,
            "scan should sort before outer schema_version (s-c-a-n < s-c-h): {s}"
        );
    }

    #[test]
    fn submit_rejects_when_not_registered() {
        let state = super::super::install::new_unregistered("https://example".into());
        // registered defaults to false; submit should refuse.
        let out = submit(&state, &sample_inputs());
        assert!(matches!(out, SubmitOutcome::Rejected { status: 0, .. }));
    }
}
