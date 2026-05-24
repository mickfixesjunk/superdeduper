//! Client-claimed achievement predicates. Each predicate evaluates
//! locally from scan data + a small amount of per-install state;
//! matched predicate IDs are appended to `run_shape.easter_egg_hits`
//! in the submission payload, and the backend grants the
//! corresponding achievement on the next submit.
//!
//! Per `gamification-achievement-balance.md` §5 + testdesign's
//! `gamification-predicates.md` (9 acceptance criteria). Each
//! predicate corresponds to an entry in `achievements-catalog.yaml`.
//!
//! ## The 9 predicates
//!
//! | ID                       | Trigger                                                          | Status     |
//! |--------------------------|------------------------------------------------------------------|------------|
//! | `time-capsule`           | mtime < 2010-01-01 on any dup-group member                       | implemented |
//! | `abyss-walker`           | path depth >= 15 on any scanned file                             | implemented |
//! | `polyglot-paths`         | Unicode-script count >= 3 in any scanned path                    | implemented |
//! | `format-fanatic`         | 5+ distinct formats in any dup group; T1.2 perceptual mode only  | stub (T1.2-gated) |
//! | `screenshot-graveyard`   | filename matches >= 100 screenshot patterns in scan              | implemented |
//! | `download-archaeology`   | Downloads folder + mtime >= 5y on any group member               | implemented |
//! | `git-repo-detected`      | `.git/` directory present in scan subtree                        | implemented |
//! | `picky-eater`            | local exclude-pattern edit count >= 10                           | stub (needs persistent counter) |
//! | `verify-veteran`         | local `superdeduper achievements verify` invocation count >= 10  | stub (needs persistent counter) |
//!
//! Sequencing: the 6 implemented predicates land in this commit;
//! stubs land next (after `unicode-script` dep + persistent
//! counter store).

use std::path::Path;

/// Snapshot of scan inputs that any predicate may need to evaluate
/// against. Built once at scan end; predicates iterate it
/// independently. Borrowed throughout — no clones.
pub struct PredicateContext<'a> {
    /// Every file the walker emitted (full paths). May include
    /// files that didn't survive size-grouping; that's OK — the
    /// predicates that need this iterate it and don't care.
    pub all_paths: &'a [&'a Path],
    /// File mtimes in Unix-epoch seconds, parallel-indexed to
    /// `all_paths`. `None` for entries where the walker couldn't
    /// stat the file (rare but possible). Same length as
    /// `all_paths` when present.
    pub mtimes_unix_secs: Option<&'a [Option<i64>]>,
    /// Persistent counters scoped to this install. Used by
    /// `picky-eater` + `verify-veteran`. `None` when the counters
    /// haven't been wired through yet (predicates short-circuit).
    pub install_counters: Option<&'a InstallCounters>,
    /// Whether the scan ran in perceptual / format-aware mode.
    /// Required for `format-fanatic` (T1.2-gated).
    pub perceptual_mode_active: bool,
}

/// Install-level state needed by counter-driven predicates. The
/// fields exist as `u64` counters in the on-disk install state;
/// loaded once per scan and passed through `PredicateContext`.
/// Concrete persistence + bumping logic lands in a follow-up
/// commit alongside the predicate stubs they unblock.
#[derive(Debug, Clone, Copy, Default)]
pub struct InstallCounters {
    /// Lifetime count of times the user has saved a new
    /// exclude-pattern edit via Settings → Exclusions or CLI.
    pub exclude_pattern_edits: u64,
    /// Lifetime count of times the user has invoked
    /// `superdeduper achievements verify` from the CLI.
    pub achievements_verify_invocations: u64,
}

