//! Stages 2–5 of the scan pipeline.
//!
//! See the module-level docs on each submodule for what stage it owns.

pub mod confirm;
pub mod grouping;
pub mod hash;
pub mod iocp;
pub mod layout;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One file the inventory classified as a non-trivial placeholder.
/// Emitted regardless of whether the file later got blocked by the
/// tier guard (some placeholders are size-unique and never enter a
/// dup-candidate group; we still want to surface them so the user
/// understands the scan saw them).
///
/// The downstream output's `skipped[]` array contains one of these per
/// observed placeholder; `placeholder == ReparseDedup` files are
/// emitted as informational and still get hashed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    pub path: PathBuf,
    /// String form of the `PlaceholderState` for stable JSON output.
    /// `"recall_on_open"`, `"recall_on_data_access"`, `"reparse_dedup"`,
    /// `"other_reparse"`.
    pub placeholder: String,
    /// Raw reparse tag value when `placeholder == "other_reparse"`,
    /// `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reparse_tag: Option<u32>,
}

impl SkippedFile {
    /// Build from a path + `PlaceholderState`. Returns `None` for
    /// `NotPlaceholder` (not worth emitting).
    pub fn from_state(
        path: PathBuf,
        state: crate::inventory::PlaceholderState,
    ) -> Option<Self> {
        use crate::inventory::PlaceholderState as P;
        let (kind, tag) = match state {
            P::NotPlaceholder => return None,
            P::RecallOnOpen => ("recall_on_open", None),
            P::RecallOnDataAccess => ("recall_on_data_access", None),
            P::ReparseDedup => ("reparse_dedup", None),
            P::OtherReparse(t) => ("other_reparse", Some(t)),
        };
        Some(Self {
            path,
            placeholder: kind.to_string(),
            reparse_tag: tag,
        })
    }
}

/// A confirmed set of byte-identical files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    /// File size in bytes (all members share this).
    pub size: u64,
    /// Hex-encoded BLAKE3 hash of the content (Tier 3 result).
    pub content_hash: String,
    /// All paths in the group, in stable order.
    pub files: Vec<PathBuf>,
    /// `true` if this group was determined via NTFS hardlink or reflink
    /// equivalence rather than by hashing.
    pub link_equivalent: bool,
    /// Number of distinct (volume, inode) pairs among `files`.
    /// `files.len() - unique_inodes` is the count of hardlink aliases.
    /// Used by `output::summarize` to compute the "actual disk
    /// reclaimable" metric (`(unique_inodes - 1) * size`) alongside
    /// the path-aware "duplicate path bytes" metric
    /// (`(files.len() - 1) * size`). Older clients reading the JSON
    /// will see `0` if the field wasn't persisted — semantically
    /// "I don't know how many distinct inodes there are"; consumers
    /// should fall back to the path-aware metric in that case.
    #[serde(default)]
    pub unique_inodes: u64,
}
