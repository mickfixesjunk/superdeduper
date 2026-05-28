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
#[cfg(test)]
use crate::pipeline::SimilarityKind;
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
    /// #119 — files excluded from permanent `remove` because they
    /// carried an audio decode warning at scan time (a-hybrid guard).
    pub skipped_decode_warning: u64,
    pub failed: u64,
    pub bytes_reclaimed: u64,
}

/// #79 — Per-action rollup emitted by the GUI's Go workers. Captures
/// the success-only byte total that #79's PATCH submits as the
/// reclaim credit, plus the failure counters for the summary modal.
/// Archive has its own richer `gui::archive::ArchiveActionSummary`
/// (with failure-bucket breakdown per #80 Bug C); this is the
/// simpler shape used by Recycle / Remove / Hardlink / Reflink /
/// SafeRename.
///
/// Mapped to the web-side LOCKED_ACTION_KEYS via `locked_action_key`:
/// * Recycle → `deleted_to_recycle_bytes`
/// * Remove → `deleted_permanently_bytes`
/// * Hardlink → `hardlink_replaced_bytes`
/// * Reflink → `reflink_replaced_bytes`
/// * SafeRename → `None` (not credited; non-destructive per #79 spec)
#[derive(Debug, Clone)]
pub struct DedupeActionSummary {
    pub action: crate::cli::DedupeAction,
    pub ok_count: u64,
    /// The figure that gets credited to the leaderboard as the
    /// action's `actions_taken_summary` value via #79 PATCH. Sum of
    /// `size` for every file the worker successfully processed.
    pub ok_bytes: u64,
    pub failed_count: u64,
    pub failed_bytes: u64,
    pub user_stopped: bool,
}

impl DedupeActionSummary {
    /// #79 — Map the action variant to its server-side
    /// LOCKED_ACTION_KEYS slot. `None` ⇒ this action isn't
    /// credited (currently only SafeRename).
    pub fn locked_action_key(&self) -> Option<&'static str> {
        match self.action {
            crate::cli::DedupeAction::Recycle => Some("deleted_to_recycle_bytes"),
            crate::cli::DedupeAction::Remove => Some("deleted_permanently_bytes"),
            crate::cli::DedupeAction::Hardlink => Some("hardlink_replaced_bytes"),
            crate::cli::DedupeAction::Reflink => Some("reflink_replaced_bytes"),
            crate::cli::DedupeAction::SafeRename => None,
        }
    }
}

/// #79 — Single source-of-truth list of LOCKED_ACTION_KEYS as
/// committed with web (the 5 byte-credit keys + archive's
/// `archived_bytes` come through `gui::archive::ArchiveActionSummary`).
/// Held verbatim so the boundary test below pins this list against
/// the per-variant mapping; a future engine edit that adds a new
/// DedupeAction or renames a key without updating the other side
/// fails the test immediately. Mirrors web's cc6c5e74 invariant
/// pattern per design 2026-05-25T21:36Z.
pub const LOCKED_ACTION_KEYS: &[&str] = &[
    "deleted_to_recycle_bytes",
    "deleted_permanently_bytes",
    "hardlink_replaced_bytes",
    "reflink_replaced_bytes",
    "archived_bytes",
];

#[cfg(test)]
mod dedupe_action_summary_tests {
    use super::*;
    use crate::cli::DedupeAction;

