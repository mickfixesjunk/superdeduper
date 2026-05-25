//! #47 — Vanity-slug issuance for public profile URLs.
//!
//! Engine generates a sensible slug from the user's OAuth display
//! name at account-claim time, validates per the locked rule set
//! below, and submits it to the server for storage + uniqueness
//! enforcement. The server is the source of truth (it knows the
//! global namespace); engine's slot is "first attempt + retry
//! with a numeric suffix if the server reports conflict."
//!
//! ## Slug rules (locked)
//!
//! * Length: 3..=32 chars.
//! * Charset: lowercase ASCII letters, digits, hyphens only.
//! * Must START with a letter (digits-first looks too id-like;
//!   hyphen-first reads as a flag).
//! * Cannot END with a hyphen.
//! * Cannot contain consecutive hyphens.
//! * Cannot be on the reserved list (system + admin paths).
//!
//! ## Generation strategy
//!
//! 1. Lowercase the display name.
//! 2. Replace whitespace + invalid chars with `-`.
//! 3. Collapse consecutive hyphens.
//! 4. Trim leading hyphens until we land on a letter (if no letter
//!    at all, generation fails — caller falls back to a UUID-prefix
//!    slug like `user-9d4a`).
//! 5. Trim trailing hyphens.
//! 6. Truncate to 32 chars.
//! 7. If after all that the slug is <3 chars or reserved, generation
//!    fails (caller falls back).

#![cfg(feature = "telemetry")]

use std::collections::HashSet;
use std::sync::OnceLock;

/// Errors surfaced by [`validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VanityError {
    TooShort,
    TooLong,
    /// First char not an ASCII letter.
    BadFirstChar,
    /// Trailing hyphen.
    TrailingHyphen,
    /// `--` somewhere in the slug.
    ConsecutiveHyphens,
    /// Contains a char outside `[a-z0-9-]`.
    InvalidChar(char),
    /// On the reserved list (admin, api, www, etc.).
    Reserved,
}

impl std::fmt::Display for VanityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "slug must be at least 3 chars"),
            Self::TooLong => write!(f, "slug must be at most 32 chars"),
            Self::BadFirstChar => write!(f, "slug must start with a letter"),
            Self::TrailingHyphen => write!(f, "slug cannot end with a hyphen"),
            Self::ConsecutiveHyphens => write!(f, "slug cannot contain `--`"),
            Self::InvalidChar(c) => write!(f, "slug contains invalid char `{c}`"),
            Self::Reserved => write!(f, "slug is reserved"),
        }
    }
}

impl std::error::Error for VanityError {}

const MIN_LEN: usize = 3;
const MAX_LEN: usize = 32;

/// Reserved slugs. Conservative — anything that conflicts with a
/// likely URL path (`/api/...`, `/admin`, `/static`), any of the
/// channel slugs, or could be confused for a system identity.
/// Lowercase; comparison is case-insensitive at the validate call
/// site (input is normalised before lookup).
fn reserved_slugs() -> &'static HashSet<&'static str> {
    static SET: OnceLock<HashSet<&'static str>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            // System paths.
            "admin",
            "administrator",
            "root",
            "system",
            "sys",
            "owner",
            "official",
            // URL paths (engine + likely-future web routes).
            "api",
            "www",
            "static",
            "assets",
            "cdn",
            "files",
            "media",
            "uploads",
            "auth",
            "login",
            "logout",
            "signin",
            "signout",
            "register",
            "signup",
            "oauth",
            "callback",
            "settings",
            "account",
            "profile",
            "profiles",
            "leaderboard",
            "leaderboards",
            "rank",
            "ranks",
            "search",
            "help",
            "support",
            "docs",
            "doc",
            "blog",
            "news",
            "about",
            "tos",
            "privacy",
            "terms",
            "legal",
            "contact",
            "home",
            "index",
            // Channel slugs.
            "prod",
            "dev",
            "local",
            "staging",
            "beta",
            "alpha",
            // Engine identity.
            "superdeduper",
            "sd",
            "engine",
            "client",
            "server",
            // Anonymous / no-account placeholders.
            "anonymous",
            "anon",
            "guest",
            "user",
            "users",
            "self",
            "me",
            // Common slurs / abuse — keep this minimal; the server's
            // moderation layer is the authority. We block only the
            // most blatantly trivial cases here.
            "null",
            "undefined",
            "deleted",
            "banned",
        ]
        .into_iter()
        .collect()
    })
}

