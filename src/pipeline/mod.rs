//! Stages 2–5 of the scan pipeline.
//!
//! See the module-level docs on each submodule for what stage it owns.

pub mod confirm;
pub mod grouping;
pub mod hash;
pub mod image_hash;
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
    pub fn from_state(path: PathBuf, state: crate::inventory::PlaceholderState) -> Option<Self> {
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

/// What kind of similarity grouping produced a [`DuplicateGroup`].
///
/// `ByteIdentical` is the long-standing default (T0–T3 pipeline).
/// `PerceptualImage` is the T1.2 / #25 Tier-4 output. Audio mode
/// (#26) will add `PerceptualAudio` when it lands. Serialised as
/// the lowercase-kebab string the spec §3.3 step 5 names.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SimilarityKind {
    #[default]
    ByteIdentical,
    PerceptualImage,
}

/// A confirmed set of byte-identical files OR a Tier-4 perceptual
/// similarity group (per `similarity_kind`).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DuplicateGroup {
    /// File size in bytes. For byte-identical groups every member
    /// shares this; for perceptual groups it's the LARGEST file's
    /// size (perceptual matches aren't size-equal in general).
    pub size: u64,
    /// Group identity. Byte-identical groups store the hex-encoded
    /// BLAKE3 content hash (Tier-3 result). Perceptual groups store
    /// a synthetic `perceptual-{fingerprint:016x}` token derived
    /// from one member's dHash — opaque to the consumer; just used
    /// to de-dupe identity across reports.
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
    /// What sort of similarity grouped these files. `byte-identical`
    /// for the legacy T0–T3 pipeline; `perceptual-image` for T1.2
    /// Tier-4 output. `#[serde(default)]` keeps v2 JSON readable as
    /// v3 (older outputs land as ByteIdentical).
    #[serde(default)]
    pub similarity_kind: SimilarityKind,
}