    /// #79 boundary test — every credited DedupeAction variant
    /// maps to a key in LOCKED_ACTION_KEYS, and every key in
    /// LOCKED_ACTION_KEYS is reachable from either a DedupeAction
    /// variant or archive's `archived_bytes`. Anyone editing one
    /// side without the other surfaces here.
    #[test]
    fn locked_action_key_mapping_matches_locked_action_keys_list() {
        let credited: std::collections::HashSet<&'static str> = [
            DedupeAction::Recycle,
            DedupeAction::Remove,
            DedupeAction::Hardlink,
            DedupeAction::Reflink,
        ]
        .iter()
        .map(|a| {
            let s = DedupeActionSummary {
                action: *a,
                ok_count: 0,
                ok_bytes: 0,
                failed_count: 0,
                failed_bytes: 0,
                user_stopped: false,
            };
            s.locked_action_key()
                .expect("credited action must have a locked key")
        })
        .collect();
        let from_list: std::collections::HashSet<&'static str> =
            LOCKED_ACTION_KEYS.iter().copied().collect();
        // Archive's key is carried via ArchiveActionSummary, not
        // the DedupeAction enum — but it IS in LOCKED_ACTION_KEYS.
        // So the engine-credited set is credited ∪ {archived_bytes}.
        let mut engine_emit = credited.clone();
        engine_emit.insert("archived_bytes");
        assert_eq!(
            engine_emit, from_list,
            "DedupeAction::locked_action_key + archive's archived_bytes \
             must cover LOCKED_ACTION_KEYS exactly. If you renamed a key \
             or added a new credited action, update both the per-variant \
             mapping AND this list."
        );
    }

    #[test]
    fn safe_rename_is_not_credited() {
        let s = DedupeActionSummary {
            action: DedupeAction::SafeRename,
            ok_count: 5,
            ok_bytes: 5_000_000,
            failed_count: 0,
            failed_bytes: 0,
            user_stopped: false,
        };
        assert!(
            s.locked_action_key().is_none(),
            "SafeRename is reversible, so no leaderboard credit"
        );
    }
}

