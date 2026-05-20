//! Stage 5 — optional paranoid byte-by-byte verification.
//!
//! When `--paranoid` is passed, every survivor group is re-read once more
//! and compared byte-by-byte. Members that fail the comparison are split
//! out into separate groups (or dropped if they end up alone).
//!
//! Implementation arrives alongside the IOCP read pipeline (step 5+);
//! for v0 this is a pass-through.

use crate::pipeline::DuplicateGroup;
use crate::Result;

pub fn paranoid_verify(groups: Vec<DuplicateGroup>) -> Result<Vec<DuplicateGroup>> {
    // TODO(step 5+): re-read each group's files in parallel via the IOCP
    // pipeline and split groups whose members disagree.
    Ok(groups)
}
