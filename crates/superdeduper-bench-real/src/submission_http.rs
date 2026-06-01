//! Submission HTTP path: pure helpers + the two HTTP entry points.
//!
//! Phase 2-B v0.3.20 (2026-06-01): moved from
//! `src/leaderboard/submission.rs` to this crate. The InstallState-coupled
//! engine `submit()` / `submit_recorded_payload()` API stays in engine as
//! a thin wrapper around the primitives-taking `submit_inner()` /
//! `submit_recorded_payload_inner()` below. The disk-queue + archive
//! state machinery (queue_dir / archive_dir / review_dir / enqueue /
//! store_pending / peek_pending / take_pending) STAYS engine-side --
//! those touch the engine config dir on disk and have no place in
//! bench-real.
//!
//! Pure helpers (build_payload, parse_ok, parse_error,
//! duplicate_submission_id_from_409, effective_lane, now_iso8601)
//! moved with their consumer; engine re-exports build_payload +
//! now_iso8601 for the engine-side callers that build payloads for the
//! disk queue / archive paths.

use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

use superdeduper_bench_iface::{RankEntry, SubmissionInputs, SubmitOutcome};

// --------------------------------------------------------- time helpers

/// Howard Hinnant's civil-from-days algorithm -- converts unix-epoch
/// days (signed, can be negative) to `(year, month, day)`. Inlined from
/// the engine `crate::time` module to keep bench-real free of any
/// engine-only dep.
fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d)
}

fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let h = (sod / 3_600) as u32;
    let mi = ((sod / 60) % 60) as u32;
    let s = (sod % 60) as u32;
    let (y, mo, d) = days_to_ymd(days);
    (y, mo, d, h, mi, s)
}

/// ISO-8601 timestamp `YYYY-MM-DDTHH:MM:SSZ` (UTC, `Z` suffix). Matches
/// the leaderboard backend's `timestamp` schema.
pub fn now_iso8601() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

// --------------------------------------------------------- payload builder

/// Build the canonical /submit body from the inputs.
///
/// Mirrors the backend schema's required top-level keys. T-BENCH-ME:
/// when `inputs.bench` is `Some`, the canonical-bench work-proof +
/// version-binding fields lift to the top level (only for
/// scope=canonical-bench; ordinary scans omit them entirely, preserving
/// the v1 schema's top-level key set). Mick bench-lane UX: the user's
/// effective lane (after cold-enforce override) goes top-level.
pub fn build_payload(inputs: &SubmissionInputs, install_id: &str) -> Value {
    let mut body = serde_json::json!({
        "schema_version": "v1",
        "client_version": inputs.client_version,
        "install_id": install_id,
        "timestamp": now_iso8601(),
        "hardware": inputs.hardware,
        "run_shape": inputs.run_shape,
        "result_summary": inputs.result_summary,
    });
    if let Some(bench) = &inputs.bench {
        let obj = body.as_object_mut().expect("json object");
        obj.insert("protocol_version".into(), bench.protocol_version.clone().into());
        obj.insert("corpus_version".into(), bench.corpus_version.clone().into());
        obj.insert("tier".into(), bench.tier.clone().into());
        obj.insert("bench_run_id".into(), bench.bench_run_id.clone().into());
        obj.insert("bench_proof".into(), bench.bench_proof.clone());
        obj.insert("cold_enforced".into(), bench.cold_enforced.into());
    }
    if let Some(lane) = effective_lane(inputs) {
        body.as_object_mut()
            .expect("json object")
            .insert("lane".into(), lane.into());
    }
    body
}

/// Phase B.5 option (b) -- Mick GO 2026-05-31. Derive the effective
/// lane for the wire body, applying the cold-enforce override: when
/// this submission carries a bench block with `cold_enforced=false`,
/// the lane is FORCED to `"casual"` regardless of `inputs.lane`.
/// Otherwise the caller's lane (or `None`) is preserved.
fn effective_lane(inputs: &SubmissionInputs) -> Option<String> {
    let is_warm_bench = inputs
        .bench
        .as_ref()
        .map(|b| !b.cold_enforced)
        .unwrap_or(false);
    if is_warm_bench {
        return Some("casual".to_string());
    }
    inputs.lane.clone()
}

// --------------------------------------------------------- response parsing

