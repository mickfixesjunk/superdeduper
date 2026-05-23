//! Cloud-files placeholder + reparse-point classification.
//!
//! On Windows, files can carry NTFS reparse points that change the
//! semantics of `ReadFile`. Cloud-storage providers (OneDrive Files
//! On-Demand, iCloud, SharePoint) use `FILE_ATTRIBUTE_RECALL_ON_OPEN`
//! or `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` to mark files that are
//! stubbed locally — reading them triggers a cloud-download
//! ("hydration") that costs bandwidth and disk.
//!
//! Naive dedupers `ReadFile` these stubs and force-hydrate them
//! during a scan. superdeduper detects the placeholder state during
//! enumeration and skips content reads entirely; the file is
//! reported on the event stream with its `PlaceholderState` so
//! consumers know what was skipped and why.
//!
//! On non-Windows platforms, every file is `NotPlaceholder` by
//! definition — the FS API doesn't surface the recall bits.

use serde::{Deserialize, Serialize};

/// What kind of placeholder/reparse semantics a file has.
///
/// Files with `NotPlaceholder` are normal candidates for hashing.
/// Anything else gets skipped by the walker (or, for `ReparseDedup`,
/// hashed but flagged informationally — the file's data IS readable,
/// it's just stored by NTFS dedup).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlaceholderState {
    /// Default: a regular file. Hashing proceeds as normal.
    #[default]
    NotPlaceholder,
    /// `FILE_ATTRIBUTE_RECALL_ON_OPEN` — opening the file at all
    /// triggers cloud hydration. Skip content reads.
    RecallOnOpen,
    /// `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` — partially-hydrated
    /// placeholder; reading data past what's locally cached triggers
    /// cloud hydration. Skip content reads.
    RecallOnDataAccess,
    /// `IO_REPARSE_TAG_DEDUP` — Windows Server data deduplication.
    /// The file is readable normally, but the underlying data is
    /// shared by the filesystem. Hashing is safe; flag informational
    /// so consumers know any "duplicate" we'd report is already
    /// FS-level deduplicated.
    ReparseDedup,
    /// Some other reparse-point tag we don't have specific handling
    /// for. Recorded so future analysis can add cases. The u32 is
    /// the raw tag value.
    OtherReparse(u32),
}

impl PlaceholderState {
    /// Whether this state should block content reads in the hash
    /// tiers. The "block content read" path is what protects against
    /// force-hydration.
    ///
    /// `ReparseDedup` does NOT block reads — the data is local (NTFS
    /// dedup is transparent at the FS API layer) and hashing it is
    /// safe and informational.
    ///
    /// `OtherReparse` DOES block reads. By the time `classify()`
    /// returns this variant, symlinks and junctions are already
    /// filtered upstream via `--follow-links`, so what's left is
    /// genuinely-unknown tags: HSM (tape retrieval), PrjFS (Git VFS
    /// materialization), unrecognized cloud providers — all
    /// hydration-class. Safer-by-default. Known-safe tags can be
    /// promoted to specific variants later if real-world cases
    /// justify it.
    pub fn blocks_content_read(self) -> bool {
        matches!(
            self,
            Self::RecallOnOpen | Self::RecallOnDataAccess | Self::OtherReparse(_)
        )
    }

    /// Whether this state should appear as a skip on the walker's
    /// event stream. `NotPlaceholder` doesn't get an event; all
    /// other states do (including dedup, for informational purposes).
    pub fn emits_event(self) -> bool {
        !matches!(self, Self::NotPlaceholder)
    }

    /// Whether destructive actions (recycle, hardlink-replace,
    /// archive-move, safe-rename) should be refused for files in
    /// this state. Defense in depth: even if the file made it past
    /// the walker into a duplicate group somehow, the action layer
    /// declines to touch it.
    pub fn blocks_destructive_action(self) -> bool {
        !matches!(self, Self::NotPlaceholder)
    }

    /// Policy-aware variant of [`blocks_content_read`]. Returns true
    /// when the tier guard should refuse a content read; when
    /// `allow_recall_on_read` is true, the user has explicitly opted
    /// into accepting cloud-hydration for placeholders, so
    /// `RecallOnOpen` / `RecallOnDataAccess` pass through.
    /// `OtherReparse` stays blocked under all policies — the safe
    /// default for unknown reparses is "don't touch."
    pub fn blocks_content_read_under_policy(self, allow_recall_on_read: bool) -> bool {
        match self {
            Self::NotPlaceholder | Self::ReparseDedup => false,
            Self::RecallOnOpen | Self::RecallOnDataAccess => !allow_recall_on_read,
            Self::OtherReparse(_) => true,
        }
    }

