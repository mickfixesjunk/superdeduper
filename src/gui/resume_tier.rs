//! #99 PR2 — Unified resume-tier classification.
//!
//! Pre-#99 the resume path used a binary gate: `roots_match &&
//! settings_match` either accepted the entire checkpoint or threw
//! everything away. That model conflated four distinct drift axes
//! (settings, filesystem, engine-version, UI threading) into one
//! YES/NO answer, which drove #39 ("sometimes from scratch after
//! inventory"), #52 ("USN-advance partial restart"), and #64 ("Resume
//! click hangs"). #99 unifies them.
//!
//! [`ResumeTier`] names the five user-visible outcomes; [`classify_resume_tier`]
//! is the pure function the resume orchestrator + the launch-time
//! modal both consume. Pure-function shape means the drift-matrix
//! test (`tests/scan_resume_drift_matrix.rs`) can exercise every
//! cell of the (roots × settings × cache × inventory) matrix
//! without spinning a real engine.

use crate::gui::checkpoint::Checkpoint;
use crate::gui::state::{RootEntry, ScanSettings};
use std::path::{Path, PathBuf};

/// What the engine + GUI agree the next scan should do, given a
/// loaded checkpoint and the current launch-time context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeTier {
    /// Roots match (canonical), settings match, schema hasn't bumped.
    /// Restore everything; cache hits during the resumed scan are
    /// free. The default happy path.
    Full,
    /// Roots match, settings match, BUT the cache was wiped (engine
    /// version bumped → SCHEMA_VERSION mismatch → cache.rs:202-212
    /// purge ran). Inventory + previous_duplicates restore; cache
    /// lookups will all miss; user sees honest "re-validating from
    /// cold cache" status.
    Warm,
    /// Roots match, settings differ. The walker output (saved_inventory)
    /// is still reusable — same file list — but the analysis pipeline
    /// must redo grouping/hashing under the new settings. User sees
    /// "Settings changed — keeping the file list, redoing analysis."
    InventoryOnly,
    /// Checkpoint exists but holds neither inventory nor any prior
    /// duplicates. The walker was paused mid-Stage-1 (or just before).
    /// #61 case: modal still surfaces the resume choice but the
    /// walker starts fresh. Roots + settings still required to
    /// match — Marker is for the "honest checkpoint that just
    /// happens to be empty" case, not for cross-config resume.
    Marker,
    /// Nothing usable. Either no checkpoint, or roots don't match
    /// (no shared inventory to reuse), or some structural reason to
    /// reject. Walker starts fresh.
    Fresh,
}

impl ResumeTier {
    /// One-line user-friendly description rendered in the
    /// launch-time resume modal. Plain-direct voice per
    /// [[release-notes-voice]] — no marketing softening, no false
    /// confidence.
    pub fn modal_label(self) -> &'static str {
        match self {
            ResumeTier::Full => "Resume previous scan",
            ResumeTier::Warm => "Resume — engine updated, re-validating from cold cache",
            ResumeTier::InventoryOnly => "Settings changed — keeping the file list, redoing analysis",
            ResumeTier::Marker => "Resume previous scan (was paused before any duplicates found)",
            ResumeTier::Fresh => "Start fresh — no resumable state",
        }
    }

    /// Whether the resumed scan will reuse the prior duplicate list.
    /// `Full` and `Warm` both restore; the others don't.
    pub fn restores_previous_duplicates(self) -> bool {
        matches!(self, ResumeTier::Full | ResumeTier::Warm)
    }

    /// Whether the resumed scan will reuse the prior walker output
    /// (saved_inventory). `Full`, `Warm`, and `InventoryOnly` all
    /// reuse the file list; `Marker` and `Fresh` start the walker
    /// from scratch.
    pub fn reuses_saved_inventory(self) -> bool {
        matches!(
            self,
            ResumeTier::Full | ResumeTier::Warm | ResumeTier::InventoryOnly
        )
    }
}

