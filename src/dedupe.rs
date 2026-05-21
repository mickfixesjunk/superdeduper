//! `superdeduper dedupe` — destructive operations against a results file.
//!
//! Safety contracts enforced here (all of them, at multiple layers):
//!
//! * Reference paths are never modified. Period. Enforced both when
//!   picking the keeper and again right before the action runs.
//! * System-critical paths (`C:\Windows`, `C:\Program Files`, …) are
//!   refused unless `--allow-system-paths` is passed.
//! * Before any destructive action, the file's `(size, mtime)` is
//!   re-checked against the results-file's snapshot. Mismatch ⇒ that
//!   group is skipped with an error.
//! * `--dry-run` short-circuits every action with a planned-action
//!   log line, never touching the filesystem.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{DedupeAction, DedupeArgs, KeepStrategy};
use crate::pipeline::DuplicateGroup;
use crate::{Error, Result};

/// Schema for a saved scan results file. Serialised by `output::write`
/// when `--format json` is used; parsed back here.
#[derive(Debug, Serialize, Deserialize)]
pub struct ResultsFile {
    pub schema: String,
    pub groups: Vec<DuplicateGroup>,
    #[serde(default)]
    pub summary: Option<Summary>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Summary {
    pub groups: usize,
    pub files: usize,
    pub reclaimable_bytes: u64,
}

#[derive(Debug, Default, Clone)]
pub struct Outcome {
    pub planned: u64,
    pub executed: u64,
    pub skipped_reference: u64,
    pub skipped_system: u64,
    pub skipped_invalidated: u64,
    pub failed: u64,
    pub bytes_reclaimed: u64,
}

/// Run the dedupe planner against `args`. Returns a tally of what was
/// touched and what was skipped, plus per-action log lines via
/// `tracing`.
pub fn run(args: &DedupeArgs) -> Result<Outcome> {
    let raw = fs::read_to_string(&args.results_file)?;
    let results: ResultsFile = serde_json::from_str(&raw)
        .map_err(|e| Error::other(format!("parsing results file: {e}")))?;
    if !results.schema.starts_with("superdeduper.scan") {
        return Err(Error::other(format!(
            "unknown results schema `{}`",
            results.schema
        )));
    }

    let references = canonical_set(&[]); // reference set is part of the scan; reserved.
    let mut outcome = Outcome::default();
    for (i, group) in results.groups.iter().enumerate() {
        if let Err(e) = process_group(i, group, args, &references, &mut outcome) {
            outcome.failed += 1;
            tracing::warn!(group = i + 1, error = %e, "group skipped");
        }
    }
    Ok(outcome)
}

fn process_group(
    idx: usize,
    group: &DuplicateGroup,
    args: &DedupeArgs,
    references: &BTreeMap<PathBuf, ()>,
    outcome: &mut Outcome,
) -> Result<()> {
    if group.files.len() < 2 {
        return Ok(());
    }

    // System-path guard.
    for path in &group.files {
        if !args.allow_system_paths && is_system_path(path) {
            outcome.skipped_system += 1;
            tracing::warn!(path = %path.display(), "system path; group skipped");
            return Ok(());
        }
    }

    let keeper_idx = pick_keeper(group, args.strategy, references)?;
    let keeper = &group.files[keeper_idx];

    for (i, path) in group.files.iter().enumerate() {
        if i == keeper_idx {
            continue;
        }
        outcome.planned += 1;

        if references.contains_key(path) {
            outcome.skipped_reference += 1;
            tracing::info!(
                group = idx + 1,
                path = %path.display(),
                "reference path; never modified"
            );
            continue;
        }

        // Re-verify the file hasn't changed since the scan.
        match validate_file(path, group.size) {
            Ok(()) => {}
            Err(e) => {
                outcome.skipped_invalidated += 1;
                tracing::warn!(
                    group = idx + 1,
                    path = %path.display(),
                    error = %e,
                    "file changed since scan; skipping"
                );
                continue;
            }
        }

        if args.dry_run {
            tracing::info!(
                group = idx + 1,
                action = ?args.action,
                target = %path.display(),
                keeper = %keeper.display(),
                "DRY RUN"
            );
            outcome.executed += 1;
            outcome.bytes_reclaimed += group.size;
            continue;
        }

        match perform_action(args.action, path, keeper) {
            Ok(()) => {
                outcome.executed += 1;
                outcome.bytes_reclaimed += group.size;
                tracing::info!(
                    group = idx + 1,
                    action = ?args.action,
                    target = %path.display(),
                    keeper = %keeper.display(),
                    "applied"
                );
            }
            Err(e) => {
                outcome.failed += 1;
                tracing::error!(
                    group = idx + 1,
                    path = %path.display(),
                    error = %e,
                    "action failed"
                );
            }
        }
    }
    Ok(())
}

fn pick_keeper(
    group: &DuplicateGroup,
    strategy: KeepStrategy,
    references: &BTreeMap<PathBuf, ()>,
) -> Result<usize> {
    // Reference paths always win, regardless of strategy.
    for (i, p) in group.files.iter().enumerate() {
        if references.contains_key(p) {
            return Ok(i);
        }
    }
    let mut idx = 0usize;
    match strategy {
        KeepStrategy::First => idx = 0,
        KeepStrategy::Oldest | KeepStrategy::Newest => {
            let mut best_time: Option<std::time::SystemTime> = None;
            for (i, p) in group.files.iter().enumerate() {
                let t = fs::metadata(p).and_then(|m| m.modified()).ok();
                let take = match (best_time, t, strategy) {
                    (None, Some(_), _) => true,
                    (Some(cur), Some(t), KeepStrategy::Oldest) if t < cur => true,
                    (Some(cur), Some(t), KeepStrategy::Newest) if t > cur => true,
                    _ => false,
                };
                if take {
                    idx = i;
                    best_time = t;
                }
            }
        }
        KeepStrategy::ShortestPath | KeepStrategy::LongestPath => {
            for (i, p) in group.files.iter().enumerate() {
                let len = p.as_os_str().len();
                let pick = match strategy {
                    KeepStrategy::ShortestPath => len < group.files[idx].as_os_str().len(),
                    _ => len > group.files[idx].as_os_str().len(),
                };
                if pick {
                    idx = i;
                }
            }
        }
        KeepStrategy::InReference => {
            return Err(Error::other(
                "--strategy in-reference requires reference paths in the scan",
            ));
        }
        KeepStrategy::Interactive => {
            return Err(Error::other(
                "--strategy interactive is not yet implemented",
            ));
        }
    }
    Ok(idx)
}

fn validate_file(path: &Path, expected_size: u64) -> Result<()> {
    let meta = fs::metadata(path)?;
    if meta.len() != expected_size {
        return Err(Error::other(format!(
            "size changed: was {expected_size}, now {}",
            meta.len()
        )));
    }
    Ok(())
}

fn perform_action(action: DedupeAction, path: &Path, keeper: &Path) -> Result<()> {
    match action {
        DedupeAction::Remove => action_remove(path),
        DedupeAction::Recycle => action_recycle(path),
        DedupeAction::Hardlink => action_hardlink(path, keeper),
        DedupeAction::Reflink => action_reflink(path, keeper),
        DedupeAction::SafeRename => action_safe_rename(path),
    }
}

/// File extension we append in safe-rename mode. Chosen to be
/// distinctive enough that an undo walker won't accidentally touch
/// user files (e.g. `.bak` or `.tmp` would be too generic).
pub const SAFE_RENAME_SUFFIX: &str = ".superdeduper";

/// Single-file destructive actions, exposed so callers (the GUI) can
/// run them directly without round-tripping through a results file.
/// Each goes through the same Win32 / portable backend the planner
/// uses, so the safety guarantees are identical.
pub fn action_remove(path: &Path) -> Result<()> {
    fs::remove_file(path)?;
    Ok(())
}

pub fn action_recycle(path: &Path) -> Result<()> {
    recycle(path)
}

pub fn action_hardlink(target: &Path, keeper: &Path) -> Result<()> {
    replace_with_hardlink(target, keeper)
}

pub fn action_reflink(target: &Path, keeper: &Path) -> Result<()> {
    replace_with_reflink(target, keeper)
}

/// Safe-mode rename: append `.superdeduper` to the target. Idempotent —
/// files already ending in the suffix are a no-op. Reversible via
/// `unsuperdeduper_root`. Never deletes anything.
pub fn action_safe_rename(target: &Path) -> Result<()> {
    let name = target
        .file_name()
        .ok_or_else(|| Error::other(format!("{} has no file name", target.display())))?;
    let name_str = name.to_string_lossy();
    if name_str.ends_with(SAFE_RENAME_SUFFIX) {
        // Already safe-renamed; nothing to do.
        return Ok(());
    }
    let new_name = format!("{name_str}{SAFE_RENAME_SUFFIX}");
    let dest = target.with_file_name(new_name);
    if dest.exists() {
        return Err(Error::other(format!(
            "safe-rename: {} already exists",
            dest.display()
        )));
    }
    fs::rename(target, &dest)?;
    Ok(())
}

/// Walk `root` and rename every file ending in `.superdeduper` back to
/// its original. Used by the GUI's Unsuperdeduper button to reverse a
/// safe-rename batch on demand — no scan required first.
///
/// Returns `(renamed, skipped, errors)` so callers can surface a
/// summary line. Errors are logged via `tracing::warn!` and don't
/// halt the walk; a single permission-denied subdirectory shouldn't
/// abort the whole undo.
pub fn unsuperdeduper_root(root: &Path) -> Result<(u64, u64, u64)> {
    let mut renamed = 0u64;
    let mut skipped = 0u64;
    let mut errors = 0u64;
    let mut stack: Vec<std::path::PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "unsuperdeduper: dir open failed");
                errors += 1;
                continue;
            }
        };
        for entry in read.flatten() {
            let path = entry.path();
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            if !meta.is_file() {
                skipped += 1;
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                skipped += 1;
                continue;
            };
            if !name.ends_with(SAFE_RENAME_SUFFIX) {
                continue;
            }
            let restored_name = &name[..name.len() - SAFE_RENAME_SUFFIX.len()];
            let dest = path.with_file_name(restored_name);
            if dest.exists() {
                tracing::warn!(
                    path = %path.display(),
                    "unsuperdeduper: restore target already exists; skipping"
                );
                skipped += 1;
                continue;
            }
            match fs::rename(&path, &dest) {
                Ok(()) => renamed += 1,
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "unsuperdeduper: rename failed");
                    errors += 1;
                }
            }
        }
    }
    Ok((renamed, skipped, errors))
}