/// Check a slug against the rule set. Returns `Ok(())` for valid
/// slugs. Caller can `?` directly; the `VanityError` type carries
/// a user-friendly `Display` impl.
pub fn validate(slug: &str) -> Result<(), VanityError> {
    if slug.len() < MIN_LEN {
        return Err(VanityError::TooShort);
    }
    if slug.len() > MAX_LEN {
        return Err(VanityError::TooLong);
    }
    let first = slug.chars().next().unwrap();
    if !first.is_ascii_lowercase() {
        return Err(VanityError::BadFirstChar);
    }
    if slug.ends_with('-') {
        return Err(VanityError::TrailingHyphen);
    }
    if slug.contains("--") {
        return Err(VanityError::ConsecutiveHyphens);
    }
    for c in slug.chars() {
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
            return Err(VanityError::InvalidChar(c));
        }
    }
    if reserved_slugs().contains(slug) {
        return Err(VanityError::Reserved);
    }
    Ok(())
}

/// Produce a slug candidate from a free-form display name. Returns
/// `None` if no legal slug can be derived (display name is empty,
/// has no letters, or normalises to a reserved/too-short slug).
/// Caller decides the fallback — typically `user-{install_id[..4]}`.
pub fn generate_from_display_name(display_name: &str) -> Option<String> {
    let lower = display_name.to_lowercase();
    let mut s = String::with_capacity(lower.len());
    // Step 1+2: lowercase + replace invalid with `-`.
    for c in lower.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            s.push(c);
        } else {
            s.push('-');
        }
    }
    // Step 3: collapse consecutive hyphens.
    let mut collapsed = String::with_capacity(s.len());
    let mut last_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !last_hyphen {
                collapsed.push('-');
            }
            last_hyphen = true;
        } else {
            collapsed.push(c);
            last_hyphen = false;
        }
    }
    // Step 4: trim leading non-letters (drop hyphens + digits until
    // we hit a letter).
    let trimmed_start: String = collapsed
        .chars()
        .skip_while(|c| !c.is_ascii_lowercase())
        .collect();
    if trimmed_start.is_empty() {
        return None; // no letter at all → caller falls back.
    }
    // Step 5: trim trailing hyphens.
    let trimmed_end = trimmed_start.trim_end_matches('-').to_string();
    // Step 6: truncate to MAX_LEN.
    let truncated: String = trimmed_end.chars().take(MAX_LEN).collect();
    // Truncation might leave a trailing hyphen — re-trim.
    let truncated = truncated.trim_end_matches('-').to_string();
    // Step 7: validate. If it fails, the slug isn't usable — caller
    // falls back to a UUID-prefix slug.
    validate(&truncated).ok()?;
    Some(truncated)
}

/// Outcome of [`claim`]. Distinct from a generic Result so callers
/// can distinguish "server accepted exactly this slug" from "server
/// suggested something different" (rare; the directive's wire shape
/// is "claim returns the canonical slug, possibly with a suffix").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimOutcome {
    /// Server stored the slug. Carries the FINAL slug the server
    /// chose — usually equal to the request but may differ if the
    /// server settled a race-with-another-claim by appending a
    /// suffix.
    Claimed(String),
    /// Server returned a conflict (HTTP 409) — caller should retry
    /// with [`suffix_for_retry`] and a fresh attempt counter.
    Conflict,
    /// Server returned a validation rejection (HTTP 400 with a
    /// body describing the rule violation). Caller surfaces the
    /// message to the user; retry won't help.
    Rejected(String),
    /// Server returned a non-2xx, non-4xx response, or the request
    /// timed out / failed at the transport layer. Caller can retry
    /// on next launch.
    Transient(String),
    /// Auth failure — install_id / install_key combo didn't pass
    /// the server's check. Caller surfaces "register first" or
    /// "your install was reset."
    Unauthorised(String),
}