/// Launch-time inputs for [`classify_resume_tier`]. Pure data;
/// observable at engine startup without any extra walker pass.
#[derive(Debug, Clone)]
pub struct SessionContext {
    /// Current scan roots (user-facing path inputs from
    /// PersistedAppState.roots).
    pub roots: Vec<RootEntry>,
    /// Current scan settings (user-facing config from
    /// PersistedAppState.settings).
    pub settings: ScanSettings,
    /// `true` when the cache's stored SCHEMA_VERSION doesn't match
    /// the running binary's. Observable at startup via
    /// `cache.rs:202-212` (the cache wipe runs unconditionally on
    /// schema mismatch; this flag records that the wipe happened,
    /// which signals the resume run will have a cold cache
    /// regardless of FS drift state).
    pub schema_version_mismatch: bool,
}

/// Classify what should happen on Resume given a loaded
/// [`Checkpoint`] + current launch context.
///
/// Pure function: no I/O, no side effects, deterministic for given
/// inputs. The drift-matrix test (`tests/scan_resume_drift_matrix.rs`)
/// covers every cell of the input cross-product.
///
/// USN drift is NOT a classify input — it's a per-file outcome at
/// `cache.rs:324` lookup time. The Warm tier semantic already
/// accommodates "may or may not have FS drift; re-validate per-file
/// at hash time." Pre-computing USN drift would require an extra
/// walker pass before classify; this design avoids that.
pub fn classify_resume_tier(prior: &Checkpoint, current: &SessionContext) -> ResumeTier {
    classify_from_parts(
        &prior.roots,
        &prior.settings,
        prior.previous_duplicates.is_empty(),
        prior.saved_inventory.is_some(),
        current,
    )
}

/// #99 PR2 — Summary-based classify, for the launch-time modal
/// that only has [`crate::gui::checkpoint::CheckpointSummary`] (not
/// the full multi-MB Checkpoint). Same semantics as
/// [`classify_resume_tier`]; same input cross-product covered by
/// the drift-matrix test.
pub fn classify_resume_tier_from_summary(
    summary: &crate::gui::checkpoint::CheckpointSummary,
    current: &SessionContext,
) -> ResumeTier {
    classify_from_parts(
        &summary.roots,
        &summary.settings,
        summary.duplicate_count == 0,
        summary.has_saved_inventory,
        current,
    )
}

/// Shared core. Both wrappers project their input onto these
/// constituent fields.
fn classify_from_parts(
    prior_roots: &[RootEntry],
    prior_settings: &ScanSettings,
    prior_duplicates_empty: bool,
    prior_inventory_present: bool,
    current: &SessionContext,
) -> ResumeTier {
    // (1) Roots mismatch → can't reuse anything (no shared
    // inventory, no shared duplicates) → Fresh.
    if !roots_match_canonical(prior_roots, &current.roots) {
        return ResumeTier::Fresh;
    }
    // (2) No inventory AND no prior duplicates → Marker (#61 case).
    //   Marker takes precedence over settings drift: a pre-inventory
    //   marker has nothing to reuse OR re-analyse, so settings
    //   drift is moot. Per drift-matrix A6: marker + settings_drift
    //   still returns Marker.
    if prior_duplicates_empty && !prior_inventory_present {
        return ResumeTier::Marker;
    }
    // (3) Roots match, settings differ → reuse the walker output if
    //   any → InventoryOnly. Without inventory there's nothing to
    //   reuse → Fresh (this only fires when duplicates exist but no
    //   inventory — narrow edge case for older checkpoint formats).
    if prior_settings != &current.settings {
        if prior_inventory_present {
            return ResumeTier::InventoryOnly;
        }
        return ResumeTier::Fresh;
    }
    // (4) Roots + settings both match.
    //   - SCHEMA bumped → Warm (cold cache, restore inventory +
    //     dups, expect cache misses during hash).
    //   - Otherwise → Full.
    if current.schema_version_mismatch {
        return ResumeTier::Warm;
    }
    ResumeTier::Full
}

