//! #77 — Cross-machine badge multiplier summary.
//!
//! For accounts with multiple linked installs, the GUI's badge wall
//! shows an "×N" overlay on any badge that was earned on N ≥ 2 of
//! the user's installs. Clicking the badge opens a per-install
//! detail panel listing which installs earned it + when.
//!
//! ## Wire shape (Option B per design 2026-05-25T18:46Z)
//!
//! GET `{server_url}/api/v1/account/{account_id}/badge-summary`
//! X-Sd-Install-Id: <install_id>
//! X-Sd-Signature: <hmac over {install_id, account_id}>
//!
//! Response:
//! ```json
//! [
//!   {
//!     "achievement_id": "storage-crusader",
//!     "count": 3,
//!     "installs": [
//!       {
//!         "install_id": "9d4a0000-...",
//!         "install_id_prefix": "9d4a",
//!         "nickname": null,
//!         "cpu_class": "x86_64",
//!         "earned_at_unix": 1779594153
//!       },
//!       ...
//!     ]
//!   },
//!   ...
//! ]
//! ```
//!
//! Engine's expected shape: each entry surfaces the achievement_id
//! plus count plus a list of installs with enough metadata to render
//! the click-detail panel.
//!
//! `nickname` is null when the user hasn't set one; engine falls
//! back to `install_id_prefix`. Archived installs are INCLUDED per
//! design's archived-install policy; engine renders them as
//! "Archived ({prefix})" in the detail panel.
//!
//! ## Stub fallback
//!
//! Web's endpoint isn't shipped yet (as of 2026-05-25). Until it
//! lands, `fetch_or_mock` returns an empty Vec on any failure
//! (404 → "endpoint not yet there"; transport error → "server
//! down or no network"; auth error → "user not signed in"). The
//! badge wall renders normally with no multipliers. Once web
//! ships, real counts flow through automatically.

#![cfg(feature = "telemetry")]

use serde::{Deserialize, Serialize};

use super::install::InstallState;

/// One row in the `/account/{id}/badge-summary` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBadgeEntry {
    pub achievement_id: String,
    pub count: u32,
    #[serde(default)]
    pub installs: Vec<AccountBadgeInstall>,
}

/// One install that earned the achievement_id from the parent
/// `AccountBadgeEntry`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBadgeInstall {
    pub install_id: String,
    /// Cached short-prefix the server emits so engine doesn't have
    /// to compute it (server-side prefix is the same shape engine
    /// would compute: first 4 hex chars of the install_id UUID).
    #[serde(default)]
    pub install_id_prefix: String,
    /// User-set nickname for this install (per-install setting,
    /// not implemented engine-side yet but reserved here).
    /// `None` ⇒ engine renders `install_id_prefix` as fallback.
    #[serde(default)]
    pub nickname: Option<String>,
    /// CPU class hint for the per-install detail panel; same
    /// hardware-bracket vocabulary the leaderboard uses
    /// (`x86_64`, `aarch64`, etc.). `None` ⇒ render no hardware
    /// hint.
    #[serde(default)]
    pub cpu_class: Option<String>,
    /// Unix-seconds when the achievement was granted on this
    /// install. Used by the click-detail panel for the "earned
    /// 2026-05-23" line.
    #[serde(default)]
    pub earned_at_unix: u64,
    /// `true` when the install is no longer active (user
    /// explicitly archived or last-seen older than the
    /// account's archive threshold). Detail panel renders
    /// "Archived ({prefix})" prefix.
    #[serde(default)]
    pub archived: bool,
}

#[derive(Debug, Clone)]
pub enum BadgeSummaryOutcome {
    Ok(Vec<AccountBadgeEntry>),
    /// Server returned 401. Install rotated / unlinked since last
    /// sign-in.
    Unauthorised(String),
    /// Server returned a non-401 4xx. Engine bug; surface verbatim.
    Rejected(String),
    /// 5xx / network failure / endpoint not deployed yet
    /// (404 falls in this bucket because the endpoint is fresh).
    /// Engine treats as "no data" + skips the multiplier overlay.
    Transient(String),
}