    /// Policy-aware variant of [`blocks_destructive_action`]. When
    /// `allow_destructive_on_deduped` is true, `ReparseDedup` files
    /// become eligible targets — NTFS dedup is transparent at the API
    /// layer, so destructive actions don't risk data loss (though
    /// reclaimable bytes will be ~0 since the FS already shares the
    /// extents). All recall/unknown-reparse states stay blocked.
    pub fn blocks_destructive_action_under_policy(
        self,
        allow_destructive_on_deduped: bool,
    ) -> bool {
        match self {
            Self::NotPlaceholder => false,
            Self::ReparseDedup => !allow_destructive_on_deduped,
            Self::RecallOnOpen | Self::RecallOnDataAccess | Self::OtherReparse(_) => true,
        }
    }
}

/// Inspect Win32 file attributes + reparse tag and classify.
///
/// `attrs` is the `FILE_ATTRIBUTE_*` bitmask (e.g. from
/// `FILE_ID_BOTH_DIR_INFO::FileAttributes`).
/// `reparse_tag` is the tag value, only meaningful when
/// `FILE_ATTRIBUTE_REPARSE_POINT` is set in `attrs`; pass `None`
/// otherwise.
///
/// Stable across all Windows versions that support cloud Files
/// On-Demand (Windows 8.1+). The recall bits are constants in
/// `winnt.h`; we reproduce them here to avoid pulling a `windows`
/// crate dependency into a function called from cross-platform
/// code paths.
#[cfg(windows)]
pub fn classify(attrs: u32, reparse_tag: Option<u32>) -> PlaceholderState {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_ATTRIBUTE_RECALL_ON_OPEN: u32 = 0x0004_0000;
    const FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    // From ntifs.h. Values are stable and architecture-agnostic.
    const IO_REPARSE_TAG_DEDUP: u32 = 0x8000_0013;
    // Cloud-files reparse tags live in the 0x901..0x903 range
    // (NAME_SURROGATE bits + cloud-provider-specific tail); we
    // match on the recall attributes rather than the tag value
    // because providers use different tag values within the family.

    // Recall bits take precedence — if a file is a cloud placeholder
    // AND a reparse point, we treat it as a cloud placeholder.
    if attrs & FILE_ATTRIBUTE_RECALL_ON_OPEN != 0 {
        return PlaceholderState::RecallOnOpen;
    }
    if attrs & FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS != 0 {
        return PlaceholderState::RecallOnDataAccess;
    }
    if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        if let Some(tag) = reparse_tag {
            if tag == IO_REPARSE_TAG_DEDUP {
                return PlaceholderState::ReparseDedup;
            }
            return PlaceholderState::OtherReparse(tag);
        }
        // Reparse point with no tag info — treat as Other(0).
        return PlaceholderState::OtherReparse(0);
    }
    PlaceholderState::NotPlaceholder
}

