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
}
