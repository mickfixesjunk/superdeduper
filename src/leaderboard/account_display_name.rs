//! Mick FEATURE #3 (2026-05-30) — account nickname (display_name) get/set client.
//!
//! Wires to web's POST /api/v1/account/display_name endpoint shipped at
//! 42bdef6 with WRITE-TIME BACKFILL: changing the nickname server-side
//! updates the `account_display_name` column on every existing submission
//! row tied to the account's install_ids. The engine surfaces the change
//! through CLI (`superdeduper account nickname set <name>`) and GUI
//! (Settings > Account > Nickname row + are-you-sure modal).
//!
//! ## Wire shape
//!
//! POST `{server_url}/api/v1/account/display_name`
//! X-Sd-Install-Id: <install_id>
//! X-Sd-Signature: <hmac>
//! Content-Type: application/json
//!
//! Body: `{install_id, account_id?, display_name}`
//!
//! Success 200: `{display_name, backfill: {installs_processed, rows_updated, rows_failed}}`
//! Errors: 400 validation, 401 HMAC fail, 403 not-linked/mismatch/banned,
//! 429 rate-limit (5 changes per account per 24h).
//!
//! GET `{server_url}/api/v1/profile/me` (shared with account_privacy::fetch)
//! returns `{display_name, display_name_source: "manual"|"auto", ...}` so
//! the engine can tell whether the user has personalised the nickname or
//! is still on the auto-generated `user-XXXXXXXX` fallback.

#![cfg(feature = "telemetry")]

use super::install::InstallState;

/// Hard length cap shared with web's validator (1-32 chars).
pub const NICKNAME_MIN_LEN: usize = 1;
pub const NICKNAME_MAX_LEN: usize = 32;

/// Client-side validation mirroring the server's allowlist so the user gets
/// fast feedback before the round-trip. Charset: `a-z A-Z 0-9 _ - . '` plus
/// space; reject anything else (emoji, control chars, zero-width, slashes).
/// Reject the reserved `user-XXXXXXXX` anonymized-fallback shape so the user
/// can't claim it as their personal name.
pub fn validate_nickname(name: &str) -> Result<(), String> {
    let len = name.chars().count();
    if len < NICKNAME_MIN_LEN {
        return Err("Nickname can't be empty.".to_string());
    }
    if len > NICKNAME_MAX_LEN {
        return Err(format!(
            "Nickname is {len} characters; max is {NICKNAME_MAX_LEN}."
        ));
    }
    for c in name.chars() {
        let ok = c.is_ascii_alphanumeric()
            || matches!(c, ' ' | '_' | '-' | '.' | '\'');
        if !ok {
            return Err(format!(
                "Nickname contains unsupported character `{c}`. Allowed: letters, digits, space, dash, underscore, period, apostrophe."
            ));
        }
    }
    if is_reserved_user_pattern(name) {
        return Err(
            "Nickname `user-XXXXXXXX` is reserved for the auto-generated anonymous fallback. Pick something else.".to_string(),
        );
    }
    Ok(())
}