/// POST a slug claim to the server. The wire shape:
///
/// ```text
/// POST {server_url}/api/v1/account/vanity-slug
/// X-Sd-Signature: <hmac>
/// Content-Type: application/json
///
/// {"install_id": "...", "account_id": "...", "slug": "..."}
/// ```
///
/// Response 200: `{"slug": "..."}` (possibly different from request
/// if the server applied a tie-break suffix).
/// Response 409: conflict — slug taken.
/// Response 400: validation rejection (body has `reason`).
/// Response 401: auth fail — caller usually re-registers.
/// Response 5xx / network failure: Transient.
///
/// Server-side route, response shape, and uniqueness semantics are
/// owned by `web`. Engine's contract is "we POST a candidate, you
/// return the canonical." See web's `vanity-slug.ts` (forthcoming).
pub fn claim(
    state: &super::install::InstallState,
    account_id: &str,
    server_url: &str,
    slug: &str,
) -> ClaimOutcome {
    // Validate locally first — saves an RTT for obviously-broken
    // slugs (caller might have skipped validate() when retrying).
    if let Err(e) = validate(slug) {
        return ClaimOutcome::Rejected(format!("local validate: {e}"));
    }
    let key = match state.install_key() {
        Some(k) => k,
        None => {
            return ClaimOutcome::Rejected("install_key_hex malformed".into());
        }
    };
    let payload = serde_json::json!({
        "install_id": state.install_id,
        "account_id": account_id,
        "slug": slug,
    });
    let body = super::hmac_signer::canonical_body(&payload);
    let signature = super::hmac_signer::sign(&key, &body);
    let url = format!(
        "{}/api/v1/account/vanity-slug",
        server_url.trim_end_matches('/')
    );
    let response = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(10))
        .send_bytes(&body);
    match response {
        Ok(resp) => match resp.into_json::<serde_json::Value>() {
            Ok(v) => match v.get("slug").and_then(|s| s.as_str()) {
                Some(returned) => ClaimOutcome::Claimed(returned.to_string()),
                None => ClaimOutcome::Transient(
                    "server returned 200 but no `slug` field in response body".into(),
                ),
            },
            Err(e) => ClaimOutcome::Transient(format!("200 OK but body parse failed: {e}")),
        },
        Err(ureq::Error::Status(409, _)) => ClaimOutcome::Conflict,
        Err(ureq::Error::Status(401, resp)) => {
            ClaimOutcome::Unauthorised(resp.into_string().unwrap_or_default())
        }
        Err(ureq::Error::Status(code, resp)) if (400..500).contains(&code) => {
            let body_text = resp.into_string().unwrap_or_default();
            ClaimOutcome::Rejected(format!("{code}: {body_text}"))
        }
        Err(ureq::Error::Status(code, resp)) => ClaimOutcome::Transient(format!(
            "{code}: {}",
            resp.into_string().unwrap_or_default()
        )),
        Err(ureq::Error::Transport(t)) => ClaimOutcome::Transient(format!("transport: {t}")),
    }
}

/// Higher-level orchestration: derive a slug from `display_name`,
/// fall back to a UUID-prefix slug if generation fails, and retry
/// conflicts up to `max_retries` times with numeric suffixes.
/// Returns the canonical slug on success, or `Err(reason)` on a
/// terminal failure (rejection, auth, or repeated conflict).
pub fn derive_and_claim(
    state: &super::install::InstallState,
    account_id: &str,
    server_url: &str,
    display_name: &str,
    max_retries: u32,
) -> Result<String, String> {
    // Base candidate from display name; fallback if generation fails.
    let base = generate_from_display_name(display_name)
        .or_else(|| {
            // Fallback shape: `user-{first 4 hex of install_id}`.
            // Stable across the account but doesn't leak the
            // display name; ugly but always-legal.
            let prefix = state.install_id.chars().take(4).collect::<String>();
            if prefix.is_empty() {
                None
            } else {
                let candidate = format!("user-{prefix}");
                validate(&candidate).ok().map(|()| candidate)
            }
        })
        .ok_or_else(|| "could not derive any legal slug candidate".to_string())?;

    let mut attempt = 0u32;
    let mut candidate = base.clone();
    loop {
        match claim(state, account_id, server_url, &candidate) {
            ClaimOutcome::Claimed(canonical) => return Ok(canonical),
            ClaimOutcome::Conflict if attempt < max_retries => {
                attempt += 1;
                candidate = suffix_for_retry(&base, attempt + 1)
                    .ok_or_else(|| "no legal suffix retry left".to_string())?;
                continue;
            }
            ClaimOutcome::Conflict => {
                return Err(format!(
                    "conflict on `{candidate}` and {max_retries} retries exhausted"
                ));
            }
            ClaimOutcome::Rejected(r) => return Err(format!("rejected: {r}")),
            ClaimOutcome::Unauthorised(r) => return Err(format!("unauthorised: {r}")),
            ClaimOutcome::Transient(r) => return Err(format!("transient: {r}")),
        }
    }
}