/// False-positive guard helper (A14 in testdesign's drift-matrix
/// spec). Compares two `RootEntry` slices treating Windows path
/// case + trailing slash as equivalent.
///
/// Pre-#99 the binary filter used raw `Vec<RootEntry>` equality,
/// which is byte-equality on the `path` field. That fails when:
/// - Windows user types `C:\Users\Mick\` vs `c:\users\mick`
///   (same physical folder, byte-different paths)
/// - Trailing slash variation
///
/// Catching A14 prevents the worst-class regression: silent
/// checkpoint loss when the user re-enters their own scan root
/// in a case the OS treats as equivalent but the engine doesn't.
pub fn roots_match_canonical(a: &[RootEntry], b: &[RootEntry]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut a_norm: Vec<NormalizedRoot> = a.iter().map(NormalizedRoot::from_entry).collect();
    let mut b_norm: Vec<NormalizedRoot> = b.iter().map(NormalizedRoot::from_entry).collect();
    a_norm.sort();
    b_norm.sort();
    a_norm == b_norm
}

/// Sort-compare-able normalized form of a `RootEntry` for canonical
/// roots comparison. Path is lowercased on Windows + trailing
/// separators stripped on all platforms. Other RootEntry fields
/// (e.g., is_reference) compared verbatim — if user toggled
/// "treat as reference" between sessions, that IS a settings
/// drift signal worth catching at A14's guard layer.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct NormalizedRoot {
    path: PathBuf,
    is_reference: bool,
}

impl NormalizedRoot {
    fn from_entry(r: &RootEntry) -> Self {
        Self {
            path: canonical_path_for_compare(&r.path),
            is_reference: r.is_reference,
        }
    }
}

