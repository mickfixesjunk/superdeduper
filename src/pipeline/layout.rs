//! Stage 3 — resolve physical extents and compute the LCN sort key.
//!
//! Implementation lands in Implementation Order step 4. For now this is a
//! no-op pass-through so the end-to-end skeleton has a coherent shape.

use crate::inventory::FileEntry;
use crate::pipeline::grouping::SizeGroup;
use crate::Result;

/// File annotated with its starting LCN for HDD-friendly read ordering.
///
/// For non-NTFS or unavailable extent maps, `start_lcn` is `None` and the
/// caller should treat ordering as unspecified.
#[derive(Debug, Clone)]
pub struct LaidOutFile {
    pub entry: FileEntry,
    pub start_lcn: Option<u64>,
}

/// Annotate every file in every group with extent information. The
/// returned groups are otherwise unchanged in membership.
pub fn resolve(groups: Vec<SizeGroup>) -> Result<Vec<LaidOutGroup>> {
    Ok(groups
        .into_iter()
        .map(|g| LaidOutGroup {
            size: g.size,
            files: g
                .files
                .into_iter()
                .map(|entry| LaidOutFile {
                    entry,
                    start_lcn: None,
                })
                .collect(),
        })
        .collect())
}

#[derive(Debug)]
pub struct LaidOutGroup {
    pub size: u64,
    pub files: Vec<LaidOutFile>,
}