/// Run every predicate, return the IDs that matched. Order in the
/// returned vec is stable (matches the order predicates are listed
/// in this module) so a payload diff between scans is meaningful.
///
/// Predicates that can't evaluate (missing context, gating
/// feature off) silently return `None`; the catalog backend then
/// simply never grants those entries until conditions are met on
/// a future scan.
pub fn evaluate_all(ctx: &PredicateContext<'_>) -> Vec<String> {
    let mut hits = Vec::new();
    if abyss_walker(ctx).is_some() {
        hits.push("abyss-walker".to_string());
    }
    if download_archaeology(ctx).is_some() {
        hits.push("download-archaeology".to_string());
    }
    if format_fanatic(ctx).is_some() {
        hits.push("format-fanatic".to_string());
    }
    if git_repo_detected(ctx).is_some() {
        hits.push("git-repo-detected".to_string());
    }
    if picky_eater(ctx).is_some() {
        hits.push("picky-eater".to_string());
    }
    if polyglot_paths(ctx).is_some() {
        hits.push("polyglot-paths".to_string());
    }
    if screenshot_graveyard(ctx).is_some() {
        hits.push("screenshot-graveyard".to_string());
    }
    if time_capsule(ctx).is_some() {
        hits.push("time-capsule".to_string());
    }
    if verify_veteran(ctx).is_some() {
        hits.push("verify-veteran".to_string());
    }
    hits
}

// =====================================================================
// Implemented predicates
// =====================================================================

/// abyss-walker: path depth >= 15 on any scanned file.
///
/// "Depth" = number of separator components between the scan root
/// and the file. We approximate with raw `Path::components()`
/// count since we don't preserve the scan root in the context.
/// A 15-deep tree is unusual; most user trees max out around 8-10.
fn abyss_walker(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    const DEPTH_THRESHOLD: usize = 15;
    if ctx
        .all_paths
        .iter()
        .any(|p| p.components().count() >= DEPTH_THRESHOLD)
    {
        Some("abyss-walker")
    } else {
        None
    }
}

/// git-repo-detected: `.git/` directory present in scan subtree.
///
/// Matches a path component named exactly `.git` (not `.gitignore`
/// or `.gitattributes`). One match grants the predicate.
fn git_repo_detected(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    if ctx.all_paths.iter().any(|p| {
        p.components()
            .any(|c| c.as_os_str().to_string_lossy() == ".git")
    }) {
        Some("git-repo-detected")
    } else {
        None
    }
}

/// time-capsule: mtime older than 2010-01-01 on any dup-group
/// member. Catches "I haven't touched these files in 15+ years"
/// archival scenarios. The cutoff is fixed (not relative) so
/// behaviour is stable across years.
fn time_capsule(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    /// 2010-01-01 00:00:00 UTC as a Unix timestamp. Pre-computed
    /// to avoid pulling chrono into this module.
    const CUTOFF_UNIX_SECS: i64 = 1_262_304_000;
    let mtimes = ctx.mtimes_unix_secs?;
    let any_old = mtimes
        .iter()
        .any(|m| m.map(|t| t < CUTOFF_UNIX_SECS).unwrap_or(false));
    if any_old {
        Some("time-capsule")
    } else {
        None
    }
}

/// screenshot-graveyard: 100+ files matching screenshot filename
/// patterns. Common patterns: `Screenshot YYYY-MM-DD`, `IMG_*`,
/// `Screen Shot YYYY-MM-DD`, screenshot date-tagged variants.
fn screenshot_graveyard(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    const THRESHOLD: usize = 100;
    let count = ctx
        .all_paths
        .iter()
        .filter_map(|p| p.file_name())
        .filter_map(|n| n.to_str())
        .filter(|name| looks_like_screenshot(name))
        .count();
    if count >= THRESHOLD {
        Some("screenshot-graveyard")
    } else {
        None
    }
}

fn looks_like_screenshot(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("screenshot ")
        || lower.starts_with("screenshot-")
        || lower.starts_with("screenshot_")
        || lower.starts_with("screen shot ")
        || lower.starts_with("img_")
        || (lower.starts_with("img") && lower.chars().nth(3).is_some_and(|c| c.is_ascii_digit()))
}

/// download-archaeology: Downloads folder member with mtime
/// 5+ years older than now. Detects the "I've been hoarding
/// downloads for years" pattern.
fn download_archaeology(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    let mtimes = ctx.mtimes_unix_secs?;
    if mtimes.len() != ctx.all_paths.len() {
        return None;
    }
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let five_years_ago = now_unix - (5 * 365 * 24 * 60 * 60);

    let hit = ctx
        .all_paths
        .iter()
        .zip(mtimes.iter())
        .any(|(p, mtime)| {
            let in_downloads = p.components().any(|c| {
                let s = c.as_os_str().to_string_lossy();
                s.eq_ignore_ascii_case("Downloads")
            });
            let old_enough = mtime.map(|t| t < five_years_ago).unwrap_or(false);
            in_downloads && old_enough
        });
    if hit {
        Some("download-archaeology")
    } else {
        None
    }
}