fn canonical_path_for_compare(p: &Path) -> PathBuf {
    let s: String = p.to_string_lossy().into_owned();
    let trimmed = s.trim_end_matches(['/', '\\']).to_string();
    #[cfg(windows)]
    let trimmed = trimmed.to_lowercase();
    PathBuf::from(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::checkpoint::Checkpoint;
    use crate::gui::events::DuplicateGroupSummary;
    use crate::gui::state::{RootEntry, ScanSettings};
    use std::path::PathBuf;

    fn root(path: &str) -> RootEntry {
        RootEntry {
            path: PathBuf::from(path),
            is_reference: false,
        }
    }

    fn ctx_with(roots: Vec<RootEntry>, settings: ScanSettings, schema_drift: bool) -> SessionContext {
        SessionContext {
            roots,
            settings,
            schema_version_mismatch: schema_drift,
        }
    }

    fn checkpoint_with(
        roots: Vec<RootEntry>,
        settings: ScanSettings,
        dups: Vec<DuplicateGroupSummary>,
        with_inventory: bool,
    ) -> Checkpoint {
        let mut cp = Checkpoint::new(roots, settings);
        cp.previous_duplicates = dups;
        if with_inventory {
            cp.saved_inventory = Some(Vec::new());
        }
        cp
    }

    fn synthetic_dup() -> DuplicateGroupSummary {
        DuplicateGroupSummary {
            size: 1024,
            content_hash: "abc123".into(),
            files: vec![PathBuf::from("/a"), PathBuf::from("/b")],
            link_equivalent: false,
            unique_inodes: 2,
            similarity_kind: crate::pipeline::SimilarityKind::ByteIdentical,
        }
    }

    #[test]
    fn full_when_everything_matches() {
        let roots = vec![root("/scan")];
        let settings = ScanSettings::default();
        let cp = checkpoint_with(roots.clone(), settings.clone(), vec![synthetic_dup()], true);
        let ctx = ctx_with(roots, settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Full);
    }

    #[test]
    fn warm_when_schema_bumped() {
        let roots = vec![root("/scan")];
        let settings = ScanSettings::default();
        let cp = checkpoint_with(roots.clone(), settings.clone(), vec![synthetic_dup()], true);
        let ctx = ctx_with(roots, settings, true);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Warm);
    }

    #[test]
    fn inventory_only_when_settings_differ_but_inventory_present() {
        let roots = vec![root("/scan")];
        let mut prior_settings = ScanSettings::default();
        let mut current_settings = ScanSettings::default();
        prior_settings.paranoid = false;
        current_settings.paranoid = true;
        let cp = checkpoint_with(roots.clone(), prior_settings, vec![synthetic_dup()], true);
        let ctx = ctx_with(roots, current_settings, false);
        assert_eq!(
            classify_resume_tier(&cp, &ctx),
            ResumeTier::InventoryOnly
        );
    }

    #[test]
    fn fresh_when_settings_differ_and_no_inventory() {
        let roots = vec![root("/scan")];
        let mut prior_settings = ScanSettings::default();
        let mut current_settings = ScanSettings::default();
        prior_settings.paranoid = false;
        current_settings.paranoid = true;
        let cp = checkpoint_with(roots.clone(), prior_settings, vec![synthetic_dup()], false);
        let ctx = ctx_with(roots, current_settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Fresh);
    }

    #[test]
    fn marker_when_no_inventory_and_no_dups() {
        let roots = vec![root("/scan")];
        let settings = ScanSettings::default();
        let cp = checkpoint_with(roots.clone(), settings.clone(), vec![], false);
        let ctx = ctx_with(roots, settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Marker);
    }

    #[test]
    fn a6_marker_with_settings_drift_still_marker() {
        // drift-matrix A6: marker has nothing to apply settings to,
        // so settings drift doesn't downgrade to Fresh / InventoryOnly.
        let roots = vec![root("/scan")];
        let mut prior_settings = ScanSettings::default();
        let mut current_settings = ScanSettings::default();
        prior_settings.paranoid = false;
        current_settings.paranoid = true;
        let cp = checkpoint_with(roots.clone(), prior_settings, vec![], false);
        let ctx = ctx_with(roots, current_settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Marker);
    }

    #[test]
    fn fresh_when_roots_differ() {
        let prior_roots = vec![root("/old")];
        let current_roots = vec![root("/new")];
        let settings = ScanSettings::default();
        let cp = checkpoint_with(prior_roots, settings.clone(), vec![synthetic_dup()], true);
        let ctx = ctx_with(current_roots, settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Fresh);
    }

    #[test]
    fn a13_settings_rewrite_without_change_still_full() {
        // A13: config save re-emits same values; classify must
        // still see them as equal. Relies on ScanSettings: PartialEq
        // comparing value-equality not identity.
        let roots = vec![root("/scan")];
        let settings_a = ScanSettings::default();
        let settings_b = ScanSettings::default(); // distinct instance, identical values
        let cp = checkpoint_with(roots.clone(), settings_a, vec![synthetic_dup()], true);
        let ctx = ctx_with(roots, settings_b, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Full);
    }

    #[test]
    #[cfg(windows)]
    fn a14_windows_case_variation_still_full() {
        let prior_roots = vec![root(r"C:\Users\Mick")];
        let current_roots = vec![root(r"c:\users\mick")];
        let settings = ScanSettings::default();
        let cp = checkpoint_with(prior_roots, settings.clone(), vec![synthetic_dup()], true);
        let ctx = ctx_with(current_roots, settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Full);
    }

    #[test]
    fn a14_trailing_slash_variation_still_full() {
        let prior_roots = vec![root("/scan/")];
        let current_roots = vec![root("/scan")];
        let settings = ScanSettings::default();
        let cp = checkpoint_with(prior_roots, settings.clone(), vec![synthetic_dup()], true);
        let ctx = ctx_with(current_roots, settings, false);
        assert_eq!(classify_resume_tier(&cp, &ctx), ResumeTier::Full);
    }

    #[test]
    fn restores_previous_duplicates_only_for_full_and_warm() {
        assert!(ResumeTier::Full.restores_previous_duplicates());
        assert!(ResumeTier::Warm.restores_previous_duplicates());
        assert!(!ResumeTier::InventoryOnly.restores_previous_duplicates());
        assert!(!ResumeTier::Marker.restores_previous_duplicates());
        assert!(!ResumeTier::Fresh.restores_previous_duplicates());
    }

    #[test]
    fn reuses_saved_inventory_for_full_warm_and_inventory_only() {
        assert!(ResumeTier::Full.reuses_saved_inventory());
        assert!(ResumeTier::Warm.reuses_saved_inventory());
        assert!(ResumeTier::InventoryOnly.reuses_saved_inventory());
        assert!(!ResumeTier::Marker.reuses_saved_inventory());
        assert!(!ResumeTier::Fresh.reuses_saved_inventory());
    }
}