fn parse_ok(resp: ureq::Response) -> SubmitOutcome {
    let body: Value = match resp.into_json() {
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

/// #99/v0.2.37 -- extract the existing submission_id from a 409
/// (duplicate_submission) response body. Web contract A: the field is
/// `submission_id` (same key as the 200 path), omitted if unresolvable.
fn duplicate_submission_id_from_409(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .as_ref()
        .and_then(|v| v.get("submission_id"))
        .and_then(|s| s.as_str())
        .map(String::from)
}

fn parse_error(code: u16, resp: ureq::Response) -> SubmitOutcome {
    let body_text = resp.into_string().unwrap_or_default();
    if code == 409 {
        return SubmitOutcome::DuplicateNoChange {
            submission_id: duplicate_submission_id_from_409(&body_text),
        };
    }
    if (500..600).contains(&code) {
        return SubmitOutcome::Transient {
            reason: format!("{code}: {body_text}"),
        };
    }
    let reason = serde_json::from_str::<Value>(&body_text)
        .ok()
        .and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from))
        .unwrap_or(body_text);
    SubmitOutcome::Rejected {
        status: code,
        reason,
    }
}

// --------------------------------------------------------- HTTP entry points

/// Build + sign + POST a submission. Pure HTTP: no engine InstallState
/// dependency. The engine wrapper extracts `server_url` / `install_id`
/// / `install_key` from `InstallState` and delegates here.
///
/// Network errors surface as `Transient`; the caller decides whether to
/// enqueue for retry. Never panics on network failure.
pub fn submit_inner(
    server_url: &str,
    install_id: &str,
    install_key: &[u8; 32],
    inputs: &SubmissionInputs,
) -> SubmitOutcome {
    let payload = build_payload(inputs, install_id);
    let body = superdeduper_hmac_signer::canonical_body(&payload);
    let signature = superdeduper_hmac_signer::sign(install_key, &body);

    let url = format!("{}/api/v1/submit", server_url.trim_end_matches('/'));
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

/// #41 -- re-POST a previously-recorded canonical payload. Same signing
/// flow as `submit_inner` but the body comes from the stored
/// `scan_history::ScanRecord.submission_payload` rather than being
/// rebuilt from a fresh `SubmissionInputs`. Refreshes `timestamp` in
/// place before signing so the server's clock_skew sanity check still
/// passes on resubmits.
///
/// The `current_install_id` / `built_with_install_id` mismatch surfaces
/// a clear `Rejected{ status: 0, reason: "install changed since scan" }`
/// up-front rather than waiting for the server's 401 under a rotated key.
pub fn submit_recorded_payload_inner(
    server_url: &str,
    current_install_id: &str,
    install_key: &[u8; 32],
    payload: &Value,
    built_with_install_id: &str,
) -> SubmitOutcome {
    if current_install_id != built_with_install_id {
        return SubmitOutcome::Rejected {
            status: 0,
            reason: format!(
                "install changed since scan: payload was built with install_id `{}` \
                 but current install is `{}`. Reset-install rotates the HMAC key; \
                 the recorded signature no longer matches. Delete this row from \
                 History -- re-running the scan under the current install will \
                 produce a fresh submittable payload.",
                built_with_install_id, current_install_id,
            ),
        };
    }
    let mut refreshed = payload.clone();
    if let Some(obj) = refreshed.as_object_mut() {
        obj.insert(
            "timestamp".to_string(),
            Value::String(now_iso8601()),
        );
    }
    let body = superdeduper_hmac_signer::canonical_body(&refreshed);
    let signature = superdeduper_hmac_signer::sign(install_key, &body);

    let url = format!("{}/api/v1/submit", server_url.trim_end_matches('/'));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_format_matches_schema() {
        // Smoke: format is YYYY-MM-DDTHH:MM:SSZ
        let s = now_iso8601();
        assert_eq!(s.len(), 20, "expected 20-char iso8601: got {s}");
        assert!(s.ends_with('Z'));
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    #[test]
    fn unix_to_ymdhms_matches_known_vectors() {
        // 2024-01-01T00:00:00Z = 1_704_067_200
        assert_eq!(unix_to_ymdhms(1_704_067_200), (2024, 1, 1, 0, 0, 0));
        // 1970-01-01T00:00:00Z = 0
        assert_eq!(unix_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
        // 2026-05-24T03:42:33Z = 1_779_594_153
        assert_eq!(unix_to_ymdhms(1_779_594_153), (2026, 5, 24, 3, 42, 33));
    }

    #[test]
    fn duplicate_submission_id_from_409_extracts_id() {
        let body = r#"{"submission_id": "abc-123", "reason": "duplicate_submission"}"#;
        assert_eq!(
            duplicate_submission_id_from_409(body),
            Some("abc-123".to_string())
        );
    }

    #[test]
    fn duplicate_submission_id_from_409_handles_absent_field() {
        let body = r#"{"reason": "duplicate_submission"}"#;
        assert_eq!(duplicate_submission_id_from_409(body), None);
    }

    #[test]
    fn duplicate_submission_id_from_409_handles_malformed_json() {
        assert_eq!(duplicate_submission_id_from_409("not json"), None);
    }
}