/// Append a numeric suffix to a candidate slug for retry after a
/// server-side conflict response. Returns `None` if appending the
/// suffix would exceed MAX_LEN AND truncating the base to make
/// room would leave it shorter than MIN_LEN — caller falls back
/// to a fresh random slug.
pub fn suffix_for_retry(base: &str, attempt: u32) -> Option<String> {
    let suffix = format!("-{attempt}");
    let max_base_len = MAX_LEN.saturating_sub(suffix.len());
    if max_base_len < MIN_LEN {
        return None;
    }
    let trimmed_base: String = base.chars().take(max_base_len).collect();
    let trimmed_base = trimmed_base.trim_end_matches('-').to_string();
    if trimmed_base.is_empty() {
        return None;
    }
    let candidate = format!("{trimmed_base}{suffix}");
    validate(&candidate).ok()?;
    Some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_canonical() {
        assert!(validate("mick").is_ok());
        assert!(validate("alice-w").is_ok());
        assert!(validate("user42").is_ok());
        assert!(validate("a1-b2-c3").is_ok());
        // 32 chars exactly — boundary.
        assert!(validate("abcdefghijklmnopqrstuvwxyz012345").is_ok());
    }

    #[test]
    fn validate_rejects_too_short() {
        assert_eq!(validate("ab"), Err(VanityError::TooShort));
        assert_eq!(validate("a"), Err(VanityError::TooShort));
        assert_eq!(validate(""), Err(VanityError::TooShort));
    }

    #[test]
    fn validate_rejects_too_long() {
        let s = "a".repeat(33);
        assert_eq!(validate(&s), Err(VanityError::TooLong));
    }

    #[test]
    fn validate_rejects_bad_first_char() {
        assert_eq!(validate("1user"), Err(VanityError::BadFirstChar));
        assert_eq!(validate("-user"), Err(VanityError::BadFirstChar));
        // Uppercase first char triggers BadFirstChar via the
        // lowercase predicate (the InvalidChar check runs on the
        // body separately).
        assert_eq!(validate("User"), Err(VanityError::BadFirstChar));
    }

    #[test]
    fn validate_rejects_trailing_hyphen() {
        assert_eq!(validate("user-"), Err(VanityError::TrailingHyphen));
    }

    #[test]
    fn validate_rejects_consecutive_hyphens() {
        assert_eq!(validate("user--name"), Err(VanityError::ConsecutiveHyphens));
    }

    #[test]
    fn validate_rejects_invalid_chars() {
        // Uppercase mid-slug — caught by InvalidChar (first-char
        // gate already cleared with `m`).
        let err = validate("mickFix").unwrap_err();
        assert_eq!(err, VanityError::InvalidChar('F'));
        let err = validate("mick_fix").unwrap_err();
        assert_eq!(err, VanityError::InvalidChar('_'));
    }

    #[test]
    fn validate_rejects_reserved() {
        assert_eq!(validate("admin"), Err(VanityError::Reserved));
        assert_eq!(validate("api"), Err(VanityError::Reserved));
        assert_eq!(validate("superdeduper"), Err(VanityError::Reserved));
        // Non-reserved variant — defeats the simple block.
        assert!(validate("admin1").is_ok());
    }

    #[test]
    fn generate_from_display_name_canonical() {
        assert_eq!(
            generate_from_display_name("Mick Fixes Junk"),
            Some("mick-fixes-junk".to_string())
        );
        assert_eq!(
            generate_from_display_name("Alice"),
            Some("alice".to_string())
        );
    }

    #[test]
    fn generate_from_display_name_collapses_punctuation() {
        assert_eq!(
            generate_from_display_name("alice@example.com"),
            Some("alice-example-com".to_string())
        );
        assert_eq!(
            generate_from_display_name("alice___bob"),
            Some("alice-bob".to_string())
        );
    }

    #[test]
    fn generate_from_display_name_trims_leading_digits() {
        assert_eq!(
            generate_from_display_name("42 Alice"),
            Some("alice".to_string())
        );
        assert_eq!(
            generate_from_display_name("-42-alice"),
            Some("alice".to_string())
        );
    }

    #[test]
    fn generate_from_display_name_no_letter_returns_none() {
        assert_eq!(generate_from_display_name("12345"), None);
        assert_eq!(generate_from_display_name("---"), None);
        assert_eq!(generate_from_display_name(""), None);
    }

    #[test]
    fn generate_from_display_name_caps_at_32() {
        // 40-letter input → 32-char output.
        let long = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
        let out = generate_from_display_name(long).unwrap();
        assert_eq!(out.len(), 32);
        assert!(validate(&out).is_ok());
    }

    #[test]
    fn generate_returns_none_on_reserved_normalisation() {
        // "Admin!" → "admin" → reserved → caller falls back.
        assert_eq!(generate_from_display_name("Admin!"), None);
    }

    #[test]
    fn suffix_for_retry_appends() {
        let base = "mick";
        assert_eq!(suffix_for_retry(base, 2).as_deref(), Some("mick-2"));
        assert_eq!(suffix_for_retry(base, 99).as_deref(), Some("mick-99"));
    }

    #[test]
    fn suffix_for_retry_truncates_long_base() {
        let base = "abcdefghijklmnopqrstuvwxyz012345"; // 32 chars
        let out = suffix_for_retry(base, 3).unwrap();
        assert!(out.ends_with("-3"));
        assert!(out.len() <= MAX_LEN);
        assert!(validate(&out).is_ok());
    }

    #[test]
    fn suffix_for_retry_strips_hyphen_from_base_before_suffix() {
        // Base ends with a hyphen; truncation strategy should drop
        // the hyphen so we don't get "alice---2" or "alice--2".
        let base = "alice-bob-".to_string(); // hand-crafted, not from generate (which would also strip).
        let out = suffix_for_retry(&base, 5).unwrap();
        assert_eq!(out, "alice-bob-5");
    }
}
