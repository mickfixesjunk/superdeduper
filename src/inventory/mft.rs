//! Stage 1, fast path — direct MFT enumeration via `FSCTL_ENUM_USN_DATA`.
//!
//! Implementation lands in Implementation Order step 3. For now this is a
//! stub that always returns `Unsupported`, which causes the inventory
//! dispatcher to fall through to the directory-walk path.

use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::{Error, Result};

pub fn enumerate(_cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    Err(Error::Unsupported(
        "inventory::mft: FSCTL_ENUM_USN_DATA path not yet implemented",
    ))
}
