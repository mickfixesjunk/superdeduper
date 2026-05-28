//! Submission-payload metadata helpers shared by the CLI
//! (`src/main.rs`'s `run_scan`) and the GUI
//! (`src/gui/live.rs`'s engine thread).
//!
//! #142 — Pre-#142 these helpers lived only in `gui::live` because
//! only the GUI built a submission payload at scan-finish. The CLI
//! flow's `run_scan` wrote a scan_history row WITHOUT a payload,
//! which made every CLI-scanned row invisible to `submit-pending`
//! (it skips rows with no payload), blocking testdesign's e2e
//! flow + every pathfinder achievement that depends on a CLI
//! submission landing on the leaderboard.
//!
//! Moved here so both surfaces compute scope / corpus_kind /
//! share_count_in_scope from the same source.

#![cfg(feature = "telemetry")]

use std::path::{Path, PathBuf};

/// Scope heuristic: `"selection"` (multi-root), `"whole-volume"`
/// (single drive-root), or `"subdirectory"` (single non-root path).
/// Backend uses this for run-shape bucketing.
pub fn classify_scope(roots: &[PathBuf]) -> String {
    if roots.len() > 1 {
        return "selection".to_string();
    }
    match roots.first() {
        Some(p) if is_drive_root(p) => "whole-volume".to_string(),
        Some(_) => "subdirectory".to_string(),
        None => "subdirectory".to_string(),
    }
}

/// `"system"` if any root path looks like an OS-system tree;
/// otherwise `"user-data"`. Conservative heuristic — the backend
/// uses this for category bucketing.
pub fn classify_corpus_kind(roots: &[PathBuf]) -> String {
    for p in roots {
        let s = p.to_string_lossy().to_ascii_lowercase();
        if s.contains("\\windows\\")
            || s.ends_with("\\windows")
            || s.contains("/system/")
            || s.starts_with("/system")
            || s.contains("\\program files")
            || s.starts_with("/usr/")
            || s.starts_with("/bin/")
            || s.starts_with("/sbin/")
        {
            return "system".to_string();
        }
    }
    "user-data".to_string()
}

/// True if `p` looks like a whole-volume root. Windows `C:\`,
/// verbatim `\\?\C:\`, or Unix `/`.
pub fn is_drive_root(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s == "/"
        || (s.len() == 3 && s.chars().nth(1) == Some(':') && s.ends_with('\\'))
        || (s.len() == 7 && s.starts_with("\\\\?\\") && s.ends_with('\\'))
}

/// True if `p` looks like a network share — Windows UNC
/// (`\\server\share\...` or its verbatim form `\\?\UNC\...`) or a
/// URL-style network-FS scheme (`smb://`, `nfs://`, `cifs://`).
/// Used by `count_distinct_share_roots` + as a feature-bit input
/// elsewhere in the submission build.
pub fn is_network_share_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'\\' && bytes[1] == b'\\' {
        let prefix3 = bytes.get(2).copied();
        if prefix3 != Some(b'?') && prefix3 != Some(b'.') {
            return true;
        }
        if s.starts_with("\\\\?\\UNC\\") {
            return true;
        }
    }
    s.starts_with("smb://") || s.starts_with("nfs://") || s.starts_with("cifs://")
}

/// Count distinct `\\server\share` (or URL-authority) groupings
/// among `paths`. Multiple roots into the same share count once.
/// Backend uses this for the `multi-share-maestro` latent grant.
pub fn count_distinct_share_roots(paths: &[PathBuf]) -> u64 {
    use std::collections::HashSet;
    let mut shares: HashSet<String> = HashSet::new();
    for p in paths {
        if !is_network_share_path(p) {
            continue;
        }
        let s = p.to_string_lossy();
        let key = if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
            let two: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
            format!("unc:{}", two.join("\\"))
        } else if let Some(rest) = s.strip_prefix("\\\\") {
            let two: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
            format!("unc:{}", two.join("\\"))
        } else if let Some(rest) = s.strip_prefix("smb://") {
            let authority = rest.split('/').next().unwrap_or("");
            format!("smb:{authority}")
        } else if let Some(rest) = s.strip_prefix("nfs://") {
            let authority = rest.split('/').next().unwrap_or("");
            format!("nfs:{authority}")
        } else if let Some(rest) = s.strip_prefix("cifs://") {
            let authority = rest.split('/').next().unwrap_or("");
            format!("cifs:{authority}")
        } else {
            continue;
        };
        shares.insert(key);
    }
    shares.len() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn classify_scope_buckets() {
        assert_eq!(classify_scope(&[p("C:\\")]), "whole-volume");
        assert_eq!(classify_scope(&[p("/")]), "whole-volume");
        assert_eq!(classify_scope(&[p("/home/mick")]), "subdirectory");
        assert_eq!(
            classify_scope(&[p("/home/mick"), p("/tmp")]),
            "selection",
        );
        assert_eq!(classify_scope(&[]), "subdirectory");
    }

    #[test]
    fn classify_corpus_kind_buckets() {
        assert_eq!(
            classify_corpus_kind(&[p("C:\\Windows\\System32")]),
            "system",
        );
        assert_eq!(classify_corpus_kind(&[p("/usr/lib")]), "system");
        assert_eq!(classify_corpus_kind(&[p("/home/mick")]), "user-data");
        assert_eq!(classify_corpus_kind(&[]), "user-data");
    }

    #[test]
    fn is_network_share_path_recognises_common_forms() {
        assert!(is_network_share_path(&p("\\\\server\\share\\file")));
        assert!(is_network_share_path(&p("\\\\?\\UNC\\server\\share")));
        assert!(is_network_share_path(&p("smb://server/share")));
        assert!(is_network_share_path(&p("nfs://server/export")));
        assert!(!is_network_share_path(&p("\\\\?\\C:\\path")));
        assert!(!is_network_share_path(&p("C:\\path")));
        assert!(!is_network_share_path(&p("/home/mick")));
    }

    #[test]
    fn count_distinct_share_roots_groups_by_server_share() {
        let paths = vec![
            p("\\\\server\\share1\\a"),
            p("\\\\server\\share1\\b"),
            p("\\\\server\\share2\\c"),
            p("smb://other/export"),
            p("/home/mick"),
        ];
        // 3 distinct share roots: server\share1, server\share2, smb:other
        assert_eq!(count_distinct_share_roots(&paths), 3);
    }
}
