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
    /// F-CLI-7 — group-member files that fell under a `scan --reference`
    /// root, resolved + persisted at scan time so the separated
    /// scan→dedupe-file flow can honor `--strategy in-reference` (and
    /// never-modify-references) without a `dedupe --reference` flag.
    /// Stored as they appear in `groups[].files`; the reference check
    /// canonicalizes these and the group members alike before matching.
    /// Empty when the scan had no reference roots. `#[serde(default)]`
    /// keeps older results JSON readable.
    #[serde(default)]
    pub reference_paths: Vec<PathBuf>,
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
    /// §7.1 — files refused because they're cloud placeholders / reparse
    /// points that block destructive action under the active policy.
    pub skipped_placeholder: u64,
    /// Keeper-preservation gate 2 — non-keeper members refused because they
    /// resolve by file-IDENTITY to the keeper (the keeper reached via an
    /// alias). Destructive action on them would delete the keeper.
    pub skipped_keeper_identity: u64,
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

    // F-CLI-7 — reference set comes from the scan-persisted reference_paths
    // (group-member files under a `scan --reference` root). Was always
    // empty before, so `--strategy in-reference` + never-modify-reference
    // couldn't work via the scan→dedupe-file two-step.
    let references = canonical_set(&results.reference_paths);
    // F-CLI-7 — `--strategy in-reference` against a results file whose
    // scan had NO `--reference` roots can't anchor on anything; surface
    // it as one clean config error up front rather than failing every
    // group in the loop below.
    if matches!(args.strategy, KeepStrategy::InReference) && references.is_empty() {
        return Err(Error::ConfigInvalid {
            field: "--strategy",
            reason: "in-reference needs reference roots, but this results file's scan \
                     had none — re-run `scan --reference <root>` to mark reference copies"
                .into(),
        });
    }
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

    // System-path guard (F-CLI-4). Refuse the whole group if any member
    // is under a system-critical path. §7.1 — emit a per-file
    // refused_system_path receipt for each refused member so the
    // containment matrix can assert WHICH file was refused, not just read
    // the coarse skipped_system counter (the empty-snapshot-delta cell
    // would otherwise have no per-action signal).
    if !args.allow_system_paths {
        // canonical-consistent with the GUI guard (resolves_to_system_path):
        // both check the raw path AND its canonical resolution, so an alias to
        // a system dir is refused on BOTH surfaces (v0.2.36 finding #1 fix).
        let sys: Vec<&PathBuf> = group
            .files
            .iter()
            .filter(|p| resolves_to_system_path(p))
            .collect();
        if !sys.is_empty() {
            outcome.skipped_system += 1;
            for p in &sys {
                tracing::warn!(path = %p.display(), "system path; group skipped");
                let mut r = crate::action_receipt::ActionReceipt::new(
                    crate::action_receipt::action_label(args.action),
                    &p.display().to_string(),
                    &p.display().to_string(),
                    group.size,
                );
                r.outcome = "refused_system_path".to_string();
                let _ = receipts.emit(&r);
            }
            return Ok(());
        }
    }

    // F-CLI-7 — in-reference only acts on groups that contain a reference
    // member; a group with none isn't a config error (run() already handled
    // the no-refs-at-all case) — skip it cleanly rather than fail.
    if matches!(args.strategy, KeepStrategy::InReference)
        && !group
            .files
            .iter()
            .any(|f| references.contains_key(&canonical_key(f)))
    {
        return Ok(());
    }

    let keeper_idx = pick_keeper(group, args.strategy, references)?;
    let keeper = &group.files[keeper_idx];
    // Keeper-preservation gate 2 (Mick safety directive): resolve the
    // keeper's PHYSICAL-FILE identity once per group. Any non-keeper member
    // that resolves to this same identity is the keeper reached via an alias
    // (\\?\ verbatim / 8.3 / junction / symlink / case) — destructive action
    // on it would delete the keeper's data. file_identity defeats the whole
    // alias class because it opens a handle + reads the kernel file-id.
    let keeper_identity = file_identity(keeper);

    for (i, path) in group.files.iter().enumerate() {
        if i == keeper_idx {
            // §7.1 — the keeper is consciously preserved; emit an explicit
            // left_alone receipt (one per keeper-per-group) so the
            // containment matrix asserts conscious-preserve, not
            // silent-miss. No data moves: delta 0, no inode change.
            let size = group.file_sizes.get(i).copied().unwrap_or(group.size);
            let mut r = crate::action_receipt::ActionReceipt::new(
                crate::action_receipt::action_label(args.action),
                &keeper.display().to_string(),
                &keeper.display().to_string(),
                size,
            );
            r.outcome = "left_alone".to_string();
            let _ = receipts.emit(&r);
            continue;
        }
        outcome.planned += 1;

        // F-SAFE-2 (data-loss) — the "never modify a reference" guard must
        // canonicalize the member, same as the two sibling reference checks
        // (in-reference skip + pick_keeper). `references` is canonical-keyed,
        // so a RAW contains_key here MISSED any reference member whose path
        // representation differs from its canonical form (\\?\ verbatim,
        // symlink root, 8.3, case-fold) — that reference copy would then fall
        // through to validate_file + perform_action and be DELETED. Same
        // canonical_key asymmetry class as the P0 and F-CLI-7.
        if references.contains_key(&canonical_key(path)) {
            outcome.skipped_reference += 1;
            tracing::info!(
                group = idx + 1,
                path = %path.display(),
                "reference path; never modified"
            );
            continue;
        }

        // Re-verify the file hasn't changed since the scan. #147 — use
        // THIS member's recorded scan-time size, not the group
        // representative (`group.size` is the largest member for
        // perceptual groups, so size-varying members would be wrongly
        // rejected). Falls back to `group.size` when per-file sizes
        // weren't recorded (older results JSON / byte-identical groups,
        // where every member equals `group.size` anyway).
        let expected_size = group.file_sizes.get(i).copied().unwrap_or(group.size);
        match validate_file(path, expected_size) {
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

        // §7.1 — refuse destructive action on cloud-placeholder / reparse
        // files with a DISTINCT outcome. Previously this was caught only
        // inside perform_action's guard_destructive and collapsed into the
        // generic `error` outcome (message-substring only) — fragile, since
        // a regression that stopped refusing would still read as a generic
        // failure. perform_action keeps the same guard as defense-in-depth.
        // On non-Windows this is always NotPlaceholder, so it never fires.
        let pstate = placeholder_state_for(path)?;
        if pstate.blocks_destructive_action_under_policy(args.allow_destructive_on_deduped) {
            outcome.skipped_placeholder += 1;
            tracing::warn!(
                group = idx + 1,
                path = %path.display(),
                state = ?pstate,
                "placeholder/reparse; destructive action refused"
            );
            let mut r = crate::action_receipt::ActionReceipt::new(
                crate::action_receipt::action_label(args.action),
                &path.display().to_string(),
                &keeper.display().to_string(),
                group.size,
            );
            r.outcome = "refused_placeholder".to_string();
            r.error = Some(format!(
                "refused destructive action on placeholder/reparse file ({pstate:?})"
            ));
            let _ = receipts.emit(&r);
            continue;
        }

        // Keeper-preservation gate 2 (Mick safety directive) — file-IDENTITY.
        // If this member resolves to the SAME physical file as the keeper, it
        // IS the keeper reached via an alias (\\?\ / 8.3 / junction / symlink
        // / case). Opening a handle to read the kernel file-id collapses every
        // alias form to one comparison. Runs BEFORE the dry-run branch so a
        // preview reflects the refusal, and subsumes the old inode-string
        // already_hardlinked check.
        let member_identity = file_identity(path);
        let positive_keeper_match =
            matches!((keeper_identity, member_identity), (Some(k), Some(m)) if k == m);
        if positive_keeper_match {
            let mut r = crate::action_receipt::ActionReceipt::new(
                crate::action_receipt::action_label(args.action),
                &path.display().to_string(),
                &keeper.display().to_string(),
                group.size,
            );
            if matches!(args.action, DedupeAction::Hardlink) {
                // The dedup intent (loser shares the keeper's inode) is
                // already satisfied — no-op, not a refusal. 0 bytes freed.
                r.outcome = "already_hardlinked".to_string();
                let _ = receipts.emit(&r);
                outcome.executed += 1;
            } else {
                // Destructive action on the keeper-via-alias: fail-closed.
                r.outcome = "refused_keeper_identity".to_string();
                r.error = Some(
                    "refused: member resolves by file-id to the keeper \
                     (same physical file via an alias) — destructive action \
                     would delete the keeper"
                        .to_string(),
                );
                outcome.skipped_keeper_identity += 1;
                tracing::warn!(
                    group = idx + 1,
                    path = %path.display(),
                    keeper = %keeper.display(),
                    "keeper-identity: member resolves to the keeper; destructive action refused"
                );
                let _ = receipts.emit(&r);
            }
            continue;
        }

        // Quality Finding 1/2 — fail-CLOSED on identity-resolution
        // UNCERTAINTY for IRREVERSIBLE actions. If we can't positively
        // resolve BOTH the keeper's and this member's file-id (locked,
        // sharing-violation, access-denied), we cannot PROVE the member
        // isn't the keeper-via-alias — and for permanent `remove` there is no
        // gate-4 that can PREVENT the loss (post-condition verify runs after
        // the delete, only DETECTS it). The directive is default-trip =
        // preserve, so refuse rather than delete on a guess. A None keeper
        // would otherwise disable the gate for the WHOLE group (the most acute
        // fail-open). Tolerance for transient unopenability is kept only for
        // REVERSIBLE actions (recycle = trash, safe-rename = undoable) +
        // hardlink/reflink (replace-with-link, guarded by the move-aside
        // restore), where a wrong call is recoverable.
        if matches!(args.action, DedupeAction::Remove)
            && (keeper_identity.is_none() || member_identity.is_none())
        {
            outcome.skipped_keeper_identity += 1;
            tracing::warn!(
                group = idx + 1,
                path = %path.display(),
                keeper = %keeper.display(),
                keeper_resolved = keeper_identity.is_some(),
                member_resolved = member_identity.is_some(),
                "keeper-identity: file-id unresolvable under permanent remove; refused (fail-closed)"
            );
            let mut r = crate::action_receipt::ActionReceipt::new(
                crate::action_receipt::action_label(args.action),
                &path.display().to_string(),
                &keeper.display().to_string(),
                group.size,
            );
            r.outcome = "refused_keeper_unresolvable".to_string();
            r.error = Some(
                "refused: could not resolve the file-id of the keeper and/or this \
                 member, so it cannot be proven distinct from the keeper — \
                 permanent remove is irreversible, so the group is preserved \
                 (fail-closed). Retry when the file is not locked."
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

        // (already_hardlinked is now handled by the file-identity gate
        // above — a member sharing the keeper's physical identity is the
        // satisfied no-op, detected robustly via file-id rather than the
        // old inode-string sentinel.)

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
                    None,
                    trash_outcome,
                    decode_warning,
                );
            }
            Err(e) => {
                outcome.failed += 1;
                // §7.1 — a cross-volume hardlink/reflink failure is a
                // distinct REFUSAL outcome, not a generic error. The
                // underlying op now propagates the OS error (Windows
                // ERROR_NOT_SAME_DEVICE / Unix EXDEV) — see the P0 fix in
                // winapi_wrappers::create_hard_link — and the original file
                // is preserved by perform_action's restore path.
                let cross_volume = matches!(
                    args.action,
                    DedupeAction::Hardlink | DedupeAction::Reflink
                ) && is_cross_device(&e);
                let outcome_override = cross_volume.then_some("refused_cross_volume");
                tracing::error!(
                    group = idx + 1,
                    path = %path.display(),
                    error = %e,
                    cross_volume,
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
                    outcome_override,
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
    // §7.1 — when set, replaces the default ok/error outcome with a
    // distinct named outcome (e.g. refused_cross_volume) while keeping any
    // human detail in `error`. The outcome string is the matrix assertion.
    outcome_override: Option<&str>,
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
    if let Some(o) = outcome_override {
        receipt.outcome = o.to_string();
    }

    // GH #33 — populate the recycle_bin_entry block on every
    // successful recycle action, regardless of platform. Linux's
    // XDG trash impl fills all four fields; Windows currently
    // populates only `original_path` (container/$I/$R filename
    // capture from IFileOperationProgressSink is v2 work). The empty-
    // string fallback for unknown fields keeps the receipt shape
    // stable across platforms so fixture assertions can target
    // `recycle_bin_entry.original_path` uniformly.
    //
    // Pre-fix: the entry was gated on at least one of
    // container/info/data being Some, which skipped Windows
    // entirely (TrashOutcome::default()) — harness fixtures had to
    // strip the field from their expected_receipt_fields rather
    // than asserting on it. Per sdd-testwin full-#12 R3 finding.
    if matches!(action, DedupeAction::Recycle) && error.is_none() {
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

// §7.1 — OS error code for a cross-volume link/clone attempt, used to
// classify a hardlink/reflink failure as refused_cross_volume rather
// than a generic error. Windows: ERROR_NOT_SAME_DEVICE. Unix: EXDEV.
#[cfg(windows)]
const CROSS_DEVICE_ERRNO: i32 = 17;
#[cfg(not(windows))]
const CROSS_DEVICE_ERRNO: i32 = 18;

/// True when `err` is the platform's cross-volume error (the underlying
/// op now propagates it — see the create_hard_link P0 fix).
fn is_cross_device(err: &Error) -> bool {
    matches!(err, Error::Io(e) if e.raw_os_error() == Some(CROSS_DEVICE_ERRNO))
}

fn pick_keeper(
    group: &DuplicateGroup,
    strategy: KeepStrategy,
    references: &BTreeMap<PathBuf, ()>,
) -> Result<usize> {
    // Reference paths always win, regardless of strategy.
    for (i, p) in group.files.iter().enumerate() {
        if references.contains_key(&canonical_key(p)) {
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
    // Keeper-identity chokepoint backstop. process_group's planner gate
    // refuses + emits before reaching here on the normal CLI path, so this
    // is defense-in-depth (independent gate per the directive) — and it
    // guards any future caller that reaches perform_action directly.
    guard_keeper_identity(action, path, Some(keeper))?;
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

/// Keeper-preservation gate 2 at the ACTION LAYER — shared by the CLI
/// chokepoint (`perform_action`) AND the GUI direct-action entry points
/// (`action_*`). The GUI dedupe path runs NONE of the CLI planner's
/// keeper-safety stack (no pick_keeper / reference / system-path / identity
/// guards — it calls `action_*` directly), so without this it deletes a
/// keeper reached via an alias. This is the one placement that protects
/// BOTH clients.
///
/// Refuses a DELETE-the-loser action (remove / recycle / safe-rename) on a
/// `target` that resolves by file-IDENTITY to `keeper` (the keeper via a
/// `\\?\`/8.3/junction/symlink/case alias — `file_identity` collapses them
/// all). For permanent `remove` (irreversible), also refuses when identity
/// can't be positively resolved (fail-closed; recycle/safe-rename are
/// recoverable so they tolerate transient unopenability). hardlink/reflink
/// replace-with-link (don't delete data) and aren't gated here — a positive
/// match there is the benign already-linked no-op. `keeper == None` ⇒ caller
/// asserts no keeper to protect (non-dedup) ⇒ no gate.
fn guard_keeper_identity(action: DedupeAction, target: &Path, keeper: Option<&Path>) -> Result<()> {
    if !matches!(
        action,
        DedupeAction::Remove | DedupeAction::Recycle | DedupeAction::SafeRename
    ) {
        return Ok(());
    }
    let Some(keeper) = keeper else { return Ok(()) };
    let kid = file_identity(keeper);
    let mid = file_identity(target);
    if matches!((kid, mid), (Some(k), Some(m)) if k == m) {
        return Err(Error::other(format!(
            "refused: {} resolves by file-id to the keeper {} (same physical \
             file via an alias) — destructive action would delete the keeper",
            target.display(),
            keeper.display(),
        )));
    }
    if matches!(action, DedupeAction::Remove) && (kid.is_none() || mid.is_none()) {
        return Err(Error::other(format!(
            "refused: could not resolve the file-id of the keeper and/or {} to \
             prove it distinct from the keeper — permanent remove is \
             irreversible, so it is preserved (fail-closed)",
            target.display(),
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
///
/// The delete-the-loser variants also take the group's `keeper` and run
/// the keeper-identity gate, so the GUI path (which doesn't go through the
/// CLI planner) can't delete the keeper reached via an alias. Pass
/// `Some(keeper)` from any dedup flow; `None` only when there is genuinely
/// no keeper to protect. They also take `references` (the user's reference
/// paths/roots) and run `guard_reference`, so the GUI can't delete a copy
/// under a reference root reached via an alias. Pass `&[]` when there are no
/// references to protect.
/// v0.2.36 relocation gate — system-path. The CLI planner refuses
/// system-critical paths in `process_group` (honouring the
/// `--allow-system-paths` escape hatch); the GUI runs none of that, so this
/// action-layer guard closes the GUI gap. The GUI has NO allow-system-paths
/// escape, so it refuses UNCONDITIONALLY here — a GUI dedup must never
/// delete/replace a file under `C:\Windows`, `C:\Program Files`, etc. Lives
/// on the `action_*` entry points only (NOT in the shared `guard_destructive`,
/// which `perform_action` also calls — that would override the CLI's
/// `--allow-system-paths` flag).
fn guard_system_path(path: &Path) -> Result<()> {
    if resolves_to_system_path(path) {
        return Err(Error::other(format!(
            "refused: {} is under (or resolves to) a system-critical path; \
             refusing to delete/replace a system file",
            path.display(),
        )));
    }
    Ok(())
}

/// Whether `path` IS, or canonically RESOLVES to, an OS-critical path. Shared
/// by the GUI action layer (`guard_system_path`) AND the CLI planner
/// (`process_group`'s system-path skip) so the two AGREE on what's refused —
/// v0.2.36 finding #1: the prior asymmetry (GUI canonical-checked, CLI
/// raw-only) let the GUI over-refuse while the CLI under-refused the same path.
///
/// `is_system_path` is path-string-based (for_user_display + prefix-match):
/// robust vs the `\\?\` verbatim prefix (F-CLI-4) but NOT vs junction / 8.3 /
/// symlink aliases to a system dir (e.g. `C:\link -> C:\Windows`, then
/// `C:\link\foo.dll` wouldn't match the `C:\Windows` prefix). Resolving the REAL
/// target via `canonical_key` and checking that too defeats the alias class —
/// the same string-vs-identity hardening as keeper gate-2. The raw check stays
/// as the fallback for when canonicalize fails (target gone) but the raw path is
/// already a system prefix.
fn resolves_to_system_path(path: &Path) -> bool {
    is_system_path(path) || is_system_path(&canonical_key(path))
}

/// v0.2.36 relocation gate — reference protection. The CLI planner skips
/// every reference member in `process_group` (F-SAFE-2: `references.
/// contains_key(&canonical_key(path))`, canonical-keyed). The GUI runs none
/// of that: its bulk paths filter reference roots with a RAW
/// `path.starts_with(root)` prefix match (caller-side, alias-vulnerable — a
/// `\\?\` verbatim / 8.3 / junction / symlink / case alias under a reference
/// root slips PAST `starts_with`, exactly the canonical_key asymmetry class as
/// F-SAFE-2 / F-CLI-7 / the P0), and the per-group + single-row seam
/// (`run_one_dedupe_action`) had NO reference protection at ALL. This gate
/// moves the check DOWN to the action layer, canonical-keyed, so it protects
/// every GUI path and defeats the whole alias class.
///
/// Refuses a destructive action when `target` IS, or resolves UNDER, any
/// `reference` entry — canonicalizing BOTH sides first (the GUI's raw
/// `starts_with` is the bug we're closing). `reference` may be exact file
/// paths (CLI `reference_paths`) OR root directories (GUI reference roots) —
/// the equals-or-canonical-ancestor test covers both. Empty `reference` ⇒
/// caller asserts no references to protect ⇒ no gate. Lives on the `action_*`
/// entry points only (mirrors `guard_system_path`); the CLI is already
/// protected at the planner, so `perform_action` doesn't re-run it.
fn guard_reference(target: &Path, reference: &[PathBuf]) -> Result<()> {
    if reference.is_empty() {
        return Ok(());
    }
    let canon_target = canonical_key(target);
    for r in reference {
        let canon_ref = canonical_key(r);
        // Component-wise: PathBuf::starts_with won't false-match across a
        // component boundary (e.g. /a/bc does not start_with /a/b).
        if canon_target == canon_ref || canon_target.starts_with(&canon_ref) {
            return Err(Error::other(format!(
                "refused: {} is (or resolves under) the reference path {} — \
                 references are never modified",
                target.display(),
                r.display(),
            )));
        }
    }
    Ok(())
}

pub fn action_remove(path: &Path, keeper: Option<&Path>, references: &[PathBuf]) -> Result<()> {
    guard_destructive(path, false)?;
    guard_system_path(path)?;
    guard_reference(path, references)?;
    guard_keeper_identity(DedupeAction::Remove, path, keeper)?;
    fs::remove_file(path)?;
    Ok(())
}

pub fn action_recycle(path: &Path, keeper: Option<&Path>, references: &[PathBuf]) -> Result<()> {
    guard_destructive(path, false)?;
    guard_system_path(path)?;
    guard_reference(path, references)?;
    guard_keeper_identity(DedupeAction::Recycle, path, keeper)?;
    // Drop the TrashOutcome — the GUI's per-row recycle path doesn't
    // emit receipts (it's only the `dedupe` subcommand flow that
    // surfaces them). Future: thread metadata into a per-action
    // event the GUI can render in the action-progress modal.
    recycle(path)?;
    Ok(())
}

// hardlink/reflink ALSO take `references` + run guard_reference: replace-with-link
// renames the target aside, links it to the keeper, and removes the original —
// for a REFERENCE target that destroys the reference's independent copy and
// repoints its path at the keeper, i.e. it MODIFIES the reference ("references
// are never modified. Period." — module contract). Same rationale as
// guard_system_path on these two (replace-with-link also touches the file) and
// matches the CLI, whose process_group reference skip runs before EVERY action
// incl. hardlink/reflink (quality finding on a11499a). NOT keeper-identity-
// guarded — a positive keeper match under hardlink/reflink is the benign
// already-linked no-op.
pub fn action_hardlink(target: &Path, keeper: &Path, references: &[PathBuf]) -> Result<()> {
    guard_destructive(target, false)?;
    guard_system_path(target)?;
    guard_reference(target, references)?;
    replace_with_hardlink(target, keeper)
}

pub fn action_reflink(target: &Path, keeper: &Path, references: &[PathBuf]) -> Result<()> {
    guard_destructive(target, false)?;
    guard_system_path(target)?;
    guard_reference(target, references)?;
    replace_with_reflink(target, keeper)
}

/// Safe-mode rename: append `.superdeduper` to the target. Idempotent —
/// files already ending in the suffix are a no-op. Reversible via
/// `unsuperdeduper_root`. Never deletes anything.
pub fn action_safe_rename(
    target: &Path,
    keeper: Option<&Path>,
    references: &[PathBuf],
) -> Result<()> {
    guard_destructive(target, false)?;
    guard_system_path(target)?;
    guard_reference(target, references)?;
    guard_keeper_identity(DedupeAction::SafeRename, target, keeper)?;
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

/// Canonicalize one path for reference matching. Tolerates non-existent
/// paths by falling back to the input. Both the reference-set keys
/// (`canonical_set`) and every group member checked against them go
/// through this, so matching is consistent — otherwise a scanned path
/// whose representation differs from its canonical form (symlinked root,
/// Windows `\\?\` verbatim prefix) would never match its own reference
/// entry, and in-reference would silently skip the group instead of
/// keeping the reference copy.
fn canonical_key(p: &Path) -> PathBuf {
    fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Build a canonical lookup set from the scan-persisted reference paths.
fn canonical_set(paths: &[PathBuf]) -> BTreeMap<PathBuf, ()> {
    paths.iter().map(|p| (canonical_key(p), ())).collect()
}

/// Real physical-file identity: (volume, file-id). Two paths with the
/// same `FileIdentity` are the SAME on-disk file regardless of how the
/// path was spelled. Windows: (VolumeSerialNumber, 128-bit FileId) from
/// `GetFileInformationByHandleEx(FileIdInfo)`. Unix: (st_dev, st_ino).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    volume: u64,
    file_id: u128,
}

/// Resolve a path to its physical-file identity by OPENING it and
/// querying the kernel — the keeper-preservation file-id gate (Mick
/// safety directive gate 2).
///
/// Opening a handle is inherently alias-immune: `\\?\C:\x`, `C:\x`, the
/// 8.3 short name, a junction/symlink path, and a case-variant ALL open
/// the same physical file and report the same `(volume, file_id)`. So
/// this one resolution defeats the entire verbatim-prefix/alias class
/// (the recurring #34 / #118 / F-CLI-4 family) on the destructive path —
/// far stronger than any path-string comparison.
///
/// `None` when the file can't be opened/queried (deleted, locked, FS
/// without the info). Callers must treat `None` as "can't prove identity"
/// and NOT delete on a guess — but they also must not OVER-refuse on
/// `None` (that would block legitimate deletes of transiently-locked
/// files); the gate refuses only on a POSITIVE id match.
fn file_identity(path: &Path) -> Option<FileIdentity> {
    file_identity_impl(path)
}

#[cfg(windows)]
fn file_identity_impl(path: &Path) -> Option<FileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    // FILE_FLAG_BACKUP_SEMANTICS (0x02000000) lets the same call work for
    // directories too; harmless for files. We do NOT pass
    // FILE_FLAG_OPEN_REPARSE_POINT — we WANT to follow junctions/symlinks
    // to the real target (that's the AKP-4 alias-defeat). Placeholders are
    // already refused upstream (refused_placeholder) so this open can't
    // hydrate one on the destructive path.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x0200_0000)
        .open(path)
        .ok()?;
    let handle = file.as_raw_handle() as isize;
    let mut info = windows::Win32::Storage::FileSystem::FILE_ID_INFO::default();
    // SAFETY: handle is valid (owned by `file`); `info` has the correct
    // type + size for the `FileIdInfo` class.
    let ok = unsafe {
        windows::Win32::Storage::FileSystem::GetFileInformationByHandleEx(
            windows::Win32::Foundation::HANDLE(handle as _),
            windows::Win32::Storage::FileSystem::FileIdInfo,
            &mut info as *mut _ as *mut std::ffi::c_void,
            std::mem::size_of::<windows::Win32::Storage::FileSystem::FILE_ID_INFO>() as u32,
        )
    };
    if ok.is_err() {
        return None;
    }
    Some(FileIdentity {
        volume: info.VolumeSerialNumber,
        file_id: u128::from_le_bytes(info.FileId.Identifier),
    })
}

#[cfg(unix)]
fn file_identity_impl(path: &Path) -> Option<FileIdentity> {
    use std::os::unix::fs::MetadataExt;
    // `metadata` (not symlink_metadata) follows symlinks to the real
    // target — the alias-defeat on Unix.
    let m = fs::metadata(path).ok()?;
    Some(FileIdentity {
        volume: m.dev(),
        file_id: m.ino() as u128,
    })
}

#[cfg(not(any(windows, unix)))]
fn file_identity_impl(_path: &Path) -> Option<FileIdentity> {
    None
}

/// Return true if `path` falls under any of the platform's
/// system-critical prefixes. Windows enumerates the well-known paths
/// from the spec; other platforms use a sensible default for testing.
pub fn is_system_path(path: &Path) -> bool {
    // F-CLI-4 — normalize the Windows verbatim prefix (`\\?\`) before
    // matching. The engine walks with verbatim paths internally (Win32
    // long-path support), so a raw `to_string_lossy()` is
    // `\\?\C:\Windows\…`, which would NOT match the `c:\windows`
    // prefixes below — silently bypassing the system-path guard for the
    // exact destructive-action path users hit. `for_user_display` strips
    // the prefix (and passes Linux/regular paths through unchanged).
    let s = crate::path_display::for_user_display(path).to_ascii_lowercase();
    #[cfg(windows)]
    {
        // OS-CRITICAL only: deleting/replacing under these breaks the OS or
        // installed software. The hard-refuse guard is for "this breaks the
        // OS," NOT "this is your app data." v0.2.36 finding #1 (Mick ruling,
        // option b): the prior %USERPROFILE%\AppData-wide clause was dropped —
        // AppData\Local\Temp + caches are PRIME dedup targets, so hard-refusing
        // all of AppData hobbled the tool. Fine-grained "maybe don't dedup app
        // configs" stays the exclusions PRESET's job (overridable), not this
        // hard guard.
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
            file_sizes: Vec::new(),
        }
    }

    fn results(groups: Vec<DuplicateGroup>) -> ResultsFile {
        ResultsFile {
            schema: "superdeduper.scan.v1".into(),
            groups,
            summary: None,
            reference_paths: Vec::new(),
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
        action_remove(&f, None, &[]).unwrap();
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

    // F-CLI-7 — the scan→dedupe-file two-step must honour
    // `--strategy in-reference`: the reference member persisted in the
    // results file's `reference_paths` is kept, the non-reference dupe
    // is removed.
    fn in_reference_args(results_path: PathBuf) -> DedupeArgs {
        DedupeArgs {
            results_file: results_path,
            strategy: KeepStrategy::InReference,
            action: DedupeAction::Remove,
            mode: crate::cli::ScanMode::Exact,
            dry_run: false,
            allow_system_paths: false,
            allow_destructive_on_deduped: false,
            integration_test_mode: false,
            receipt_file: None,
        }
    }

    #[test]
    fn in_reference_keeps_the_reference_file_via_persisted_paths() {
        let d = tmpdir();
        let reference = d.join("master.bin");
        let dupe = d.join("copy.bin");
        write_file(&reference, b"same");
        write_file(&dupe, b"same");
        let mut r = results(vec![group(4, vec![dupe.clone(), reference.clone()])]);
        r.reference_paths = vec![reference.clone()];
        let path = write_results(&d, &r);
        let outcome = run(&in_reference_args(path)).unwrap();
        assert_eq!(outcome.executed, 1, "the non-reference dupe is removed");
        assert!(reference.exists(), "reference copy must be kept");
        assert!(!dupe.exists(), "non-reference dupe must be removed");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn in_reference_skips_groups_with_no_reference_member() {
        // A scan with reference roots can still yield duplicate groups
        // where none of the members live under a reference — those are
        // skipped cleanly, not config-failed.
        let d = tmpdir();
        let reference = d.join("master.bin");
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write_file(&reference, b"ref");
        write_file(&a, b"same");
        write_file(&b, b"same");
        // Group has no reference member; reference_paths is non-empty so
        // run()'s up-front "no refs at all" guard does not fire.
        let mut r = results(vec![group(4, vec![a.clone(), b.clone()])]);
        r.reference_paths = vec![reference.clone()];
        let path = write_results(&d, &r);
        let outcome = run(&in_reference_args(path)).unwrap();
        assert_eq!(outcome.executed, 0, "no-reference group is skipped");
        assert_eq!(outcome.failed, 0, "skipping must not count as a failure");
        assert!(a.exists() && b.exists(), "both members untouched");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn in_reference_without_any_reference_paths_is_a_config_error() {
        let d = tmpdir();
        let a = d.join("a.bin");
        let b = d.join("b.bin");
        write_file(&a, b"same");
        write_file(&b, b"same");
        // No reference_paths at all → clean up-front config error.
        let r = results(vec![group(4, vec![a.clone(), b.clone()])]);
        let path = write_results(&d, &r);
        let err = run(&in_reference_args(path)).unwrap_err();
        assert!(
            matches!(err, Error::ConfigInvalid { field: "--strategy", .. }),
            "expected a --strategy config error, got: {err:?}"
        );
        assert!(a.exists() && b.exists(), "nothing removed on config error");
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

    // §7.1 — receipt-emission contract for the containment matrix.
    // Helper: run with integration-test-mode + a receipt file and return
    // the NDJSON lines.
    fn run_capturing_receipts(d: &Path, r: &ResultsFile, action: DedupeAction, dry_run: bool) -> String {
        let path = write_results(d, r);
        let receipt_path = d.join("receipts.jsonl");
        let args = DedupeArgs {
            results_file: path,
            strategy: KeepStrategy::First,
            action,
            mode: crate::cli::ScanMode::Exact,
            dry_run,
            allow_system_paths: false,
            allow_destructive_on_deduped: false,
            integration_test_mode: true,
            receipt_file: Some(receipt_path.clone()),
        };
        run(&args).unwrap();
        fs::read_to_string(&receipt_path).unwrap_or_default()
    }

    #[test]
    fn keeper_emits_left_alone_receipt() {
        let d = tmpdir();
        let keeper = d.join("a_keeper.bin");
        let dupe = d.join("b_dupe.bin");
        write_file(&keeper, b"same");
        write_file(&dupe, b"same");
        let r = results(vec![group(4, vec![keeper.clone(), dupe.clone()])]);
        // dry-run: the keeper's left_alone emit is before the dry-run
        // branch, so it fires regardless + we avoid touching the backend.
        let receipts = run_capturing_receipts(&d, &r, DedupeAction::Remove, true);
        assert!(
            receipts.contains("\"outcome\":\"left_alone\""),
            "keeper must carry an explicit left_alone receipt, got: {receipts}"
        );
        assert!(
            receipts.contains("a_keeper.bin"),
            "left_alone receipt must name the keeper, got: {receipts}"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn system_path_member_emits_refused_system_path() {
        let d = tmpdir();
        // is_system_path is a pure prefix check (no file access) and the
        // guard returns before any member is stat'd, so a synthetic
        // system path is sufficient. Pairs with a normal member so the
        // group has >= 2 files.
        #[cfg(windows)]
        let sys = PathBuf::from("c:\\windows\\system32\\evil.dll");
        #[cfg(not(windows))]
        let sys = PathBuf::from("/var/lib/superdeduper-test/evil.bin");
        let normal = d.join("ok.bin");
        write_file(&normal, b"same");
        let r = results(vec![group(4, vec![sys.clone(), normal.clone()])]);
        let receipts = run_capturing_receipts(&d, &r, DedupeAction::Remove, false);
        assert!(
            receipts.contains("\"outcome\":\"refused_system_path\""),
            "a system-path member must emit a refused_system_path receipt, got: {receipts}"
        );
        assert!(normal.exists(), "the whole group is refused; nothing removed");
        fs::remove_dir_all(&d).ok();
    }

    // already_hardlinked needs real inodes; only meaningful on Unix (the
    // Windows file_index is a placeholder until plumbed — see action_receipt).
    #[cfg(unix)]
    #[test]
    fn already_hardlinked_member_is_a_noop_receipt() {
        let d = tmpdir();
        let keeper = d.join("a_keeper.bin");
        let linked = d.join("b_linked.bin");
        write_file(&keeper, b"same");
        std::fs::hard_link(&keeper, &linked).unwrap(); // shares keeper's inode
        let r = results(vec![group(4, vec![keeper.clone(), linked.clone()])]);
        let receipts = run_capturing_receipts(&d, &r, DedupeAction::Hardlink, false);
        assert!(
            receipts.contains("\"outcome\":\"already_hardlinked\""),
            "a member already sharing the keeper inode must emit already_hardlinked, got: {receipts}"
        );
        assert!(linked.exists(), "already-linked member is left in place (no-op)");
        fs::remove_dir_all(&d).ok();
    }

    // Keeper-preservation gate 2 (Mick safety directive) — DATA-LOSS guard.
    // A non-keeper member that resolves by FILE-IDENTITY to the keeper (the
    // keeper reached via an alias) must be REFUSED on a destructive action,
    // never deleted. Linux models the AKP-4 junction vector with a symlinked
    // PARENT DIR: real/x is the keeper, link/x reaches the SAME physical file
    // through the dir symlink. Without the gate, `remove` of link/x follows
    // the symlink and DELETES real/x — destroying the keeper. Negative-control
    // verified: disabling the gate makes this test fail (keeper deleted).
    #[cfg(unix)]
    #[test]
    fn keeper_identity_refuses_destructive_on_dir_aliased_keeper() {
        let d = tmpdir();
        let real = d.join("real");
        fs::create_dir_all(&real).unwrap();
        let keeper = real.join("x.bin");
        write_file(&keeper, b"precious");
        // Symlinked parent dir -> link/x.bin is the SAME physical file as
        // real/x.bin (the junction-alias class), but a different path string.
        let link = d.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let alias = link.join("x.bin");

        // --strategy first => keeper = real/x.bin (index 0); the aliased
        // path is the non-keeper that flows to the destructive path.
        let r = results(vec![group(8, vec![keeper.clone(), alias.clone()])]);
        let receipts = run_capturing_receipts(&d, &r, DedupeAction::Remove, false);
        assert!(
            receipts.contains("\"outcome\":\"refused_keeper_identity\""),
            "the aliased keeper member must emit refused_keeper_identity, got: {receipts}"
        );
        assert!(
            keeper.exists() && fs::read(&keeper).unwrap() == b"precious",
            "DATA-LOSS GUARD: the keeper (reached via the dir-symlink alias) must NOT be deleted"
        );
        fs::remove_dir_all(&d).ok();
    }

    // AKP-5 destructive: a hardlink-to-keeper under a DESTRUCTIVE action is
    // refused (fail-closed), per the locked contract — even though deleting
    // the loser NAME would be data-safe by nlink semantics, the file-id gate
    // refuses uniformly (no nlink special-case).
    #[cfg(unix)]
    #[test]
    fn keeper_identity_refuses_remove_of_hardlinked_member() {
        let d = tmpdir();
        let keeper = d.join("k.bin");
        let linked = d.join("l.bin");
        write_file(&keeper, b"same");
        std::fs::hard_link(&keeper, &linked).unwrap();
        let r = results(vec![group(4, vec![keeper.clone(), linked.clone()])]);
        let receipts = run_capturing_receipts(&d, &r, DedupeAction::Remove, false);
        assert!(
            receipts.contains("\"outcome\":\"refused_keeper_identity\""),
            "remove of a hardlink-to-keeper must refuse (file-id match), got: {receipts}"
        );
        assert!(keeper.exists() && linked.exists(), "neither name removed");
        fs::remove_dir_all(&d).ok();
    }

    // Negative control — the gate must NOT over-refuse genuinely-DIFFERENT
    // files (distinct identity). A normal dup is still removed. (Pairs with
    // remove_action_deletes_non_keeper; asserts the new outcome counter.)
    #[test]
    fn keeper_identity_does_not_refuse_distinct_file() {
        let d = tmpdir();
        let keeper = d.join("keep.bin");
        let dupe = d.join("dup.bin");
        write_file(&keeper, b"same");
        write_file(&dupe, b"same");
        let r = results(vec![group(4, vec![keeper.clone(), dupe.clone()])]);
        let args = make_args(write_results(&d, &r), false, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert_eq!(outcome.skipped_keeper_identity, 0, "distinct file must not be id-refused");
        assert_eq!(outcome.executed, 1, "the genuine dup is removed");
        assert!(keeper.exists() && !dupe.exists());
        fs::remove_dir_all(&d).ok();
    }

    // Quality Finding 1 (HIGH) — fail-CLOSED when the KEEPER's identity is
    // unresolvable under permanent `remove`. An unresolvable keeper would
    // otherwise disable gate-2 for the whole group, letting an
    // aliased-keeper member be deleted. We model an unresolvable keeper with
    // a BROKEN symlink (fs::metadata follows it -> errors -> file_identity
    // None). Under `remove`, a resolvable member must then be REFUSED
    // (refused_keeper_unresolvable), not deleted. Negative-control verified:
    // without the fail-closed branch the member is removed (gate disabled by
    // the None keeper).
    #[cfg(unix)]
    #[test]
    fn unresolvable_keeper_fails_closed_under_remove() {
        let d = tmpdir();
        // keeper = broken symlink -> file_identity(keeper) == None.
        let keeper = d.join("a_keeper.bin");
        std::os::unix::fs::symlink(d.join("missing-target"), &keeper).unwrap();
        let member = d.join("b_member.bin");
        write_file(&member, b"data");
        // --strategy first => keeper_idx = 0 (the broken symlink).
        let r = results(vec![group(4, vec![keeper.clone(), member.clone()])]);
        let receipts = run_capturing_receipts(&d, &r, DedupeAction::Remove, false);
        assert!(
            receipts.contains("\"outcome\":\"refused_keeper_unresolvable\""),
            "unresolvable keeper under remove must fail closed, got: {receipts}"
        );
        assert!(
            member.exists(),
            "DATA-LOSS GUARD: member must NOT be removed when the keeper's identity \
             can't be resolved under permanent remove"
        );
        fs::remove_dir_all(&d).ok();
    }

    // Action-layer keeper guard (the GUI-facing entry points — quality
    // proved the GUI runs none of the planner stack). action_remove /
    // action_safe_rename must REFUSE a target that resolves by file-id to
    // the keeper, even though they're called directly (no process_group).
    #[cfg(unix)]
    #[test]
    fn action_layer_guard_refuses_keeper_alias() {
        let d = tmpdir();
        let keeper = d.join("keeper.bin");
        let alias = d.join("alias.bin");
        write_file(&keeper, b"same");
        std::fs::hard_link(&keeper, &alias).unwrap(); // same physical file
        assert!(
            action_remove(&alias, Some(&keeper), &[]).is_err(),
            "action_remove must refuse a keeper-alias target"
        );
        assert!(
            action_safe_rename(&alias, Some(&keeper), &[]).is_err(),
            "action_safe_rename must refuse a keeper-alias target"
        );
        assert!(alias.exists() && keeper.exists(), "both must remain after refusal");
        // A genuinely distinct file is NOT refused (no over-refusal).
        let other = d.join("other.bin");
        write_file(&other, b"same");
        assert!(
            action_remove(&other, Some(&keeper), &[]).is_ok(),
            "a distinct file must not be id-refused"
        );
        assert!(!other.exists(), "the distinct file was removed");
        fs::remove_dir_all(&d).ok();
    }

    // Counterpart to Finding 2: the SAME unresolvable-keeper case under a
    // REVERSIBLE action (recycle) does NOT over-refuse — the transient-lock
    // tolerance is kept where a wrong call is recoverable. (Asserts the
    // fail-closed branch is correctly scoped to irreversible remove only.)
    #[cfg(target_os = "linux")]
    #[test]
    fn unresolvable_keeper_does_not_over_refuse_under_recycle() {
        let d = tmpdir();
        let keeper = d.join("a_keeper.bin");
        std::os::unix::fs::symlink(d.join("missing-target"), &keeper).unwrap();
        let member = d.join("b_member.bin");
        write_file(&member, b"data");
        let r = results(vec![group(4, vec![keeper.clone(), member.clone()])]);
        let args = DedupeArgs {
            results_file: write_results(&d, &r),
            strategy: KeepStrategy::First,
            action: DedupeAction::Recycle,
            mode: crate::cli::ScanMode::Exact,
            dry_run: false,
            allow_system_paths: false,
            allow_destructive_on_deduped: false,
            integration_test_mode: false,
            receipt_file: None,
        };
        let outcome = run(&args).unwrap();
        assert_eq!(
            outcome.skipped_keeper_identity, 0,
            "recycle is reversible — must NOT fail-closed-refuse on unresolvable id"
        );
        fs::remove_dir_all(&d).ok();
    }

    // #147 — perceptual group members have different real sizes; the
    // changed-since-scan guard must check each against ITS recorded
    // size, not the group representative (the largest member). Pre-fix
    // the smaller non-keeper was rejected as "size changed" and the
    // whole perceptual-dedup workflow silently no-op'd.
    #[test]
    fn perceptual_member_validated_against_own_size_not_group_rep() {
        let d = tmpdir();
        let keeper = d.join("a_keeper.flac"); // 100 bytes, group representative
        let dupe = d.join("b_dupe.flac"); // 50 bytes — differs from group.size
        write_file(&keeper, &vec![0u8; 100]);
        write_file(&dupe, &vec![0u8; 50]);
        let mut g = group(100, vec![keeper.clone(), dupe.clone()]);
        g.similarity_kind = SimilarityKind::PerceptualAudio;
        // index-aligned with files: [keeper=100, dupe=50]
        g.file_sizes = vec![100, 50];
        let r = results(vec![g]);
        let path = write_results(&d, &r);
        // dry-run exercises validation without depending on the recycle
        // backend; KeepStrategy::First keeps the 100-byte keeper, so the
        // 50-byte dupe is the member that pre-fix failed against size=100.
        let args = make_args(path, true, DedupeAction::Recycle);
        let outcome = run(&args).unwrap();
        assert_eq!(
            outcome.skipped_invalidated, 0,
            "the 50-byte perceptual member must validate against its own size, not group.size=100"
        );
        assert_eq!(
            outcome.executed, 1,
            "the non-keeper member should validate + be actioned"
        );
        fs::remove_dir_all(&d).ok();
    }

    // #147 NIT — the per-file size source must STILL catch a perceptual
    // member that changed on disk since the scan. Recorded file_sizes[i]
    // = 50 but the file is 60 bytes now → the guard must fire (this pins
    // "we didn't blanket-disable the changed-since-scan guard for
    // perceptual groups"; a refactor that short-circuits validation for
    // perceptual kinds would re-open the TOCTOU hole + fail here).
    #[test]
    fn perceptual_member_changed_since_scan_is_still_caught() {
        let d = tmpdir();
        let keeper = d.join("a_keeper.flac");
        let dupe = d.join("b_dupe.flac");
        write_file(&keeper, &vec![0u8; 100]);
        // On-disk size (60) ≠ the recorded scan-time size (50) → changed.
        write_file(&dupe, &vec![0u8; 60]);
        let mut g = group(100, vec![keeper.clone(), dupe.clone()]);
        g.similarity_kind = SimilarityKind::PerceptualAudio;
        g.file_sizes = vec![100, 50];
        let r = results(vec![g]);
        let path = write_results(&d, &r);
        let args = make_args(path, true, DedupeAction::Recycle);
        let outcome = run(&args).unwrap();
        assert_eq!(
            outcome.skipped_invalidated, 1,
            "a perceptual member modified since scan (recorded 50, on-disk 60) must be caught"
        );
        assert_eq!(outcome.executed, 0, "the changed member must not be actioned");
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

    // v0.2.36 relocation — the GUI action layer (action_*) now runs an
    // UNCONDITIONAL system-path refusal (no --allow-system-paths escape on the
    // GUI). Directly exercise the guard: a system-prefixed path is refused, a
    // normal path passes. (The action_* wiring is verified by inspection +
    // testdesign's adversarial GUI system-path cell on a real system file;
    // a unit test can't plant a real C:\Windows file, and guard_destructive's
    // metadata check would mask a non-existent one.)
    #[test]
    fn guard_system_path_refuses_system_prefix_allows_normal() {
        let d = tmpdir();
        #[cfg(windows)]
        let sys = PathBuf::from("C:\\Windows\\System32\\evil.dll");
        #[cfg(not(windows))]
        let sys = PathBuf::from("/etc/shadow");
        assert!(
            guard_system_path(&sys).is_err(),
            "a system-critical path must be refused at the action layer"
        );
        let normal = d.join("ok.bin");
        assert!(
            guard_system_path(&normal).is_ok(),
            "a normal path must pass the action-layer system guard"
        );
        fs::remove_dir_all(&d).ok();
    }

    // Hardening (quality forward-note on 372f42e): an ALIAS to a system dir
    // must also be refused. A symlink whose target is a system dir has a raw
    // path that is NOT a system prefix, so only the canonical_key resolution
    // catches it. Without the canonical check this passes (raw not a system
    // prefix) -> the symlink-aliased system path would be actioned.
    #[cfg(unix)]
    #[test]
    fn guard_system_path_refuses_symlink_alias_to_system_dir() {
        let d = tmpdir();
        // /usr/bin is under the "/usr/" system prefix and exists on any Linux
        // box (canonicalize needs the target to exist to resolve). Symlink to
        // it from a non-system tmpdir path: the raw string is NOT a system
        // prefix, but the canonical resolution IS.
        let target = std::path::Path::new("/usr/bin");
        if !target.exists() {
            return; // no system dir to alias here; skip
        }
        let link = d.join("syslink");
        if std::os::unix::fs::symlink(target, &link).is_err() {
            return; // environment can't symlink; skip
        }
        assert!(
            !is_system_path(&link),
            "precondition: the raw alias path is NOT a system prefix"
        );
        assert!(
            guard_system_path(&link).is_err(),
            "a symlink resolving to a system dir must be refused (canonical check)"
        );
        fs::remove_dir_all(&d).ok();
    }

    // v0.2.36 relocation #2 — guard_reference at the action layer.
    #[test]
    fn guard_reference_allows_when_no_references() {
        let d = tmpdir();
        let f = d.join("dupe.bin");
        write_file(&f, b"x");
        assert!(
            guard_reference(&f, &[]).is_ok(),
            "empty reference set ⇒ no gate"
        );
        let other = d.join("ref_root");
        fs::create_dir_all(&other).unwrap();
        assert!(
            guard_reference(&f, std::slice::from_ref(&other)).is_ok(),
            "a file NOT under any reference root must be allowed (no over-refusal)"
        );
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn guard_reference_refuses_member_under_reference_root() {
        let d = tmpdir();
        let ref_root = d.join("library");
        fs::create_dir_all(&ref_root).unwrap();
        let member = ref_root.join("keep_me.bin");
        write_file(&member, b"x");
        assert!(
            guard_reference(&member, std::slice::from_ref(&ref_root)).is_err(),
            "a file UNDER a reference root must be refused"
        );
        // And the reference path itself (exact-match, e.g. CLI reference_paths).
        assert!(
            guard_reference(&member, std::slice::from_ref(&member)).is_err(),
            "the reference path itself must be refused (exact match)"
        );
        fs::remove_dir_all(&d).ok();
    }

    // The whole point of relocating to the action layer: the GUI's old
    // caller-side `path.starts_with(reference_root)` filter is RAW-string and
    // misses a member reached via a symlink alias whose path doesn't share the
    // reference root's prefix. guard_reference canonicalizes BOTH sides, so the
    // alias is caught. This is the F-SAFE-2 canonical_key asymmetry, on the GUI
    // path. Negative-controlled by the allow test above.
    #[cfg(unix)]
    #[test]
    fn guard_reference_refuses_symlink_alias_into_reference_root() {
        let d = tmpdir();
        let ref_root = d.join("library");
        fs::create_dir_all(&ref_root).unwrap();
        let member = ref_root.join("keep_me.bin");
        write_file(&member, b"x");
        // Alias the member from a path OUTSIDE the reference root — a raw
        // starts_with(ref_root) would NOT match this alias.
        let alias = d.join("sneaky_alias.bin");
        if std::os::unix::fs::symlink(&member, &alias).is_err() {
            return; // environment can't symlink; skip
        }
        assert!(
            !alias.starts_with(&ref_root),
            "precondition: the raw alias path does NOT share the reference root prefix"
        );
        assert!(
            guard_reference(&alias, std::slice::from_ref(&ref_root)).is_err(),
            "a symlink alias resolving UNDER a reference root must be refused \
             (canonical check — the GUI's raw starts_with filter missed this)"
        );
        fs::remove_dir_all(&d).ok();
    }

    // quality finding on a11499a: hardlink/reflink ALSO modify a reference
    // (replace-with-link destroys the reference's independent copy + repoints it
    // at the keeper), so action_hardlink/action_reflink must guard_reference too
    // — not just the 3 delete variants. Reachability: GUI keeper selection is
    // user-driven, so a reference can be a hardlink/reflink target.
    #[test]
    fn action_hardlink_refuses_reference_target() {
        let d = tmpdir();
        let ref_root = d.join("library");
        fs::create_dir_all(&ref_root).unwrap();
        let reference_member = ref_root.join("master.bin");
        write_file(&reference_member, b"master");
        let keeper = d.join("keeper.bin");
        write_file(&keeper, b"master");
        assert!(
            action_hardlink(&reference_member, &keeper, std::slice::from_ref(&ref_root)).is_err(),
            "action_hardlink must refuse a reference target (replace-with-link modifies the reference)"
        );
        assert!(
            action_reflink(&reference_member, &keeper, std::slice::from_ref(&ref_root)).is_err(),
            "action_reflink must refuse a reference target"
        );
        // The reference's independent copy is intact (not renamed aside/relinked).
        assert!(
            reference_member.exists() && fs::read(&reference_member).unwrap() == b"master",
            "reference master copy must be untouched after the refusals"
        );
        fs::remove_dir_all(&d).ok();
    }

    // F-CLI-4 regression: the engine walks with verbatim (`\\?\`) paths
    // internally, so is_system_path must strip that prefix before
    // matching — otherwise `\\?\C:\Windows\…` slips past the guard and
    // a destructive action runs under a system-critical path without
    // --allow-system-paths. Windows-only (the prefix list is cfg-gated).
    #[cfg(windows)]
    #[test]
    fn verbatim_prefixed_system_paths_are_blocked() {
        for p in [
            r"\\?\C:\Windows\System32\sdd-test.dat",
            r"\\?\C:\Program Files\sdd-test\dup.dat",
            r"\\?\c:\programdata\sdd-test\dup.dat",
        ] {
            assert!(
                is_system_path(&PathBuf::from(p)),
                "verbatim-prefixed system path must be blocked: {p}"
            );
        }
        // Sanity: a verbatim non-system path is NOT blocked.
        assert!(!is_system_path(&PathBuf::from(r"\\?\D:\Media\song.flac")));
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

    // P0 (data-loss) regression — quality NIT: the replace_with_hardlink
    // restore arm had ZERO automated coverage, so a future "simplify the
    // Err arm" refactor could silently reintroduce the delete-on-failed-
    // link data loss. This pins the property on Linux (platform-available,
    // no cross-volume needed): a hardlink that FAILS must leave the
    // original target intact. We force the failure by pointing at a keeper
    // that doesn't exist (fs::hard_link -> ENOENT), exercising exactly the
    // rename-aside -> link-fails -> restore path.
    #[cfg(not(windows))]
    #[test]
    fn failed_hardlink_restores_the_original() {
        let d = tmpdir();
        let target = d.join("victim.bin");
        let missing_keeper = d.join("does-not-exist.bin");
        write_file(&target, b"precious");
        let res = replace_with_hardlink(&target, &missing_keeper);
        assert!(res.is_err(), "linking to a missing keeper must fail");
        assert!(
            target.exists(),
            "DATA-LOSS GUARD: a failed hardlink must restore the original target"
        );
        assert_eq!(
            fs::read(&target).unwrap(),
            b"precious",
            "restored target must have its original content"
        );
        // The move-aside tmp must not be left behind on the restore path.
        assert!(
            !target.with_extension("superdeduper.tmp").exists(),
            "restore must not leave the .tmp aside-copy"
        );
        fs::remove_dir_all(&d).ok();
    }

    // F-SAFE-2 (data-loss) regression — a reference member reached via a
    // path that differs from its canonical form (here: a symlinked parent
    // dir) must STILL be protected by the never-modify-reference guard.
    // Pre-fix the guard used a RAW contains_key against the canonical-keyed
    // reference set, so the alias missed the guard and the reference copy
    // was deleted. Linux symlink makes raw != canonical without needing
    // Windows verbatim/8.3.
    #[cfg(unix)]
    #[test]
    fn reference_member_via_symlinked_path_is_not_deleted() {
        let d = tmpdir();
        let real = d.join("real");
        fs::create_dir_all(&real).unwrap();
        let reference = real.join("master.bin");
        let dupe = real.join("copy.bin");
        write_file(&reference, b"same");
        write_file(&dupe, b"same");
        // Alias the reference through a symlinked dir so its path repr
        // differs from canonical (canonicalize resolves the symlink).
        let link = d.join("via_link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let reference_alias = link.join("master.bin");

        // Ordering is load-bearing (quality catch: the obvious ordering is
        // fake-green). pick_keeper's reference-wins loop returns the FIRST
        // reference member as keeper, and the keeper is excluded from the
        // delete loop BY INDEX — so it never reaches the line-280 guard.
        // To actually exercise the canonical-key fix, the aliased reference
        // (raw != canonical) must be the NON-keeper: put the canonical-path
        // member (`dupe`, raw == canonical) FIRST so it takes the keeper
        // slot, and the symlink-aliased reference SECOND so it flows through
        // line 280. RAW lookup there MISSES the alias -> deletes it (test
        // fails on the bug); CANONICAL lookup matches -> protected.
        // Negative-control confirmed: reverting line 280 to the raw lookup
        // makes this test FAIL (the aliased reference gets deleted).
        let mut r = results(vec![group(
            4,
            vec![dupe.clone(), reference_alias.clone()],
        )]);
        r.reference_paths = vec![dupe.clone(), reference_alias.clone()];
        let path = write_results(&d, &r);
        let args = make_args(path, false, DedupeAction::Remove);
        let outcome = run(&args).unwrap();
        assert!(
            reference.exists(),
            "DATA-LOSS GUARD: the aliased reference (non-keeper, raw != canonical) \
             must survive the line-280 guard via canonical_key"
        );
        assert!(dupe.exists(), "the keeper must also survive");
        assert_eq!(outcome.executed, 0, "no reference copy should be removed");
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

        action_recycle(&target, None, &[]).expect("action_recycle on Linux");

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

        match action_reflink(&b, &a, &[]) {
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