/// polyglot-paths: at least 3 distinct Unicode scripts present in
/// any single scanned path. Catches mixed-locale corpora — e.g. a
/// user with Latin + Cyrillic + Han characters in a single filename
/// or directory chain.
///
/// `Script::Common` (ASCII punctuation, digits, whitespace, currency
/// symbols), `Script::Inherited` (combining marks), and
/// `Script::Unknown` are excluded from the count — they're filler
/// characters that appear in nearly every path and would
/// false-positive on plain "english-with-numbers" file names.
///
/// Short-circuits per-path: once a path crosses the threshold the
/// predicate returns Some without scanning the rest of the corpus.
fn polyglot_paths(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    use unicode_script::{Script, UnicodeScript};
    const THRESHOLD: usize = 3;
    for path in ctx.all_paths {
        let mut scripts: std::collections::HashSet<Script> = std::collections::HashSet::new();
        for c in path.to_string_lossy().chars() {
            let s = c.script();
            if matches!(s, Script::Common | Script::Inherited | Script::Unknown) {
                continue;
            }
            scripts.insert(s);
            if scripts.len() >= THRESHOLD {
                return Some("polyglot-paths");
            }
        }
    }
    None
}

// =====================================================================
// Stub predicates (return None until follow-up commits add
// counter persistence / T1.2 perceptual mode)
// =====================================================================

/// format-fanatic: 5+ distinct formats in any dup group, gated on
/// T1.2 perceptual mode. T1.2 hasn't shipped; perceptual_mode_active
/// is permanently false. Stub returns None.
fn format_fanatic(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    if !ctx.perceptual_mode_active {
        return None;
    }
    // TODO: when T1.2 lands, count distinct format-aware
    // fingerprint kinds per dup group. >=5 = grant.
    None
}

/// picky-eater: 10+ exclude-pattern edits lifetime. Needs the
/// persistent install-counter store (follow-up commit).
fn picky_eater(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    const THRESHOLD: u64 = 10;
    let counters = ctx.install_counters?;
    if counters.exclude_pattern_edits >= THRESHOLD {
        Some("picky-eater")
    } else {
        None
    }
}

