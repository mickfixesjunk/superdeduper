//! Windows implementations of the safe wrappers. Currently a thin set of
//! type definitions; the actual FFI is implemented in Implementation Order
//! step 2. Tests live next to each wrapper as it lands.

use std::path::Path;

use crate::{Error, Result};

/// A 64-bit NTFS file reference number (record + sequence).
pub type FileRef = u64;

/// 64-bit volume-relative byte offset for the first cluster of a file.
pub type StartLcn = u64;

#[derive(Debug, Clone)]
pub struct StorageDeviceInfo {
    pub device_number: u32,
    pub partition_number: u32,
    pub has_seek_penalty: bool,
    pub sector_size: u32,
    pub physical_sector_size: u32,
}

#[derive(Debug, Clone)]
pub struct ExtentRun {
    pub vcn: u64,
    pub lcn: u64,
    pub length_clusters: u64,
}

pub fn volume_for_path(_p: &Path) -> Result<String> {
    Err(Error::Unsupported(
        "winapi_wrappers::volume_for_path: not yet implemented",
    ))
}

pub fn query_storage_device(_volume: &str) -> Result<StorageDeviceInfo> {
    Err(Error::Unsupported(
        "winapi_wrappers::query_storage_device: not yet implemented",
    ))
}

pub fn get_retrieval_pointers(_path: &Path) -> Result<Vec<ExtentRun>> {
    Err(Error::Unsupported(
        "winapi_wrappers::get_retrieval_pointers: not yet implemented",
    ))
}