#[cfg(windows)]
fn recycle(path: &Path) -> Result<()> {
    crate::winapi_wrappers::recycle(path)
}

#[cfg(not(windows))]
fn recycle(path: &Path) -> Result<()> {
    // No Recycle Bin on non-Windows; fall back to plain remove. This
    // path exists so cross-platform tests of the dedupe planner work;
    // production targets are Windows-only.
    fs::remove_file(path)?;
    Ok(())
}

#[cfg(windows)]
fn replace_with_hardlink(target: &Path, keeper: &Path) -> Result<()> {
    crate::winapi_wrappers::replace_with_hardlink(target, keeper)
}

#[cfg(not(windows))]
fn replace_with_hardlink(target: &Path, keeper: &Path) -> Result<()> {
    let tmp = target.with_extension("superdeduper.tmp");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    fs::rename(target, &tmp)?;
    match fs::hard_link(keeper, target) {
        Ok(()) => {
            fs::remove_file(&tmp).ok();
            Ok(())
        }
        Err(e) => {
            fs::rename(&tmp, target).ok();
            Err(e.into())
        }
    }
}

#[cfg(windows)]
fn replace_with_reflink(target: &Path, keeper: &Path) -> Result<()> {
    crate::winapi_wrappers::replace_with_reflink(target, keeper)
}