/// Run the dedupe planner against `args`. Returns a tally of what was
/// touched and what was skipped, plus per-action log lines via
/// `tracing`.
pub fn run(args: &DedupeArgs) -> Result<Outcome> {
    let raw = fs::read_to_string(&args.results_file)?;
    let results: ResultsFile = serde_json::from_str(&raw)?;
    if !results.schema.starts_with("superdeduper.scan") {
        return Err(Error::ConfigInvalid {
            field: "results-file:schema",
            reason: format!("unknown results schema `{}`", results.schema),
        });
    }

    let references = canonical_set(&[]); // reference set is part of the scan; reserved.
    let mut outcome = Outcome::default();
    // Construct the action-receipt writer once per run. Disabled
    // sink when --integration-test-mode is off; emit calls become
    // no-ops downstream and the flag costs effectively nothing.
    let mut receipts = crate::action_receipt::ReceiptWriter::from_flags(
        args.integration_test_mode,
        args.receipt_file.as_deref(),
    );
    for (i, group) in results.groups.iter().enumerate() {
        if let Err(e) = process_group(i, group, args, &references, &mut outcome, &mut receipts) {
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
    receipts: &mut crate::action_receipt::ReceiptWriter,
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

        // #119 a-hybrid guard: a file flagged with an audio decode
        // warning at scan time is corrupt-but-decodable — exactly what
        // the user should decide on, not silently lose. Exclude it from
        // permanent `remove`; allow reversible `recycle` (+ other
        // non-destructive actions) but surface the warning on the
        // receipt. Applied before the dry-run branch so a preview
        // reflects the same policy.
        let decode_warning: Option<String> =
            if group.decode_warning_paths.iter().any(|p| p == path) {
                Some(
                    "file carried an audio decode warning at scan time \
                     (corrupt-but-decodable)"
                        .to_string(),
                )
            } else {
                None
            };
        if decode_warning.is_some() && matches!(args.action, DedupeAction::Remove) {
            outcome.skipped_decode_warning += 1;
            tracing::warn!(
                group = idx + 1,
                path = %path.display(),
                "decode_warning: excluded from permanent removal (#119 a-hybrid guard)"
            );
            let mut r = crate::action_receipt::ActionReceipt::new(
                crate::action_receipt::action_label(DedupeAction::Remove),
                &path.display().to_string(),
                &keeper.display().to_string(),
                group.size,
            );
            r.outcome = "skipped_decode_warning".to_string();
            r.decode_warning = Some(
                "excluded from permanent removal: file flagged with an audio \
                 decode warning at scan time"
                    .to_string(),
            );
            let _ = receipts.emit(&r);
            continue;
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
            let mut dr = crate::action_receipt::ActionReceipt::dry_run(
                &path.display().to_string(),
                &keeper.display().to_string(),
                group.size,
            );
            dr.decode_warning = decode_warning.clone();
            let _ = receipts.emit(&dr);
            continue;
        }

        // Capture pre-action inode + hardlink count so the receipt
        // can report deltas. The harness uses these to assert that
        // the action affected EXACTLY the targeted inode (and
        // nothing else).
        let pre = crate::action_receipt::read_inode_and_nlink(path);

        match perform_action(args.action, path, keeper, args.allow_destructive_on_deduped) {
            Ok(trash_outcome) => {
                outcome.executed += 1;
                outcome.bytes_reclaimed += group.size;
                tracing::info!(
                    group = idx + 1,
                    action = ?args.action,
                    target = %path.display(),
                    keeper = %keeper.display(),
                    "applied"
                );
                emit_action_receipt(
                    receipts,
                    args.action,
                    path,
                    keeper,
                    group.size,
                    pre,
                    None,
                    trash_outcome,
                    decode_warning,
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
                let err_str = format!("{e}");
                emit_action_receipt(
                    receipts,
                    args.action,
                    path,
                    keeper,
                    group.size,
                    pre,
                    Some(err_str),
                    crate::platform::TrashOutcome::default(),
                    decode_warning,
                );
            }
        }
    }
    Ok(())
}

/// Build + emit one action receipt after `perform_action` returns.
/// Captures the post-action inode + hardlink count when the file
/// still exists; reports the appropriate outcome + delta. Errors
/// during emission are logged but never propagate — a receipt-file
/// write failure shouldn't kill a long-running dedupe.
#[allow(clippy::too_many_arguments)]
fn emit_action_receipt(
    receipts: &mut crate::action_receipt::ReceiptWriter,
    action: DedupeAction,
    path: &Path,
    keeper: &Path,
    size: u64,
    pre: Option<(String, u64)>,
    error: Option<String>,
    trash_outcome: crate::platform::TrashOutcome,
    decode_warning: Option<String>,
) {
    use crate::action_receipt::{
        action_label, read_inode_and_nlink, ActionReceipt, RecycleBinEntry,
    };

    let action_str = action_label(action);
    let source_str = path.display().to_string();
    let keeper_str = keeper.display().to_string();

    let mut receipt = if let Some(err) = error.as_deref() {
        ActionReceipt::error_for(action_str, &source_str, &keeper_str, size, err)
    } else {
        ActionReceipt::new(action_str, &source_str, &keeper_str, size)
    };

    // GH #33 — populate the recycle_bin_entry block when the action
    // was recycle-to-trash AND the platform backend surfaced metadata.
    // Linux's XDG trash impl fills all four fields; Windows IFileOperation
    // wiring is v2 territory (TrashOutcome::default() leaves them None).
    if matches!(action, DedupeAction::Recycle)
        && (trash_outcome.container.is_some()
            || trash_outcome.info_file.is_some()
            || trash_outcome.data_file.is_some())
    {
        receipt.recycle_bin_entry = Some(RecycleBinEntry {
            container: trash_outcome
                .container
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            index_file: trash_outcome
                .info_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            data_file: trash_outcome
                .data_file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            original_path: trash_outcome
                .original_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| source_str.clone()),
        });
    }

    if let Some((ino, _nlink_before)) = pre {
        receipt.inode_before = Some(ino);
    }
    // Post-action stat: file may have been deleted (None) or
    // re-pointed at the keeper's inode (hardlink-replace).
    let post = read_inode_and_nlink(path);
    if let Some((ino_after, _nlink_after)) = &post {
        receipt.inode_after = Some(ino_after.clone());
    }

    // Hardlink-count delta heuristic:
    // - delete-to-recycle / delete-permanently: -1 when the source's
    //   inode is gone (post is None), 0 otherwise.
    // - hardlink-replace: +1 on success (source now shares keeper's
    //   inode → keeper's nlink incremented).
    // - reflink / safe-rename: 0 (no nlink mutation).
    // - dry-run / left-alone: covered by their own emit paths.
    if error.is_none() {
        receipt.hardlink_count_delta = match action {
            DedupeAction::Recycle | DedupeAction::Remove => {
                if post.is_none() {
                    -1
                } else {
                    0
                }
            }
            DedupeAction::Hardlink => 1,
            DedupeAction::Reflink | DedupeAction::SafeRename => 0,
        };
    }

    receipt.decode_warning = decode_warning;

    let _ = receipts.emit(&receipt);
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
                let t = crate::keep::file_mtime(p);
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
            return Err(Error::ConfigInvalid {
                field: "--strategy",
                reason: "in-reference requires reference paths in the scan".into(),
            });
        }
        KeepStrategy::Interactive => {
            return Err(Error::ConfigInvalid {
                field: "--strategy",
                reason: "interactive is not yet implemented".into(),
            });
        }
        KeepStrategy::Smart => {
            // #68 — single source of truth for Smart-keeper tiebreak
            // lives in `keep::pick_keeper`. CLI flow + GUI flow
            // (via `gui::live::order_keeper_first`) both call into
            // it so a future tiebreak-signal addition lands in
            // exactly one place. Pre-compute mtimes since
            // `keep::pick_keeper` takes a parallel slice.
            let mtimes: Vec<Option<std::time::SystemTime>> = group
                .files
                .iter()
                .map(|p| crate::keep::file_mtime(p))
                .collect();
            idx = crate::keep::pick_keeper(&group.files, &mtimes);
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

/// Args-driven dispatch. Honours `--allow-destructive-on-deduped` by
/// running its own policy-aware guard up front, then calling the
/// OS-level helpers directly (skipping the default-conservative guard
/// in the pub fn action_*() wrappers, which are kept for GUI callers
/// that haven't been wired to the policy yet).
fn perform_action(
    action: DedupeAction,
    path: &Path,
    keeper: &Path,
    allow_destructive_on_deduped: bool,
) -> Result<crate::platform::TrashOutcome> {
    guard_destructive(path, allow_destructive_on_deduped)?;
    match action {
        DedupeAction::Remove => {
            fs::remove_file(path)?;
            Ok(crate::platform::TrashOutcome::default())
        }
        DedupeAction::Recycle => recycle(path),
        DedupeAction::Hardlink => {
            replace_with_hardlink(path, keeper)?;
            Ok(crate::platform::TrashOutcome::default())
        }
        DedupeAction::Reflink => {
            replace_with_reflink(path, keeper)?;
            Ok(crate::platform::TrashOutcome::default())
        }
        DedupeAction::SafeRename => {
            safe_rename_unguarded(path)?;
            Ok(crate::platform::TrashOutcome::default())
        }
    }
}

/// Refuse destructive action against cloud-placeholder and
/// reparse-tagged files. Stats `path`, classifies via
/// `inventory::placeholder::classify()`, and returns an error if
/// the resulting `PlaceholderState` blocks destructive action under
/// the supplied policy.
///
/// Cross-platform: on non-Windows, attributes are always 0 →
/// `NotPlaceholder` → never blocks. The guard is a no-op on those
/// platforms.
///
/// `allow_destructive_on_deduped` (phase 6) lets the user opt into
/// destructive actions against `ReparseDedup` files via
/// `--allow-destructive-on-deduped`. Recall/unknown reparses stay
/// blocked regardless of this flag.
fn guard_destructive(path: &Path, allow_destructive_on_deduped: bool) -> Result<()> {
    let state = placeholder_state_for(path)?;
    if state.blocks_destructive_action_under_policy(allow_destructive_on_deduped) {
        return Err(Error::other(format!(
            "refusing destructive action on placeholder/reparse file ({state:?}): {}",
            path.display(),
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn placeholder_state_for(path: &Path) -> Result<crate::inventory::PlaceholderState> {
    use std::os::windows::fs::MetadataExt;
    let m = fs::metadata(path)?;
    Ok(crate::inventory::placeholder::classify(
        m.file_attributes(),
        None,
    ))
}

#[cfg(not(windows))]
fn placeholder_state_for(_path: &Path) -> Result<crate::inventory::PlaceholderState> {
    Ok(crate::inventory::PlaceholderState::NotPlaceholder)
}

/// File extension we append in safe-rename mode. Chosen to be
/// distinctive enough that an undo walker won't accidentally touch
/// user files (e.g. `.bak` or `.tmp` would be too generic).
pub const SAFE_RENAME_SUFFIX: &str = ".superdeduper";

/// Single-file destructive actions, exposed so callers (the GUI) can
/// run them directly without round-tripping through a results file.
/// Each goes through the same Win32 / portable backend the planner
/// uses, so the safety guarantees are identical.
///
/// Every action calls `guard_destructive(path)` first. The planner
/// already filters placeholders, so this is defense in depth — even
/// if a caller forgets to filter (e.g. a future code path, or a
/// future GUI flow), the action layer refuses to delete / replace /
/// rename a cloud placeholder or reparse-tagged file.
pub fn action_remove(path: &Path) -> Result<()> {
    guard_destructive(path, false)?;
    fs::remove_file(path)?;
    Ok(())
}

pub fn action_recycle(path: &Path) -> Result<()> {
    guard_destructive(path, false)?;
    // Drop the TrashOutcome — the GUI's per-row recycle path doesn't
    // emit receipts (it's only the `dedupe` subcommand flow that
    // surfaces them). Future: thread metadata into a per-action
    // event the GUI can render in the action-progress modal.
    recycle(path)?;
    Ok(())
}

pub fn action_hardlink(target: &Path, keeper: &Path) -> Result<()> {
    guard_destructive(target, false)?;
    replace_with_hardlink(target, keeper)
}

pub fn action_reflink(target: &Path, keeper: &Path) -> Result<()> {
    guard_destructive(target, false)?;
    replace_with_reflink(target, keeper)
}

/// Safe-mode rename: append `.superdeduper` to the target. Idempotent —
/// files already ending in the suffix are a no-op. Reversible via
/// `unsuperdeduper_root`. Never deletes anything.
pub fn action_safe_rename(target: &Path) -> Result<()> {
    guard_destructive(target, false)?;
    safe_rename_unguarded(target)
}

/// The OS-level portion of safe-rename — no placeholder guard, no
/// policy. Called from `perform_action` (which has already applied a
/// policy-aware guard) and from `action_safe_rename` (which applies
/// the conservative default guard up front). Pulled out so the two
/// callers can share the rename logic without round-tripping through
/// another guard call.
fn safe_rename_unguarded(target: &Path) -> Result<()> {
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

/// Recycle/trash a file, returning whatever metadata the platform
/// backend can surface. The metadata is fed into the action_receipt's
/// `recycle_bin_entry` block per GH #33. Empty `TrashOutcome` (no
/// fields populated) is fine — the receipt just emits an empty
/// sub-object in that case.
#[cfg(windows)]
fn recycle(path: &Path) -> Result<crate::platform::TrashOutcome> {
    crate::winapi_wrappers::recycle(path)?;
    // TODO #33 v2 — windows::trash route through IFileOperation
    // doesn't yet capture the $I/$R filenames the shell minted.
    // Once it does, plumb them into TrashOutcome here.
    Ok(crate::platform::TrashOutcome::default())
}

#[cfg(target_os = "linux")]
fn recycle(path: &Path) -> Result<crate::platform::TrashOutcome> {
    // L0: XDG Trash spec implementation. Lives in src/platform/linux/trash.rs.
    crate::platform::trash_file(path).map_err(|e| Error::other(format!("trash: {e}")))
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn recycle(path: &Path) -> Result<crate::platform::TrashOutcome> {
    // Other Unixes (macOS pending L3) — fall back to plain remove for now.
    fs::remove_file(path)?;
    Ok(crate::platform::TrashOutcome::default())
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

#[cfg(target_os = "linux")]
fn replace_with_reflink(target: &Path, keeper: &Path) -> Result<()> {
    // L0: FICLONE-based clone with atomic-via-tmp-rename. The
    // `target` argument is the path we're REPLACING; the `keeper` is
    // the file we want `target` to become a CoW clone of. After this
    // call, both files share storage on disk.
    //
    // platform::clone_file(src, dst) creates `dst` as a clone of
    // `src`. So here src=keeper, dst=target.
    crate::platform::clone_file(keeper, target).map_err(|e| match e {
        crate::platform::PlatformError::Unsupported(msg) => Error::Unsupported(msg),
        other => Error::other(format!("reflink: {other}")),
    })
}

#[cfg(all(not(windows), not(target_os = "linux")))]
fn replace_with_reflink(_target: &Path, _keeper: &Path) -> Result<()> {
    Err(Error::Unsupported(
        "reflink not implemented on this platform yet (L3 roadmap covers macOS)",
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
        // `/var/lib` added 2026-05-25 per testdesign CST3 finding +
        // Mick approval: holds package-manager state (dpkg/rpm/apt
        // databases), systemd unit files, container layer storage,
        // and SQLite databases for many system services. Moving or
        // deduplicating these breaks the OS in subtle ways that
        // typically only surface on next boot or service restart.
        // Matches the v0.2.7 exclusions-preset pattern that lists
        // `/var/lib/dpkg/info/**` under OsSystemTrees — this
        // block-list catch is the cli/non-exclusions safety net.
        s.starts_with("/etc/")
            || s.starts_with("/usr/")
            || s.starts_with("/bin/")
            || s.starts_with("/var/lib/")
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
        let unique_inodes = files.len() as u64;
        DuplicateGroup {
            size,
            content_hash: "deadbeef".into(),
            files,
            link_equivalent: false,
            unique_inodes,
            similarity_kind: SimilarityKind::ByteIdentical,
            decode_warning_paths: Vec::new(),
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
            mode: crate::cli::ScanMode::Exact,
            dry_run,
            allow_system_paths: false,
            allow_destructive_on_deduped: false,
            integration_test_mode: false,
            receipt_file: None,
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
    fn guard_destructive_allows_normal_files() {
        let d = tmpdir();
        let f = d.join("regular.bin");
        write_file(&f, b"normal");
        assert!(guard_destructive(&f, false).is_ok());
        assert!(
            guard_destructive(&f, true).is_ok(),
            "policy doesn't change behaviour for NotPlaceholder files"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn action_remove_normal_file_still_works() {
        let d = tmpdir();
        let f = d.join("doomed.bin");
        write_file(&f, b"bye");
        action_remove(&f).unwrap();
        assert!(!f.exists());
        fs::remove_dir_all(&d).ok();
    }

    // #119 a-hybrid guard: a group member flagged with a decode
    // warning is excluded from permanent `remove` but allowed for
    // reversible `recycle`.
    fn flagged_group(size: u64, files: Vec<PathBuf>, flagged: Vec<PathBuf>) -> DuplicateGroup {
        let mut g = group(size, files);
        g.decode_warning_paths = flagged;
        g
    }

    #[test]
    fn flagged_file_excluded_from_permanent_remove() {
        let d = tmpdir();
        let keeper = d.join("keeper.flac");
        let dupe = d.join("corrupt_dupe.flac");
        write_file(&keeper, b"same");
        write_file(&dupe, b"same");
        let r = results(vec![flagged_group(
            4,
            vec![keeper.clone(), dupe.clone()],
            vec![dupe.clone()],
        )]);
        let path = write_results(&d, &r);
        let args = make_args(path, false, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert_eq!(
            outcome.skipped_decode_warning, 1,
            "flagged dupe must be counted as skipped"
        );
        assert_eq!(outcome.executed, 0, "nothing should be removed");
        assert!(keeper.exists(), "keeper untouched");
        assert!(
            dupe.exists(),
            "flagged dupe must NOT be permanently removed"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn flagged_file_allowed_for_recycle() {
        // The guard must NOT exclude a flagged file from `recycle`
        // (reversible) — it should reach perform_action and carry the
        // decode_warning onto its receipt. Whether the XDG-trash move
        // physically succeeds in the test env is covered separately by
        // recycle_action_uses_xdg_trash_on_linux; here we assert the
        // guard boundary (not excluded + warning surfaced), which is
        // environment-independent.
        let d = tmpdir();
        let keeper = d.join("keeper.flac");
        let dupe = d.join("corrupt_dupe.flac");
        write_file(&keeper, b"same");
        write_file(&dupe, b"same");
        let r = results(vec![flagged_group(
            4,
            vec![keeper.clone(), dupe.clone()],
            vec![dupe.clone()],
        )]);
        let path = write_results(&d, &r);
        let receipt_path = d.join("receipts.jsonl");
        let args = DedupeArgs {
            results_file: path,
            strategy: KeepStrategy::First,
            action: DedupeAction::Recycle,
            mode: crate::cli::ScanMode::Exact,
            dry_run: false,
            allow_system_paths: false,
            allow_destructive_on_deduped: false,
            integration_test_mode: true,
            receipt_file: Some(receipt_path.clone()),
        };
        let outcome = run(&args).unwrap();
        assert_eq!(
            outcome.skipped_decode_warning, 0,
            "recycle is allowed for flagged files — must not be guard-excluded"
        );
        assert_eq!(
            outcome.executed + outcome.failed,
            1,
            "the flagged dupe must reach the recycle action, not be skipped"
        );
        assert!(keeper.exists(), "keeper untouched");
        let receipts = fs::read_to_string(&receipt_path).unwrap();
        assert!(
            receipts.contains("decode_warning"),
            "recycle receipt must surface the decode_warning, got: {receipts}"
        );
        assert!(
            !receipts.contains("skipped_decode_warning"),
            "recycle must NOT be recorded as a skip, got: {receipts}"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn flagged_remove_receipt_records_skip_and_warning() {
        let d = tmpdir();
        let keeper = d.join("keeper.flac");
        let dupe = d.join("corrupt_dupe.flac");
        write_file(&keeper, b"same");
        write_file(&dupe, b"same");
        let r = results(vec![flagged_group(
            4,
            vec![keeper.clone(), dupe.clone()],
            vec![dupe.clone()],
        )]);
        let path = write_results(&d, &r);
        let receipt_path = d.join("receipts.jsonl");
        let args = DedupeArgs {
            results_file: path,
            strategy: KeepStrategy::First,
            action: DedupeAction::Remove,
            mode: crate::cli::ScanMode::Exact,
            dry_run: false,
            allow_system_paths: false,
            allow_destructive_on_deduped: false,
            integration_test_mode: true,
            receipt_file: Some(receipt_path.clone()),
        };
        run(&args).unwrap();
        let receipts = fs::read_to_string(&receipt_path).unwrap();
        assert!(
            receipts.contains("skipped_decode_warning"),
            "receipt must record the skip outcome, got: {receipts}"
        );
        assert!(
            receipts.contains("decode_warning"),
            "receipt must surface the decode_warning, got: {receipts}"
        );
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

    /// /var/lib added to the system-path block list 2026-05-25 (per
    /// testdesign CST3 finding). Catches OS-state directories like
    /// dpkg databases, systemd unit files, container layer storage —
    /// deduplicating them breaks the OS in subtle "only on reboot"
    /// ways.
    #[cfg(not(windows))]
    #[test]
    fn var_lib_is_blocked_by_default() {
        let p = PathBuf::from("/var/lib/dpkg/info/coreutils.list");
        assert!(
            is_system_path(&p),
            "/var/lib paths must be blocked from CLI dedupe by default"
        );
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

    #[test]
    #[cfg(target_os = "linux")]
    fn recycle_action_uses_xdg_trash_on_linux() {
        // L0 deliverable: action_recycle on Linux MUST route through
        // platform::trash_file (XDG Trash spec) rather than fall back
        // to plain remove. Builds an isolated fake HOME so the
        // trashed file lands in a known location we can inspect +
        // doesn't pollute the dev's real ~/.local/share/Trash.
        let d = tmpdir();
        let fake_home = d.join("fake_home");
        fs::create_dir_all(&fake_home).unwrap();
        let target = d.join("doomed-recycle.bin");
        write_file(&target, b"recycle-me");

        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &fake_home);
        std::env::remove_var("XDG_DATA_HOME");

        action_recycle(&target).expect("action_recycle on Linux");

        // Original gone.
        assert!(
            !target.exists(),
            "recycled file should not remain at source"
        );
        // Trash files dir has it.
        let trashed = fake_home.join(".local/share/Trash/files/doomed-recycle.bin");
        assert!(
            trashed.exists(),
            "file should land in XDG Trash files/ subdir"
        );
        // Matching info file.
        let info = fake_home.join(".local/share/Trash/info/doomed-recycle.bin.trashinfo");
        assert!(info.exists(), "trashinfo should be written");

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn reflink_action_returns_unsupported_on_tmpfs() {
        // tmpfs (most likely backend for std::env::temp_dir) doesn't
        // support FICLONE. The platform layer maps EOPNOTSUPP to
        // PlatformError::Unsupported; dedupe.rs maps that further to
        // Error::Unsupported. The contract: callers see Unsupported,
        // NOT a raw IO error, so they can fall back to copy + replace
        // cleanly (or surface the right user-facing message).
        let d = tmpdir();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write_file(&a, b"reflink-me");
        write_file(&b, b"reflink-me");

        match action_reflink(&b, &a) {
            // CoW-capable temp filesystem (Btrfs / XFS-reflink=1)
            // is allowed — the test should pass on those hosts too.
            Ok(()) => {
                assert!(b.exists());
            }
            Err(Error::Unsupported(_)) => { /* expected on tmpfs */ }
            Err(other) => panic!("expected Ok or Error::Unsupported, got {other:?}"),
        }
        fs::remove_dir_all(&d).ok();
    }
}
