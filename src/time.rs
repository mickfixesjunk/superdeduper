//! Time helpers — single source of truth for `now_unix_*` +
//! civil-from-days math. Pre-#91/#133 these were duplicated:
//!   * `now_unix()` defined 8× across cache, diagnose, submission,
//!     trash (Linux), gui::{app, checkpoint, results_store},
//!     scan_history. Mixed `u64` and `i64` signatures.
//!   * Howard Hinnant civil-from-days algorithm (magic constants
//!     `719_468` + `146_097`) duplicated 6× across action_receipt,
//!     gui::diagnostics, gui::checkpoint, trash, and inline copies
//!     in scan_history_panel + badge_multiplier_detail.
//!   * `SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or(0)`
//!     pattern repeated 6+ times in main, gui::live, gui::project,
//!     gui::checkpoint, settings_modal — silently maps a skewed
//!     clock to 1970-01-01.
//!
//! The fix lives here so future timestamp tweaks (clock-skew
//! policy, pre-1970 mtime handling, ISO8601 format) land in ONE
//! place instead of six.
//!
//! Leaf module — no imports from other crate modules.

use std::time::{SystemTime, UNIX_EPOCH};

/// Seconds since the Unix epoch as an i64 — matches the wire shape
/// of the cache, scan_history, and submission persistence layers.
///
/// On clock skew (system clock before UNIX_EPOCH) we log a warning
/// via `tracing` and fall back to 0. The pre-#133 silent fallback
/// at 6+ call sites turned skew into a "1970-01-01" timestamp on
/// scan history records that would render funny + poison the
/// leaderboard's sort/dedupe logic; this surfaces the failure
/// without crashing the engine. Use [`now_unix_secs_checked`] when
/// the caller wants to react to skew (e.g. refuse to write a
/// submission with a clearly-bogus timestamp).
pub fn now_unix_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(|_| {
            tracing::warn!("system clock before UNIX_EPOCH; using 0 (see #133)");
            0
        })
}

/// Seconds since the Unix epoch as a `u64`. Same skew policy as
/// [`now_unix_i64`]. Preferred at sites that previously used the
/// `.unwrap_or(Duration::from_secs(0)).as_secs()` pattern.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_else(|_| {
            tracing::warn!("system clock before UNIX_EPOCH; using 0 (see #133)");
            0
        })
}

/// Fallible variant — returns `None` on clock skew so the caller
/// can react instead of silently writing a 1970 timestamp.
///
/// Use this in any code path that persists the result to a
/// leaderboard payload or scan_history record where a 1970
/// timestamp would be observably wrong post-hoc.
pub fn now_unix_secs_checked() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// ISO-8601 timestamp with a `Z` suffix (UTC, no offset). Format:
/// `YYYY-MM-DDTHH:MM:SSZ`. Matches what the leaderboard backend's
/// submission schema expects in `outer_timestamp` / `started_at`.
pub fn now_iso8601() -> String {
    let secs = now_unix_i64();
    let (y, mo, d, h, mi, s) = unix_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Unix seconds (any sign) → broken-down UTC components:
/// `(year, month, day, hour, minute, second)`.
///
/// Pre-#91 this Howard Hinnant civil-from-days algorithm was
/// open-coded 6 different ways. Negative `secs` (mtimes from
/// pre-1970, possible on Unix filesystems) work correctly here
/// where the pre-#91 inline copies in `action_receipt::epoch_to_ymd_hms`
/// would have overflowed on the `as i64` cast or wrapped in
/// unsigned arithmetic.
pub fn unix_to_ymdhms(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let h = (sod / 3_600) as u32;
    let mi = ((sod / 60) % 60) as u32;
    let s = (sod % 60) as u32;
    let (y, mo, d) = days_to_ymd(days);
    (y, mo, d, h, mi, s)
}

/// Howard Hinnant's civil-from-days algorithm — converts unix-epoch
/// days (signed, can be negative) to `(year, month, day)`.
///
/// Faster + clearer than naive month-loop algorithms; handles
/// negative dates without special-casing.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_unix_i64_close_to_systemtime() {
        let a = now_unix_i64();
        let b = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (a - b).abs() <= 2,
            "now_unix_i64 within 2s of std: a={a} b={b}"
        );
    }

    #[test]
    fn now_unix_secs_matches_i64_at_a_glance() {
        let a = now_unix_secs() as i64;
        let b = now_unix_i64();
        assert!((a - b).abs() <= 2);
    }

    #[test]
    fn now_iso8601_well_formed() {
        let s = now_iso8601();
        assert!(s.ends_with('Z'), "ISO-8601 missing Z suffix: {s}");
        assert_eq!(s.len(), 20, "ISO-8601 length unexpected: {s}");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], "T");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
    }

    #[test]
    fn unix_to_ymdhms_epoch() {
        assert_eq!(unix_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_known_2024_fixture() {
        // 2024-09-30T00:00:00Z = 1_727_654_400
        assert_eq!(unix_to_ymdhms(1_727_654_400), (2024, 9, 30, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_negative_epoch() {
        // 1969-12-31T00:00:00Z = -86_400
        assert_eq!(unix_to_ymdhms(-86_400), (1969, 12, 31, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_leap_day() {
        // 2024-02-29T00:00:00Z = 1_709_164_800
        assert_eq!(unix_to_ymdhms(1_709_164_800), (2024, 2, 29, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_century_boundary() {
        // 2000-01-01T00:00:00Z (Y2K, also a Gregorian century-leap)
        // = 946_684_800
        assert_eq!(unix_to_ymdhms(946_684_800), (2000, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymdhms_time_component() {
        // 2024-09-30T12:34:56Z = 1_727_699_696
        assert_eq!(
            unix_to_ymdhms(1_727_654_400 + 12 * 3600 + 34 * 60 + 56),
            (2024, 9, 30, 12, 34, 56),
        );
    }

    #[test]
    fn now_unix_secs_checked_returns_some_under_normal_clock() {
        assert!(now_unix_secs_checked().is_some());
    }
}