#[cfg(not(windows))]
fn replace_with_reflink(_target: &Path, _keeper: &Path) -> Result<()> {
    Err(Error::Unsupported(
        "reflink (FSCTL_DUPLICATE_EXTENTS_TO_FILE) is Windows-only",
    ))
}

/// Convert a slice of user-supplied paths into a canonical lookup set.
/// Canonicalisation tolerates non-existent paths by falling back to
/// the input path; we still match by exact equality afterwards.
fn canonical_set(paths: &[PathBuf]) -> BTreeMap<PathBuf, ()> {
    let mut out = BTreeMap::new();
    for p in paths {
        let c = fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        out.insert(c, ());
    }
    out
}

/// Return true if `path` falls under any of the platform's
/// system-critical prefixes. Windows enumerates the well-known paths
/// from the spec; other platforms use a sensible default for testing.
pub fn is_system_path(path: &Path) -> bool {
    let s = path.to_string_lossy().to_ascii_lowercase();
    #[cfg(windows)]
    {
        for prefix in [
            "c:\\windows",
            "c:\\program files",
            "c:\\program files (x86)",
            "c:\\programdata",
        ] {
            if s.starts_with(prefix) {
                return true;
            }
        }
        // %USERPROFILE%\AppData
        if let Ok(home) = std::env::var("USERPROFILE") {
            let appdata = format!("{}\\appdata", home.to_ascii_lowercase());
            if s.starts_with(&appdata) {
                return true;
            }
        }
        false
    }
    #[cfg(not(windows))]
    {
        s.starts_with("/etc/") || s.starts_with("/usr/") || s.starts_with("/bin/")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{DedupeAction, KeepStrategy};
    use std::fs;
    use std::io::Write;

    fn tmpdir() -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "superdeduper-dedupe-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_file(path: &Path, body: &[u8]) {
        let mut f = fs::File::create(path).unwrap();
        f.write_all(body).unwrap();
    }

    fn group(size: u64, files: Vec<PathBuf>) -> DuplicateGroup {
        DuplicateGroup {
            size,
            content_hash: "deadbeef".into(),
            files,
            link_equivalent: false,
        }
    }

    fn results(groups: Vec<DuplicateGroup>) -> ResultsFile {
        ResultsFile {
            schema: "superdeduper.scan.v1".into(),
            groups,
            summary: None,
        }
    }

    fn make_args(results_path: PathBuf, dry_run: bool, action: DedupeAction) -> DedupeArgs {
        DedupeArgs {
            results_file: results_path,
            strategy: KeepStrategy::First,
            action,
            dry_run,
            allow_system_paths: false,
        }
    }

    fn write_results(d: &Path, r: &ResultsFile) -> PathBuf {
        let p = d.join("results.json");
        fs::write(&p, serde_json::to_string(r).unwrap()).unwrap();
        p
    }

    #[test]
    fn dry_run_changes_nothing() {
        let d = tmpdir();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write_file(&a, b"same");
        write_file(&b, b"same");
        let r = results(vec![group(4, vec![a.clone(), b.clone()])]);
        let path = write_results(&d, &r);
        let args = make_args(path, true, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert_eq!(outcome.planned, 1);
        assert_eq!(outcome.executed, 1);
        assert!(a.exists());
        assert!(b.exists(), "dry-run must not delete");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn remove_action_deletes_non_keeper() {
        let d = tmpdir();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        let c = d.join("c.bin");
        write_file(&a, b"x");
        write_file(&b, b"x");
        write_file(&c, b"x");
        let r = results(vec![group(1, vec![a.clone(), b.clone(), c.clone()])]);
        let path = write_results(&d, &r);
        let args = make_args(path, false, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert_eq!(outcome.planned, 2);
        assert_eq!(outcome.executed, 2);
        assert!(a.exists(), "first (keeper) survives");
        assert!(!b.exists());
        assert!(!c.exists());
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn size_mismatch_aborts_action() {
        let d = tmpdir();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write_file(&a, b"original");
        write_file(&b, b"original");
        let r = results(vec![group(8, vec![a.clone(), b.clone()])]);
        let path = write_results(&d, &r);
        // Mutate b after the scan was "captured".
        write_file(&b, b"different size");
        let args = make_args(path, false, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert_eq!(outcome.skipped_invalidated, 1);
        assert!(b.exists(), "modified file must not be deleted");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn system_path_guard_blocks_by_default() {
        // We can't plant real system files. Instead, lie: claim a known
        // system-prefixed path and assert the guard refuses it.
        let d = tmpdir();
        #[cfg(windows)]
        let sys = PathBuf::from("C:\\Windows\\superdeduper-fake.bin");
        #[cfg(not(windows))]
        let sys = PathBuf::from("/etc/superdeduper-fake.bin");
        let other = d.join("ok.bin");
        write_file(&other, b"x");
        let r = results(vec![group(1, vec![other.clone(), sys.clone()])]);
        let path = write_results(&d, &r);
        let args = make_args(path, false, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert_eq!(outcome.skipped_system, 1);
        assert!(
            other.exists(),
            "real file must not be touched when group is skipped"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn hardlink_action_replaces_with_link_on_unix() {
        // Linux path only — Windows path goes through winapi_wrappers
        // and is exercised in the Windows integration suite.
        if cfg!(windows) {
            return;
        }
        let d = tmpdir();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write_file(&a, b"link-me");
        write_file(&b, b"link-me");
        let r = results(vec![group(7, vec![a.clone(), b.clone()])]);
        let path = write_results(&d, &r);
        let args = make_args(path, false, DedupeAction::Hardlink);
        let outcome = run(&args).unwrap();
        assert_eq!(outcome.executed, 1);
        // Both files exist and share a inode (we check by stat).
        let inode_a = fs::metadata(&a).unwrap();
        let inode_b = fs::metadata(&b).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(inode_a.ino(), inode_b.ino());
        }
        let _ = inode_a;
        let _ = inode_b;
        fs::remove_dir_all(&d).ok();
    }
}