/// Fetch the cross-install badge summary for the account that
/// owns `install_id`. Server resolves the account from the
/// install_id + signature; engine doesn't need to know the
/// account_id directly (one less round-trip).
pub fn fetch(state: &InstallState) -> BadgeSummaryOutcome {
    if !state.registered {
        return BadgeSummaryOutcome::Unauthorised(
            "install not registered — sign in first".to_string(),
        );
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return BadgeSummaryOutcome::Rejected("install_key_hex malformed".to_string());
        }
    };
    // The server resolves the account from the install_id; engine
    // signs over `{install_id}` so the server can verify identity
    // before returning the account-level rollup.
    let canonical_marker = serde_json::json!({"install_id": state.install_id});
    let body = super::hmac_signer::canonical_body(&canonical_marker);
    let signature = super::hmac_signer::sign(&install_key, &body);
    let url = format!(
        "{}/api/v1/account/badge-summary",
        state.server_url.trim_end_matches('/')
    );
    // Send the signed body: the HMAC is computed over `body`, so the
    // server needs it to verify. `.call()` (empty body) yields a 400
    // empty_body / signature mismatch — same bug class as the
    // account_privacy::fetch Profile-Visibility 400. send_bytes keeps
    // this a GET while attaching the canonical body.
    let response = ureq::get(&url)
        .set("X-Sd-Install-Id", &state.install_id)
        .set("X-Sd-Signature", &signature)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(10))
        .send_bytes(&body);
    match response {
        Ok(resp) => match resp.into_json::<Vec<AccountBadgeEntry>>() {
            Ok(entries) => BadgeSummaryOutcome::Ok(entries),
            Err(e) => BadgeSummaryOutcome::Transient(format!("response parse failed: {e}")),
        },
        Err(ureq::Error::Status(401, resp)) => BadgeSummaryOutcome::Unauthorised(
            resp.into_string()
                .unwrap_or_else(|e| format!("<read error: {e}>")),
        ),
        Err(ureq::Error::Status(404, _)) => {
            // Endpoint not yet deployed; treat as no-data.
            BadgeSummaryOutcome::Transient(
                "404: endpoint not yet deployed (web's Option B; expected pre-merge)".to_string(),
            )
        }
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            BadgeSummaryOutcome::Rejected(format!(
                "{code}: {}",
                resp.into_string()
                    .unwrap_or_else(|e| format!("<read error: {e}>"))
            ))
        }
        Err(ureq::Error::Status(code, resp)) => BadgeSummaryOutcome::Transient(format!(
            "{code}: {}",
            resp.into_string()
                .unwrap_or_else(|e| format!("<read error: {e}>"))
        )),
        Err(ureq::Error::Transport(t)) => BadgeSummaryOutcome::Transient(format!("transport: {t}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_round_trips_json_with_optional_fields() {
        let entry = AccountBadgeEntry {
            achievement_id: "storage-crusader".to_string(),
            count: 3,
            installs: vec![
                AccountBadgeInstall {
                    install_id: "9d4a-aaaa".into(),
                    install_id_prefix: "9d4a".into(),
                    nickname: Some("Workstation".into()),
                    cpu_class: Some("x86_64".into()),
                    earned_at_unix: 1779594153,
                    archived: false,
                },
                AccountBadgeInstall {
                    install_id: "1b2c-bbbb".into(),
                    install_id_prefix: "1b2c".into(),
                    nickname: None,
                    cpu_class: None,
                    earned_at_unix: 1779000000,
                    archived: true,
                },
            ],
        };
        let s = serde_json::to_string(&entry).unwrap();
        let back: AccountBadgeEntry = serde_json::from_str(&s).unwrap();
        assert_eq!(back.achievement_id, "storage-crusader");
        assert_eq!(back.count, 3);
        assert_eq!(back.installs.len(), 2);
        assert_eq!(back.installs[0].nickname.as_deref(), Some("Workstation"));
        assert!(back.installs[1].archived);
    }

    #[test]
    fn entry_tolerates_minimal_payload() {
        // Server might omit `installs` for performance — engine
        // should still parse + render the multiplier from count.
        let s = r#"{"achievement_id": "tidy-up", "count": 2}"#;
        let entry: AccountBadgeEntry = serde_json::from_str(s).unwrap();
        assert_eq!(entry.count, 2);
        assert!(entry.installs.is_empty());
    }

    #[test]
    fn install_tolerates_missing_optional_fields() {
        // Server may omit nickname / cpu_class / earned_at when
        // the data isn't available yet.
        let s = r#"{"install_id": "abcd-1234", "install_id_prefix": "abcd"}"#;
        let install: AccountBadgeInstall = serde_json::from_str(s).unwrap();
        assert_eq!(install.install_id_prefix, "abcd");
        assert!(install.nickname.is_none());
        assert!(install.cpu_class.is_none());
        assert_eq!(install.earned_at_unix, 0);
        assert!(!install.archived);
    }
}
