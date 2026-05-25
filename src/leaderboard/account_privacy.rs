//! #67 — Account-level privacy flag state + API client.
//!
//! Six toggles per design 2026-05-25T17:23Z; all default OFF.
//! Engine renders the toggles in Settings → Privacy; web's
//! profile-page renderer respects the flags when deciding what
//! identifiable info to surface on the public profile.
//!
//! ## Wire shape
//!
//! GET `{server_url}/api/v1/account/me`
//!
//! Response (relevant subset):
//! ```json
//! {
//!   "privacy": {
//!     "show_display_name": false,
//!     "show_provider": false,
//!     "show_avatar": false,
//!     "show_install_breakdown": false,
//!     "show_hardware_history": false,
//!     "show_recent_runs": false
//!   }
//! }
//! ```
//!
//! PATCH `{server_url}/api/v1/account/privacy`
//! X-Sd-Signature: <hmac>
//! Content-Type: application/json
//!
//! Body: the same 6-flag object (subset accepted; missing keys
//! leave the server-side value unchanged).
//!
//! Response: the canonical 6-flag object the server stored.
//!
//! ## Failure modes
//!
//! * Unregistered install (no install_key) → can't fetch / can't patch
//! * Network → Transient
//! * 401 → install changed (Reset) or token rotated
//! * 4xx → schema rejected (engine bug; surface verbatim)

#![cfg(feature = "telemetry")]

use serde::{Deserialize, Serialize};

use super::install::InstallState;

/// Six privacy toggles. All default false. Wire shape matches
/// web's `accounts` table schema.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyFlags {
    #[serde(default)]
    pub show_display_name: bool,
    #[serde(default)]
    pub show_provider: bool,
    #[serde(default)]
    pub show_avatar: bool,
    #[serde(default)]
    pub show_install_breakdown: bool,
    #[serde(default)]
    pub show_hardware_history: bool,
    #[serde(default)]
    pub show_recent_runs: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrivacyOutcome {
    /// Server accepted + returned the canonical state.
    Ok(PrivacyFlags),
    /// HTTP 401. Install changed / install_id rotated; user should
    /// re-register or re-link.
    Unauthorised(String),
    /// HTTP 4xx (non-401). Engine schema bug; surface verbatim.
    Rejected(String),
    /// 5xx / network failure. Caller retries later.
    Transient(String),
}

/// `GET /api/v1/account/me` and pull just the `privacy` block.
/// Returns Default::default() (all OFF) when no `privacy` field is
/// present so older server responses degrade gracefully.
pub fn fetch(state: &InstallState, server_url: &str) -> PrivacyOutcome {
    if !state.registered {
        return PrivacyOutcome::Unauthorised(
            "install not registered — call `superdeduper register` first".to_string(),
        );
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return PrivacyOutcome::Rejected("install_key_hex malformed".to_string());
        }
    };
    let url = format!("{}/api/v1/account/me", server_url.trim_end_matches('/'));
    // GET still needs the install signature since this is the
    // account-private view (not the public profile).
    let canonical_marker = serde_json::json!({"install_id": state.install_id});
    let body = super::hmac_signer::canonical_body(&canonical_marker);
    let signature = super::hmac_signer::sign(&install_key, &body);
    let response = ureq::get(&url)
        .set("X-Sd-Install-Id", &state.install_id)
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(10))
        .call();
    match response {
        Ok(resp) => parse_response(resp),
        Err(ureq::Error::Status(401, resp)) => {
            PrivacyOutcome::Unauthorised(resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            PrivacyOutcome::Rejected(format!(
                "{code}: {}",
                resp.into_string().unwrap_or_default()
            ))
        }
        Err(ureq::Error::Status(code, resp)) => PrivacyOutcome::Transient(format!(
            "{code}: {}",
            resp.into_string().unwrap_or_default()
        )),
        Err(ureq::Error::Transport(t)) => PrivacyOutcome::Transient(format!("transport: {t}")),
    }
}

/// `PATCH /api/v1/account/privacy` with the supplied flags.
pub fn update(state: &InstallState, server_url: &str, flags: &PrivacyFlags) -> PrivacyOutcome {
    if !state.registered {
        return PrivacyOutcome::Unauthorised(
            "install not registered — call `superdeduper register` first".to_string(),
        );
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return PrivacyOutcome::Rejected("install_key_hex malformed".to_string());
        }
    };
    // Include install_id in the canonical body so the server can
    // route the update to the right account.
    let payload = serde_json::json!({
        "install_id": state.install_id,
        "flags": flags,
    });
    let body = super::hmac_signer::canonical_body(&payload);
    let signature = super::hmac_signer::sign(&install_key, &body);
    let url = format!(
        "{}/api/v1/account/privacy",
        server_url.trim_end_matches('/')
    );
    // ureq doesn't have an explicit `.patch()` helper; use the
    // generic `.request(method, url)` path.
    let response = ureq::request("PATCH", &url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(10))
        .send_bytes(&body);
    match response {
        Ok(resp) => parse_response(resp),
        Err(ureq::Error::Status(401, resp)) => {
            PrivacyOutcome::Unauthorised(resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            PrivacyOutcome::Rejected(format!(
                "{code}: {}",
                resp.into_string().unwrap_or_default()
            ))
        }
        Err(ureq::Error::Status(code, resp)) => PrivacyOutcome::Transient(format!(
            "{code}: {}",
            resp.into_string().unwrap_or_default()
        )),
        Err(ureq::Error::Transport(t)) => PrivacyOutcome::Transient(format!("transport: {t}")),
    }
}

fn parse_response(resp: ureq::Response) -> PrivacyOutcome {
    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return PrivacyOutcome::Transient(format!("200 OK but body parse failed: {e}"));
        }
    };
    // Accept either the bare PrivacyFlags shape (PATCH response)
    // or a wrapped {privacy: PrivacyFlags} (GET /me response).
    let flags_value = body.get("privacy").unwrap_or(&body).clone();
    match serde_json::from_value::<PrivacyFlags>(flags_value) {
        Ok(flags) => PrivacyOutcome::Ok(flags),
        Err(e) => PrivacyOutcome::Transient(format!("flags parse failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privacy_flags_default_all_off() {
        let f = PrivacyFlags::default();
        assert!(!f.show_display_name);
        assert!(!f.show_provider);
        assert!(!f.show_avatar);
        assert!(!f.show_install_breakdown);
        assert!(!f.show_hardware_history);
        assert!(!f.show_recent_runs);
    }

    #[test]
    fn privacy_flags_round_trip_json() {
        let f = PrivacyFlags {
            show_display_name: true,
            show_avatar: true,
            show_hardware_history: false,
            ..Default::default()
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: PrivacyFlags = serde_json::from_str(&s).unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn privacy_flags_tolerate_missing_keys() {
        // Older server may not yet ship the two new fields;
        // engine should still parse the response cleanly.
        let s = r#"{"show_display_name": true, "show_provider": false}"#;
        let f: PrivacyFlags = serde_json::from_str(s).unwrap();
        assert!(f.show_display_name);
        assert!(!f.show_provider);
        assert!(!f.show_avatar);
        assert!(!f.show_install_breakdown);
        assert!(!f.show_hardware_history);
        assert!(!f.show_recent_runs);
    }
}
