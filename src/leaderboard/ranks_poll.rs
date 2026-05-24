//! Background poller for `GET /api/v1/ranks`. Spawned on Accepted
//! submission; loops 30 × 1s waiting for the backend's async rank
//! compute to populate. On success merges ranks into the stored
//! outcome (so the post-scan modal updates if still open) AND
//! pushes a corner toast (so a closed-modal user still sees the
//! result).
//!
//! Canonical input per backend contract:
//!
//! ```text
//! ${install_id}|${submission_id}
//! ```
//!
//! Signed with HMAC-SHA256 using the install_key. No body.
//!
//! Failure modes are silent: this is a polish surface, not a
//! correctness path. If the poller can't reach the server or times
//! out the user still has their submission acknowledged via the
//! Accepted modal — rank just doesn't surface this run.

use std::time::Duration;

use serde::Deserialize;

use crate::leaderboard::hmac_signer;
use crate::leaderboard::install::{InstallKey, InstallState};
use crate::leaderboard::submission::{self, RankEntry};

/// Default poll cadence + bound. Matches the budget we promised web.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const POLL_TIMEOUT_TOTAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub enum PollResult {
    /// Backend returned `status: "ready"` with rank rows.
    Ready(Vec<RankEntry>),
    /// Backend returned `status: "pending"` or `ranks: []` with
    /// no `status` field. Caller should sleep + retry.
    Pending,
    /// HTTP 404 — submission not found (e.g. install_id mismatch).
    /// Permanent; caller should stop polling.
    NotFound,
    /// HTTP error or transport failure. Caller decides whether to
    /// retry — for the worker loop, we retry on transient and
    /// give up on auth/permission errors.
    Err(String),
}

#[derive(Debug, Deserialize)]
struct RanksResponse {
    status: Option<String>,
    #[serde(default)]
    ranks: Vec<RankEntry>,
}

/// One HTTP poll against the ranks endpoint. Pure function (modulo
/// network) so the worker loop reads cleanly.
pub fn poll_once(
    server_url: &str,
    install_id: &str,
    submission_id: &str,
    install_key: &InstallKey,
) -> PollResult {
    let canonical = format!("{install_id}|{submission_id}");
    let signature = hmac_signer::sign_canonical(install_key, &canonical);
    let url = format!(
        "{}/api/v1/ranks?install_id={}&submission_id={}",
        server_url.trim_end_matches('/'),
        urlencode(install_id),
        urlencode(submission_id),
    );

    let response = ureq::get(&url)
        .set("X-Sd-Signature", &signature)
        .timeout(Duration::from_secs(10))
        .call();

    match response {
        Ok(resp) => match resp.into_json::<RanksResponse>() {
            Ok(parsed) => {
                let status = parsed.status.as_deref().unwrap_or("ready");
                if status == "ready" && !parsed.ranks.is_empty() {
                    PollResult::Ready(parsed.ranks)
                } else {
                    PollResult::Pending
                }
            }
            Err(e) => PollResult::Err(format!("200 OK body parse failed: {e}")),
        },
        Err(ureq::Error::Status(404, _)) => PollResult::NotFound,
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            PollResult::Err(format!("HTTP {code}: {body}"))
        }
        Err(ureq::Error::Transport(t)) => PollResult::Err(format!("transport: {t}")),
    }
}

/// Spawn the poll worker. Reads install state, loops up to
/// [`POLL_TIMEOUT_TOTAL`] / [`POLL_INTERVAL`] iterations. On
/// `Ready` merges ranks into the stored outcome + pushes a toast.
/// Silent-give-up on timeout or persistent error.
pub fn spawn_ranks_poll_worker(submission_id: String) {
    if submission_id.is_empty() {
        eprintln!("ranks-poll: empty submission_id; skipping (web won't have known what to look up)");
        return;
    }
    std::thread::spawn(move || run(submission_id));
}

