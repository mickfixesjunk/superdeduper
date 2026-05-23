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
