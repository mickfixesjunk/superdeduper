//! Scored heuristic for "which file in a duplicate group is the
//! one to keep". Used by [`crate::cli::KeepStrategy::Smart`].
//!
//! Design notes:
//!
//! * Each file's score is a sum of signed component scores. Higher
//!   = more likely to be the "canonical" / "organised" copy.
//! * Components are deliberately small in number and obvious.
//!   Tuning this is data-driven: when a user complains "you
//!   picked the wrong file as keeper", the breakdown
//!   ([`KeepScore::breakdown`]) tells us exactly which signals
//!   fired and we add or adjust accordingly.
//! * No filesystem reads beyond `metadata().modified()` —
//!   scoring must stay fast enough to apply to every group in a
//!   half-million-file scan.
//! * Pure-Rust, no AI. The LLM tiebreak in Phase 2 will be
//!   invoked only when this picker's top two are within a small
//!   margin of each other.

use std::path::Path;
use std::time::SystemTime;

/// Fetch a file's mtime as `Option<SystemTime>`. Returns `None`
/// when the file can't be stat'd (deleted between scan + keep
/// decision, permission denied, etc.) so callers can treat
/// missing-mtime as a tiebreak input rather than a hard error.
///
/// Single source of truth for the `fs::metadata(p).and_then(|m|
/// m.modified()).ok()` pattern that previously lived in four
/// keeper-strategy call sites (per quality audit F9 /
/// #68-absorbed). Centralised here so a future refactor —
/// e.g. cache mtime in `FileEntry` to avoid the per-row stat —
/// has one call site to update.
pub fn file_mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// One file's score with the per-component breakdown that
/// produced it. The breakdown is shown in the GUI tooltip when
/// the user hovers the keeper badge, and goes into logs so a
/// surprising pick can be audited without re-running.
#[derive(Debug, Clone, Default)]
pub struct KeepScore {
    pub total: f32,
    pub breakdown: Vec<(&'static str, f32)>,
}

impl KeepScore {
    fn add(&mut self, label: &'static str, delta: f32) {
        self.breakdown.push((label, delta));
        self.total += delta;
    }
}

/// Score one candidate. The `mtime` lets us reward newer files
/// mildly without an extra `metadata()` call inside the
/// per-component code. `None` means we couldn't stat; the
/// mtime-newness component just contributes 0.
pub fn score_file(path: &Path, mtime: Option<SystemTime>) -> KeepScore {
    let mut s = KeepScore::default();
    // Strip the Windows `\\?\` verbatim prefix (S15) via the canonical
    // helper, THEN normalise to forward slashes. The substring
    // location checks below are prefix-immune, but the `/`-count depth
    // signal (a few lines down) is NOT — a raw verbatim path would
    // count `//?/` as bogus segments and inflate the depth score. The
    // strip keeps depth comparable across verbatim, clean-Windows, and
    // Linux test paths. std::path::Path doesn't parse Windows
    // separators off-Windows, so the contains-checks operate on the
    // string form.
    let path_str = crate::path_display::for_user_display(path).replace('\\', "/");
    let lower = path_str.to_lowercase();

    // ---------- Location quality (negative signals) ----------

    // Recycle Bin almost certainly isn't the canonical copy.
    // Heavy penalty so even a recently-recycled file loses to
    // any normal location.
    if lower.contains("$recycle.bin") || lower.contains("/recycler/") {
        s.add("recycle-bin", -100.0);
    }

    // Common transient / cache locations. Penalty smaller than
    // recycle bin but big enough to lose to any "real" path.
    for needle in [
        "/temp/",
        "/tmp/",
        "/cache/",
        "/caches/",
        "/node_modules/",
        "/.cargo/",
        "/.rustup/",
        "/target/debug/",
        "/target/release/",
        "/__pycache__/",
        "/.gradle/",
        "/.npm/",
        "/.yarn/",
        "appdata/local/temp",
    ] {
        if lower.contains(needle) {
            s.add("transient-location", -50.0);
            break; // one penalty per file
        }
    }

    // Desktop is usually a staging area, not a final home.
    // Penalty smaller than transient so a Desktop file still
    // beats a temp file.
    if lower.contains("/desktop/") {
        s.add("desktop-dump", -10.0);
    }

    // Downloads = "arrived here, hasn't been organised yet".
    if lower.contains("/downloads/") {
        s.add("downloads-staging", -8.0);
    }

    // OneDrive/Dropbox/Google Drive top-level locations are
    // usually the user's primary file store — mildly positive.
    for needle in ["/onedrive/", "/dropbox/", "/google drive/"] {
        if lower.contains(needle) {
            s.add("cloud-sync-root", 5.0);
            break;
        }
    }

    // ---------- Path depth (organisational signal) ----------

    // A file at C:/Users/X/Documents/Projects/Foo/file.docx is
    // more likely to be "the organised one" than C:/file.docx.
    // Counting `/` segments in the normalised string works on
    // both Windows and Linux test paths; Path::components() does
    // not (Linux treats a Windows-style path as one opaque
    // segment).
    let depth = lower.matches('/').count() as f32;
    let depth_score = (depth - 2.0).clamp(0.0, 10.0);
    if depth_score > 0.0 {
        s.add("depth", depth_score);
    }

    // ---------- Filename patterns ----------

    // Filename = substring after the last '/' in the normalised
    // path. Works for both real Windows paths (post-replace) and
    // Linux test paths.
    let lname = lower.rsplit('/').next().unwrap_or("").to_string();

    // Final / canonical markers.
    for needle in ["_final", "-final", " final.", ".final.", "final_"] {
        if lname.contains(needle) {
            s.add("final-marker", 10.0);
            break;
        }
    }

    // Draft / WIP markers — the opposite signal.
    for needle in [
        "_draft", "-draft", " draft.", ".draft.", "draft_", "_wip", "-wip", "wip_", "_old", "-old",
        "old_", "_bak", ".bak", ".bak.", ".backup", "_backup", "-backup",
    ] {
        if lname.contains(needle) {
            s.add("draft-marker", -10.0);
            break;
        }
    }

    // "Copy of …" / " - Copy" / " (1)" markers — Windows /
    // Explorer adds these automatically when the user copies a
    // file into a folder that already has one. They're almost
    // never the keeper.
    if lname.starts_with("copy of ") || lname.starts_with("copy-of-") {
        s.add("copy-of-prefix", -20.0);
    }
    if lname.contains(" - copy") {
        s.add("copy-suffix", -15.0);
    }
    // " (1)", " (2)", … " (99)" — Explorer auto-suffix.
    if let Some(paren_open) = lname.rfind(" (") {
        if let Some(paren_close) = lname[paren_open..].find(')') {
            let inner = &lname[paren_open + 2..paren_open + paren_close];
            if !inner.is_empty() && inner.chars().all(|c| c.is_ascii_digit()) {
                let n: u32 = inner.parse().unwrap_or(0);
                // n=1 is -3, n=2 is -6, etc., capped at -12.
                let pen = (n as f32 * -3.0).max(-12.0);
                s.add("paren-number-suffix", pen);
            }
        }
    }

    // ---------- mtime ----------

    // Most users want the most recent of two indistinguishable
    // files. Small positive contribution proportional to age
    // recency — capped so it can't overwhelm location signals.
    if let Some(t) = mtime {
        if let Ok(d) = SystemTime::now().duration_since(t) {
            let days = d.as_secs() as f32 / 86_400.0;
            // 0 days old → +3, 30 days → +1.5, 365+ → 0.
            let recency = (3.0 - (days / 60.0)).clamp(0.0, 3.0);
            if recency > 0.0 {
                s.add("recency", recency);
            }
        }
    }

    s
}

/// Tie tolerance used by `pick_keeper` when comparing two
/// candidate scores. `f32::EPSILON` (~1.19e-7) is the gap between
/// 1.0 and the next representable f32 — NOT a meaningful tolerance
/// for sums of components in the ±100 range. The actual scoring
/// components are integer-valued constants (-100, -50, -20, -15,
/// -10, -8, 5, 10) and recency is the only float-valued
/// contributor, so accumulated rounding error is normally exactly
/// zero in practice — but a sane tolerance is required if anyone
/// adds a second float-valued signal.
///
/// `1e-4` covers anything that comes from `recency = (3.0 - days /
/// 60.0).clamp(0.0, 3.0)` because the days-since-mtime float is
/// stable to many decimal digits + the multiplication+subtraction
/// has bounded rounding error. The relative term `best * 1e-6`
/// scales the tolerance up for unusually large scores; for the
/// typical ±100 range it stays below the 1e-4 absolute floor +
/// the floor wins.
fn score_tie_tolerance(best: f32) -> f32 {
    (best.abs() * 1e-6).max(1e-4)
}

/// Pick the highest-scoring index from a slice of paths. Ties on
/// score are broken by newest `mtime`; both-`None` mtimes pick
/// the first.
///
/// **Single source of truth** for the Smart-keeper tiebreak. Both
/// the GUI flow (via `gui::live::order_keeper_first`) and the CLI
/// flow (via `dedupe::pick_keeper`'s Smart arm) call into this
/// fn so any future tiebreak-signal addition lands in exactly one
/// place. See #68 (refactor to eliminate GUI↔CLI drift risk).
pub fn pick_keeper<P: AsRef<Path>>(paths: &[P], mtimes: &[Option<SystemTime>]) -> usize {
    assert_eq!(paths.len(), mtimes.len(), "paths/mtimes length mismatch");
    let mut best_idx = 0usize;
    let mut best_score = f32::NEG_INFINITY;
    let mut best_mtime: Option<SystemTime> = None;
    for (i, (p, m)) in paths.iter().zip(mtimes.iter()).enumerate() {
        let s = score_file(p.as_ref(), *m).total;
        let take = if s > best_score {
            true
        } else if (s - best_score).abs() < score_tie_tolerance(best_score) {
            // Tie — newer wins.
            match (best_mtime, *m) {
                (Some(cur), Some(t)) if t > cur => true,
                (None, Some(_)) => true,
                _ => false,
            }
        } else {
            false
        };
        if take {
            best_idx = i;
            best_score = s;
            best_mtime = *m;
        }
    }
    best_idx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn recycle_bin_loses() {
        let a = score_file(&p(r"C:\$Recycle.Bin\S-1\foo.txt"), None);
        let b = score_file(&p(r"C:\Users\me\Documents\foo.txt"), None);
        assert!(b.total > a.total, "{a:?} vs {b:?}");
    }

    #[test]
    fn final_beats_draft() {
        let draft = score_file(&p(r"C:\work\report_draft.docx"), None);
        let fin = score_file(&p(r"C:\work\report_final.docx"), None);
        assert!(fin.total > draft.total, "draft {draft:?} vs final {fin:?}");
    }

    #[test]
    fn copy_of_loses_to_original() {
        let orig = score_file(&p(r"C:\work\report.docx"), None);
        let copy = score_file(&p(r"C:\work\Copy of report.docx"), None);
        assert!(orig.total > copy.total, "{orig:?} vs {copy:?}");
    }

    #[test]
    fn paren_number_loses_to_original() {
        let orig = score_file(&p(r"C:\work\photo.jpg"), None);
        let dup = score_file(&p(r"C:\work\photo (1).jpg"), None);
        assert!(orig.total > dup.total);
    }

    #[test]
    fn organised_path_beats_root_dump() {
        let root = score_file(&p(r"C:\foo.txt"), None);
        let deep = score_file(&p(r"C:\Users\me\Documents\Projects\foo.txt"), None);
        assert!(deep.total > root.total);
    }

    #[test]
    fn desktop_loses_to_organised() {
        let desktop = score_file(&p(r"C:\Users\me\Desktop\foo.txt"), None);
        let organised = score_file(&p(r"C:\Users\me\Documents\Projects\foo.txt"), None);
        assert!(organised.total > desktop.total);
    }

    #[test]
    fn temp_loses_to_desktop() {
        let tmp = score_file(&p(r"C:\Users\me\AppData\Local\Temp\foo.txt"), None);
        let desk = score_file(&p(r"C:\Users\me\Desktop\foo.txt"), None);
        assert!(desk.total > tmp.total);
    }

    #[test]
    fn pick_keeper_obeys_total() {
        let paths = [
            p(r"C:\Users\me\Documents\report.docx"),
            p(r"C:\Users\me\Desktop\Copy of report.docx"),
            p(r"C:\$Recycle.Bin\report.docx"),
        ];
        let mtimes = [None, None, None];
        assert_eq!(pick_keeper(&paths, &mtimes), 0);
    }

    /// #68 — tie tolerance is `(best * 1e-6).max(1e-4)`, NOT
    /// `f32::EPSILON` (~1.19e-7). Two paths with identical scoring
    /// signals should tie + newest-mtime wins.
    #[test]
    fn pick_keeper_tie_uses_newest_mtime() {
        // Two paths with the same `score_file` total (both in
        // identical "user Documents" treatment + no special
        // tokens). Mtimes differ; newest wins.
        let paths = [
            p(r"C:\Users\me\Documents\report-a.docx"),
            p(r"C:\Users\me\Documents\report-b.docx"),
        ];
        let earlier =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1700000000);
        let later = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1779594153);
        let mtimes = [Some(earlier), Some(later)];
        assert_eq!(
            pick_keeper(&paths, &mtimes),
            1,
            "newer mtime should win the tie"
        );
        // Reversed order — first should win when mtimes flipped.
        let mtimes_rev = [Some(later), Some(earlier)];
        assert_eq!(
            pick_keeper(&paths, &mtimes_rev),
            0,
            "newer mtime should win regardless of slice order"
        );
    }

    /// #68 — tolerance lets tiny float-rounding deltas count as
    /// ties (was a no-op with f32::EPSILON since rounding errors
    /// at the typical score range exceed f32::EPSILON only in
    /// pathological inputs; this test pins the behaviour
    /// regardless).
    #[test]
    fn score_tie_tolerance_absolute_floor_holds_at_typical_range() {
        // Tolerance for best=0.0 → 1e-4 (absolute floor)
        assert!(super::score_tie_tolerance(0.0) >= 1e-4 - 1e-9);
        // Tolerance for best=10 → still 1e-4 (1e-6 * 10 = 1e-5 < floor)
        assert!(super::score_tie_tolerance(10.0) >= 1e-4 - 1e-9);
        // Tolerance for best=1_000_000 → 1.0 (1e-6 * 1e6 = 1 > floor)
        assert!(super::score_tie_tolerance(1_000_000.0) >= 1.0 - 1e-9);
    }
}