/// `user-` followed by exactly 8 ASCII hex chars is the auto-generated
/// anonymous shape the server hands out when `show_display_name` is false
/// or the user hasn't set one. Engine refuses to send it to the set
/// endpoint so users can't squat on the auto-anon namespace.
fn is_reserved_user_pattern(name: &str) -> bool {
    let bytes = name.as_bytes();
    if bytes.len() != "user-".len() + 8 {
        return false;
    }
    if !bytes.starts_with(b"user-") {
        return false;
    }
    bytes[5..].iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

/// Outcome of the GET-display-name (sub-set of /profile/me).
#[derive(Debug, Clone)]
pub struct DisplayNameInfo {
    pub display_name: String,
    /// "manual" if the user picked it; "auto" for `user-XXXXXXXX` fallback.
    /// Engine uses this to decide whether to prompt for a real nickname
    /// before the first Ranked bench (auto -> prompt; manual -> skip).
    pub source: DisplayNameSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayNameSource {
    Manual,
    Auto,
    Unknown,
}

#[derive(Debug, Clone)]
pub enum DisplayNameOutcome {
    /// 200 success — `display_name` is the new server-canonical value;
    /// `rows_updated` is the count of backfilled submission rows.
    Set { display_name: String, rows_updated: u64 },
    /// 200 success on GET — current display_name + source.
    Got(DisplayNameInfo),
    /// HTTP 401. Install changed / token rotated; user should re-link.
    Unauthorised(String),
    /// HTTP 429. Server-side rate limit (5 per 24h per account). The
    /// raw response text is surfaced so the user sees the server's
    /// reason verbatim.
    RateLimited(String),
    /// HTTP 4xx (non-401, non-429). Schema or validation rejection;
    /// surface verbatim so the user can fix the input.
    Rejected(String),
    /// 5xx / network failure. Caller retries later.
    Transient(String),
}

/// `GET /api/v1/profile/me` and pull the display_name + display_name_source.
/// Reuses the account_privacy::fetch HMAC + envelope-tolerant parse pattern
/// (server has shipped both `display_name_source` and missing-field
/// responses historically — degrade gracefully to DisplayNameSource::Unknown
/// when the field is absent).
pub fn fetch(state: &InstallState, server_url: &str) -> DisplayNameOutcome {
    if !state.registered {
        return DisplayNameOutcome::Unauthorised(
            "install not registered — call `superdeduper register` first".to_string(),
        );
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return DisplayNameOutcome::Rejected("install_key_hex malformed".to_string());
        }
    };
    let url = format!("{}/api/v1/profile/me", server_url.trim_end_matches('/'));
    let canonical_marker = serde_json::json!({"install_id": state.install_id});
    let body = super::hmac_signer::canonical_body(&canonical_marker);
    let signature = super::hmac_signer::sign(&install_key, &body);
    let response = ureq::get(&url)
        .set("X-Sd-Install-Id", &state.install_id)
        .set("X-Sd-Signature", &signature)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(10))
        .send_bytes(&body);
    match response {
        Ok(resp) => parse_get_response(resp),
        Err(ureq::Error::Status(401, resp)) => {
            DisplayNameOutcome::Unauthorised(resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            DisplayNameOutcome::Rejected(format!(
                "{code}: {}",
                resp.into_string().unwrap_or_default()
            ))
        }
        Err(ureq::Error::Status(code, resp)) => DisplayNameOutcome::Transient(format!(
            "{code}: {}",
            resp.into_string().unwrap_or_default()
        )),
        Err(ureq::Error::Transport(t)) => DisplayNameOutcome::Transient(format!("transport: {t}")),
    }
}

/// `POST /api/v1/account/display_name` with the new nickname. Client-side
/// validation runs first so a bad name doesn't burn the user's 5/24h
/// rate-limit budget on a server-side reject.
pub fn set(state: &InstallState, server_url: &str, display_name: &str) -> DisplayNameOutcome {
    if let Err(e) = validate_nickname(display_name) {
        return DisplayNameOutcome::Rejected(e);
    }
    if !state.registered {
        return DisplayNameOutcome::Unauthorised(
            "install not registered — call `superdeduper register` first".to_string(),
        );
    }
    let install_key = match state.install_key() {
        Some(k) => k,
        None => {
            return DisplayNameOutcome::Rejected("install_key_hex malformed".to_string());
        }
    };
    let payload = serde_json::json!({
        "install_id": state.install_id,
        "display_name": display_name,
    });
    let body = super::hmac_signer::canonical_body(&payload);
    let signature = super::hmac_signer::sign(&install_key, &body);
    let url = format!(
        "{}/api/v1/account/display_name",
        server_url.trim_end_matches('/')
    );
    let response = ureq::post(&url)
        .set("X-Sd-Install-Id", &state.install_id)
        .set("X-Sd-Signature", &signature)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&body);
    match response {
        Ok(resp) => parse_set_response(resp),
        Err(ureq::Error::Status(401, resp)) => {
            DisplayNameOutcome::Unauthorised(resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(429, resp)) => {
            DisplayNameOutcome::RateLimited(resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            DisplayNameOutcome::Rejected(format!(
                "{code}: {}",
                resp.into_string().unwrap_or_default()
            ))
        }
        Err(ureq::Error::Status(code, resp)) => DisplayNameOutcome::Transient(format!(
            "{code}: {}",
            resp.into_string().unwrap_or_default()
        )),
        Err(ureq::Error::Transport(t)) => DisplayNameOutcome::Transient(format!("transport: {t}")),
    }
}

fn parse_get_response(resp: ureq::Response) -> DisplayNameOutcome {
    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return DisplayNameOutcome::Transient(format!("200 OK but body parse failed: {e}"));
        }
    };
    let display_name = body
        .get("display_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let Some(display_name) = display_name else {
        return DisplayNameOutcome::Transient(format!(
            "profile/me missing display_name; raw: {body}"
        ));
    };
    let source = match body.get("display_name_source").and_then(|v| v.as_str()) {
        Some("manual") => DisplayNameSource::Manual,
        Some("auto") => DisplayNameSource::Auto,
        _ => DisplayNameSource::Unknown,
    };
    DisplayNameOutcome::Got(DisplayNameInfo {
        display_name,
        source,
    })
}