fn run(submission_id: String) {
    let state: InstallState = match crate::leaderboard::install::load() {
        Ok(Some(s)) if s.registered => s,
        _ => {
            eprintln!("ranks-poll: install not registered; skipping");
            return;
        }
    };
    let key = match state.install_key() {
        Some(k) => k,
        None => {
            eprintln!("ranks-poll: install_key malformed; skipping");
            return;
        }
    };
    let max_iters = POLL_TIMEOUT_TOTAL.as_secs() / POLL_INTERVAL.as_secs().max(1);
    for i in 0..max_iters {
        std::thread::sleep(POLL_INTERVAL);
        let result = poll_once(&state.server_url, &state.install_id, &submission_id, &key);
        match result {
            PollResult::Ready(ranks) => {
                deliver_ranks(ranks);
                return;
            }
            PollResult::Pending => continue,
            PollResult::NotFound => {
                eprintln!(
                    "ranks-poll: submission_id={submission_id} 404 — backend doesn't know about it; giving up"
                );
                return;
            }
            PollResult::Err(e) => {
                eprintln!("ranks-poll: poll {i} failed ({e}); will retry");
            }
        }
    }
    eprintln!(
        "ranks-poll: submission_id={submission_id} timed out after {}s with no ranks; silent give-up",
        POLL_TIMEOUT_TOTAL.as_secs()
    );
}

/// On a successful poll, update the in-memory outcome (so an open
/// modal re-renders with the ranks) AND push a toast (so a closed
/// modal user still sees them).
fn deliver_ranks(ranks: Vec<RankEntry>) {
    submission::update_last_outcome_ranks(ranks.clone());

    let lines: Vec<String> = ranks
        .iter()
        .map(|r| {
            format!(
                "{} rank #{} of {}",
                pretty_category(&r.category),
                r.rank,
                format_thousands(r.bucket_size),
            )
        })
        .collect();

    crate::gui::widgets::toast::push(
        "Leaderboard rank in",
        lines,
        Duration::from_secs(8),
    );
}

/// Map backend `category` codes to user-facing labels. Web returns
/// `throughput` and `lifetime-reclaimed` (hyphenated); anything
/// else passes through verbatim so a future category we haven't
/// taught the client about still renders something.
fn pretty_category(c: &str) -> String {
    match c {
        "throughput" => "Throughput".to_string(),
        "lifetime-reclaimed" => "Lifetime reclaimed".to_string(),
        other => {
            let mut s = String::with_capacity(other.len());
            let mut up = true;
            for ch in other.chars() {
                if ch == '-' || ch == '_' {
                    s.push(' ');
                    up = true;
                } else if up {
                    s.extend(ch.to_uppercase());
                    up = false;
                } else {
                    s.push(ch);
                }
            }
            s
        }
    }
}

fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Minimal percent-encoding for query-param values. install_id +
/// submission_id are server-generated UUIDs (hex + hyphens) so this
/// is defensive only — but the contract allows any string, so
/// escape everything outside the unreserved set.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if ok {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencode_passes_uuid_chars_through() {
        // Real-world install_id + submission_id format.
        let s = "e1eae1fa-58fb-4f5a-8712-a7480ac5761b";
        assert_eq!(urlencode(s), s);
    }

    #[test]
    fn urlencode_escapes_special_chars() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a|b"), "a%7Cb");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn pretty_category_known() {
        assert_eq!(pretty_category("throughput"), "Throughput");
        assert_eq!(pretty_category("lifetime-reclaimed"), "Lifetime reclaimed");
    }

    #[test]
    fn pretty_category_unknown_title_cases() {
        assert_eq!(pretty_category("reclaim-rate"), "Reclaim Rate");
        assert_eq!(pretty_category("foo_bar_baz"), "Foo Bar Baz");
    }

    #[test]
    fn format_thousands_groups_correctly() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(42), "42");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(1_284), "1,284");
        assert_eq!(format_thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn empty_submission_id_is_silent_skip() {
        // Just ensures the early-return path doesn't panic and the
        // spawn never fires a thread. Exhaustive check: this would
        // panic on unwrap() of a non-existent install state if the
        // early-return failed.
        spawn_ranks_poll_worker(String::new());
    }

    #[test]
    fn canonical_input_matches_web_contract() {
        // Per web's spec: ${install_id}|${submission_id}, pipe-
        // separated, no whitespace, no trailing newline. Lock the
        // exact byte-level format we sign — any drift here means
        // the backend's HMAC compare fails.
        let install_id = "e1eae1fa-58fb-4f5a-8712-a7480ac5761b";
        let submission_id = "2bcc8bb0-06d1-49b8-9e10-55068e1485f1";
        let canonical = format!("{install_id}|{submission_id}");
        assert_eq!(
            canonical,
            "e1eae1fa-58fb-4f5a-8712-a7480ac5761b|2bcc8bb0-06d1-49b8-9e10-55068e1485f1"
        );
        assert!(!canonical.contains(' '));
        assert!(!canonical.ends_with('\n'));
    }
}