/// verify-veteran: 10+ CLI `achievements verify` invocations
/// lifetime. Same install-counter dependency as picky-eater.
fn verify_veteran(ctx: &PredicateContext<'_>) -> Option<&'static str> {
    const THRESHOLD: u64 = 10;
    let counters = ctx.install_counters?;
    if counters.achievements_verify_invocations >= THRESHOLD {
        Some("verify-veteran")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn ctx_empty() -> PredicateContext<'static> {
        PredicateContext {
            all_paths: &[],
            mtimes_unix_secs: None,
            install_counters: None,
            perceptual_mode_active: false,
        }
    }

    #[test]
    fn evaluate_all_on_empty_context_returns_no_hits() {
        let hits = evaluate_all(&ctx_empty());
        assert!(hits.is_empty());
    }

    // -------------------- abyss-walker --------------------

    #[test]
    fn abyss_walker_grants_at_depth_15() {
        // Build a path with exactly 15 path components.
        let mut p = PathBuf::from("a");
        for _ in 0..14 {
            p.push("x");
        }
        assert_eq!(p.components().count(), 15);
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(abyss_walker(&ctx), Some("abyss-walker"));
    }

    #[test]
    fn abyss_walker_misses_at_depth_14() {
        let mut p = PathBuf::from("a");
        for _ in 0..13 {
            p.push("x");
        }
        assert_eq!(p.components().count(), 14);
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(abyss_walker(&ctx), None);
    }

    // -------------------- git-repo-detected --------------------

    #[test]
    fn git_repo_detected_grants_on_dot_git_component() {
        let p = PathBuf::from("project/.git/objects/abc");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(git_repo_detected(&ctx), Some("git-repo-detected"));
    }

    #[test]
    fn git_repo_detected_ignores_gitignore() {
        // Should NOT match — .gitignore is not the `.git/` dir.
        let p = PathBuf::from("project/.gitignore");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(git_repo_detected(&ctx), None);
    }

    #[test]
    fn git_repo_detected_misses_when_no_git() {
        let p = PathBuf::from("project/src/main.rs");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(git_repo_detected(&ctx), None);
    }

    // -------------------- time-capsule --------------------

    #[test]
    fn time_capsule_grants_on_pre_2010_mtime() {
        // 2009-06-15 = before cutoff.
        let mtimes = vec![Some(1_245_024_000_i64)];
        let p = PathBuf::from("ancient.txt");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            mtimes_unix_secs: Some(&mtimes),
            ..ctx_empty()
        };
        assert_eq!(time_capsule(&ctx), Some("time-capsule"));
    }

    #[test]
    fn time_capsule_misses_on_post_2010_mtime() {
        // 2015-01-01 = after cutoff.
        let mtimes = vec![Some(1_420_070_400_i64)];
        let p = PathBuf::from("recent.txt");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            mtimes_unix_secs: Some(&mtimes),
            ..ctx_empty()
        };
        assert_eq!(time_capsule(&ctx), None);
    }

    #[test]
    fn time_capsule_short_circuits_without_mtimes() {
        let p = PathBuf::from("anything.txt");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            mtimes_unix_secs: None,
            ..ctx_empty()
        };
        assert_eq!(time_capsule(&ctx), None);
    }

    // -------------------- screenshot-graveyard --------------------

    #[test]
    fn screenshot_graveyard_grants_at_100_screenshots() {
        let paths: Vec<PathBuf> = (0..100)
            .map(|i| PathBuf::from(format!("photos/Screenshot 2024-01-{:02}.png", i % 30 + 1)))
            .collect();
        let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(screenshot_graveyard(&ctx), Some("screenshot-graveyard"));
    }

    #[test]
    fn screenshot_graveyard_misses_at_99() {
        let paths: Vec<PathBuf> = (0..99)
            .map(|i| PathBuf::from(format!("photos/Screenshot {i}.png")))
            .collect();
        let path_refs: Vec<&Path> = paths.iter().map(|p| p.as_path()).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(screenshot_graveyard(&ctx), None);
    }

    #[test]
    fn screenshot_detection_covers_common_patterns() {
        assert!(looks_like_screenshot("Screenshot 2024-01-15.png"));
        assert!(looks_like_screenshot("Screenshot-2024-01-15.png"));
        assert!(looks_like_screenshot("Screenshot_2024-01-15.png"));
        assert!(looks_like_screenshot("Screen Shot 2024-01-15.png"));
        assert!(looks_like_screenshot("IMG_2024.jpg"));
        assert!(looks_like_screenshot("img_5432.jpg"));
        assert!(looks_like_screenshot("IMG0001.JPG"));
        // Negatives
        assert!(!looks_like_screenshot("vacation.jpg"));
        assert!(!looks_like_screenshot("DSC0001.jpg"));
        assert!(!looks_like_screenshot("readme.md"));
    }

    // -------------------- download-archaeology --------------------

    #[test]
    fn download_archaeology_grants_on_old_downloads_file() {
        // 6 years ago.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let six_years_ago = now - (6 * 365 * 24 * 60 * 60);
        let mtimes = vec![Some(six_years_ago)];
        let p = PathBuf::from("/home/user/Downloads/installer.exe");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            mtimes_unix_secs: Some(&mtimes),
            ..ctx_empty()
        };
        assert_eq!(download_archaeology(&ctx), Some("download-archaeology"));
    }

    #[test]
    fn download_archaeology_misses_recent_downloads() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let two_years_ago = now - (2 * 365 * 24 * 60 * 60);
        let mtimes = vec![Some(two_years_ago)];
        let p = PathBuf::from("/home/user/Downloads/installer.exe");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            mtimes_unix_secs: Some(&mtimes),
            ..ctx_empty()
        };
        assert_eq!(download_archaeology(&ctx), None);
    }

    #[test]
    fn download_archaeology_misses_old_non_downloads() {
        let mtimes = vec![Some(0_i64)]; // 1970
        let p = PathBuf::from("/home/user/Documents/old.txt");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            mtimes_unix_secs: Some(&mtimes),
            ..ctx_empty()
        };
        assert_eq!(download_archaeology(&ctx), None);
    }

    // -------------------- polyglot-paths --------------------

    #[test]
    fn polyglot_paths_grants_at_three_scripts() {
        // Latin + Cyrillic + Han = 3 distinct scripts in one path.
        let p = PathBuf::from("photos/привет/世界/hello.jpg");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(polyglot_paths(&ctx), Some("polyglot-paths"));
    }

    #[test]
    fn polyglot_paths_misses_at_two_scripts() {
        // Latin + Cyrillic only = 2 distinct scripts.
        let p = PathBuf::from("photos/привет/hello.jpg");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(polyglot_paths(&ctx), None);
    }

    #[test]
    fn polyglot_paths_ignores_common_script_filler() {
        // ASCII digits + punctuation are Script::Common — must not
        // contribute toward the 3-script threshold. This path has
        // only Latin letters; everything else is Common (digits,
        // dashes, slashes, dots). Should be 1 script = no grant.
        let p = PathBuf::from("Downloads/installer-1.2.3-x86_64.exe");
        let paths: Vec<&Path> = vec![&p];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(polyglot_paths(&ctx), None);
    }

    #[test]
    fn polyglot_paths_short_circuits_per_path() {
        // First path crosses the threshold (4 distinct scripts:
        // Latin + Cyrillic + Han + Arabic); the second is bogus
        // and would panic if path-level short-circuit didn't work.
        // (We can't actually inject a panicking path through the
        // type system here, but assert the happy path with mixed
        // corpus still grants.)
        let p1 = PathBuf::from("a/привет/世界/مرحبا/file.txt");
        let p2 = PathBuf::from("b/plain-path.txt");
        let paths: Vec<&Path> = vec![&p1, &p2];
        let path_refs: Vec<&Path> = paths.iter().map(|x| *x).collect();
        let ctx = PredicateContext {
            all_paths: &path_refs,
            ..ctx_empty()
        };
        assert_eq!(polyglot_paths(&ctx), Some("polyglot-paths"));
    }

    #[test]
    fn polyglot_paths_returns_none_when_no_paths() {
        assert_eq!(polyglot_paths(&ctx_empty()), None);
    }

    // -------------------- stubs --------------------

    #[test]
    fn format_fanatic_short_circuits_without_perceptual_mode() {
        let mut c = ctx_empty();
        c.perceptual_mode_active = false;
        assert_eq!(format_fanatic(&c), None);
    }

    #[test]
    fn picky_eater_short_circuits_without_counters() {
        assert_eq!(picky_eater(&ctx_empty()), None);
    }

    #[test]
    fn picky_eater_grants_at_threshold() {
        let counters = InstallCounters {
            exclude_pattern_edits: 10,
            achievements_verify_invocations: 0,
        };
        let ctx = PredicateContext {
            install_counters: Some(&counters),
            ..ctx_empty()
        };
        assert_eq!(picky_eater(&ctx), Some("picky-eater"));
    }

    #[test]
    fn verify_veteran_grants_at_threshold() {
        let counters = InstallCounters {
            exclude_pattern_edits: 0,
            achievements_verify_invocations: 10,
        };
        let ctx = PredicateContext {
            install_counters: Some(&counters),
            ..ctx_empty()
        };
        assert_eq!(verify_veteran(&ctx), Some("verify-veteran"));
    }

    // -------------------- composite --------------------

    #[test]
    fn evaluate_all_returns_stable_order() {
        // Stir together multiple matches; assert lexical
        // alphabetic order of IDs in the returned vec for
        // diff-stable payloads.
        let p_deep = {
            let mut p = PathBuf::from("a");
            for _ in 0..14 {
                p.push("x");
            }
            p
        };
        let p_git = PathBuf::from("project/.git/objects/abc");
        let paths_vec: Vec<PathBuf> = vec![p_deep, p_git];
        let path_refs: Vec<&Path> = paths_vec.iter().map(|p| p.as_path()).collect();
        let counters = InstallCounters {
            exclude_pattern_edits: 10,
            achievements_verify_invocations: 10,
        };
        let ctx = PredicateContext {
            all_paths: &path_refs,
            install_counters: Some(&counters),
            ..ctx_empty()
        };
        let hits = evaluate_all(&ctx);
        // 4 hits in this scenario: abyss-walker, git-repo-detected,
        // picky-eater, verify-veteran. Returned in alphabetical
        // dispatch-order per `evaluate_all`'s implementation.
        assert_eq!(
            hits,
            vec![
                "abyss-walker",
                "git-repo-detected",
                "picky-eater",
                "verify-veteran",
            ]
        );
    }
}