fn parse_set_response(resp: ureq::Response) -> DisplayNameOutcome {
    let body: serde_json::Value = match resp.into_json() {
        Ok(v) => v,
        Err(e) => {
            return DisplayNameOutcome::Transient(format!("200 OK but body parse failed: {e}"));
        }
    };
    let display_name = body
        .get("display_name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let Some(display_name) = display_name else {
        return DisplayNameOutcome::Transient(format!(
            "set-display-name missing display_name; raw: {body}"
        ));
    };
    let rows_updated = body
        .get("backfill")
        .and_then(|b| b.get("rows_updated"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    DisplayNameOutcome::Set {
        display_name,
        rows_updated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_happy_path() {
        assert!(validate_nickname("Mick").is_ok());
        assert!(validate_nickname("user_42").is_ok());
        assert!(validate_nickname("d'Artagnan").is_ok());
        assert!(validate_nickname("J.R.R.").is_ok());
        assert!(validate_nickname("My-Nickname 2025").is_ok());
    }

    #[test]
    fn validate_length_bounds() {
        assert!(validate_nickname("").is_err());
        assert!(validate_nickname(&"a".repeat(NICKNAME_MAX_LEN)).is_ok());
        assert!(validate_nickname(&"a".repeat(NICKNAME_MAX_LEN + 1)).is_err());
    }

    #[test]
    fn validate_rejects_unsupported_chars() {
        assert!(validate_nickname("emoji 🚀").is_err());
        assert!(validate_nickname("tabbed\tname").is_err());
        assert!(validate_nickname("zero\u{200B}width").is_err());
        assert!(validate_nickname("slash/name").is_err());
        assert!(validate_nickname("comma,name").is_err());
    }

    #[test]
    fn validate_rejects_user_pattern() {
        // Exact auto-fallback shape `user-XXXXXXXX` (8 ASCII hex chars) is reserved.
        assert!(validate_nickname("user-deadbeef").is_err());
        assert!(validate_nickname("user-DEADBEEF").is_err());
        // Almost-the-shape variants stay valid — only the exact auto-anon
        // pattern is squat-protected. `user-` is otherwise a normal prefix.
        assert!(validate_nickname("user-deadbef").is_ok()); // 7 hex chars
        assert!(validate_nickname("user-zzzzzzzz").is_ok()); // 8 alphanum chars but not hex
        assert!(validate_nickname("user-0123abcde").is_ok()); // 9 hex chars
        assert!(validate_nickname("USER-0123abcd").is_ok()); // wrong-case prefix
    }

    #[test]
    fn is_reserved_user_pattern_exact() {
        assert!(is_reserved_user_pattern("user-0123abcd"));
        assert!(is_reserved_user_pattern("user-ABCD1234"));
        assert!(!is_reserved_user_pattern("user-zzzzzzzz")); // 'z' not hex
        assert!(!is_reserved_user_pattern("user-123abc")); // too short
        assert!(!is_reserved_user_pattern("user-0123abcde")); // too long
        assert!(!is_reserved_user_pattern("USER-0123abcd")); // case-sensitive prefix
        assert!(!is_reserved_user_pattern("Mick"));
    }
}