#[cfg(not(windows))]
pub fn classify(_attrs: u32, _reparse_tag: Option<u32>) -> PlaceholderState {
    // Non-Windows: no FILE_ATTRIBUTE_RECALL_* concept; everything is
    // a normal file. The test corpus's cross-platform-non-regression
    // test (#11) asserts no placeholder events get emitted from the
    // Linux walker.
    PlaceholderState::NotPlaceholder
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_not_placeholder() {
        assert_eq!(PlaceholderState::default(), PlaceholderState::NotPlaceholder);
    }

    #[test]
    fn blocks_content_read_for_recall_and_other_reparse() {
        assert!(!PlaceholderState::NotPlaceholder.blocks_content_read());
        assert!(PlaceholderState::RecallOnOpen.blocks_content_read());
        assert!(PlaceholderState::RecallOnDataAccess.blocks_content_read());
        // Dedup stays hashable — data is local.
        assert!(!PlaceholderState::ReparseDedup.blocks_content_read());
        // OtherReparse blocks reads (HSM / PrjFS / unknown cloud).
        assert!(PlaceholderState::OtherReparse(0x1234).blocks_content_read());
    }

    #[test]
    fn destructive_blocked_for_anything_non_normal() {
        assert!(!PlaceholderState::NotPlaceholder.blocks_destructive_action());
        assert!(PlaceholderState::RecallOnOpen.blocks_destructive_action());
        assert!(PlaceholderState::RecallOnDataAccess.blocks_destructive_action());
        assert!(PlaceholderState::ReparseDedup.blocks_destructive_action());
        assert!(PlaceholderState::OtherReparse(0x1234).blocks_destructive_action());
    }

    #[test]
    fn content_read_policy_unblocks_recall_when_opted_in() {
        // Without opt-in, behaviour matches the default.
        assert!(!PlaceholderState::NotPlaceholder.blocks_content_read_under_policy(false));
        assert!(PlaceholderState::RecallOnOpen.blocks_content_read_under_policy(false));
        assert!(PlaceholderState::OtherReparse(0).blocks_content_read_under_policy(false));

        // With opt-in, recall-class states pass; others unchanged.
        assert!(!PlaceholderState::RecallOnOpen.blocks_content_read_under_policy(true));
        assert!(!PlaceholderState::RecallOnDataAccess.blocks_content_read_under_policy(true));
        // Unknown reparses still blocked under any policy — too risky.
        assert!(PlaceholderState::OtherReparse(0xDEAD).blocks_content_read_under_policy(true));
        // ReparseDedup is always allowed regardless of policy.
        assert!(!PlaceholderState::ReparseDedup.blocks_content_read_under_policy(false));
        assert!(!PlaceholderState::ReparseDedup.blocks_content_read_under_policy(true));
    }

    #[test]
    fn destructive_policy_unblocks_deduped_when_opted_in() {
        // Default (allow_destructive_on_deduped = false): same as old method.
        assert!(!PlaceholderState::NotPlaceholder.blocks_destructive_action_under_policy(false));
        assert!(PlaceholderState::ReparseDedup.blocks_destructive_action_under_policy(false));
        assert!(PlaceholderState::RecallOnOpen.blocks_destructive_action_under_policy(false));

        // Opted in: only ReparseDedup unblocks.
        assert!(!PlaceholderState::ReparseDedup.blocks_destructive_action_under_policy(true));
        // Recall/unknown stay blocked even with the dedup flag — those
        // are about cloud safety, not FS-dedup transparency.
        assert!(PlaceholderState::RecallOnOpen.blocks_destructive_action_under_policy(true));
        assert!(PlaceholderState::RecallOnDataAccess.blocks_destructive_action_under_policy(true));
        assert!(PlaceholderState::OtherReparse(0xC0DE).blocks_destructive_action_under_policy(true));
    }

    #[cfg(windows)]
    #[test]
    fn classify_recall_on_open() {
        let attrs = 0x0004_0000; // FILE_ATTRIBUTE_RECALL_ON_OPEN
        assert_eq!(classify(attrs, None), PlaceholderState::RecallOnOpen);
    }

    #[cfg(windows)]
    #[test]
    fn classify_recall_on_data_access() {
        let attrs = 0x0040_0000; // FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS
        assert_eq!(classify(attrs, None), PlaceholderState::RecallOnDataAccess);
    }

    #[cfg(windows)]
    #[test]
    fn classify_dedup_reparse() {
        let attrs = 0x0000_0400; // FILE_ATTRIBUTE_REPARSE_POINT
        assert_eq!(
            classify(attrs, Some(0x8000_0013)),
            PlaceholderState::ReparseDedup
        );
    }

    #[cfg(windows)]
    #[test]
    fn classify_other_reparse_captures_tag() {
        let attrs = 0x0000_0400; // FILE_ATTRIBUTE_REPARSE_POINT
        let unknown = 0x12345678;
        assert_eq!(
            classify(attrs, Some(unknown)),
            PlaceholderState::OtherReparse(unknown)
        );
    }

    #[cfg(windows)]
    #[test]
    fn classify_recall_takes_precedence_over_reparse() {
        // File is both a reparse point AND a cloud placeholder.
        // Cloud-recall semantics matter more for our skip decision.
        let attrs = 0x0004_0000 | 0x0000_0400;
        assert_eq!(
            classify(attrs, Some(0x8000_0013)),
            PlaceholderState::RecallOnOpen
        );
    }
}
