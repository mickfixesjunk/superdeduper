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

/// Send `path` to the Recycle Bin via `SHFileOperationW` with
/// `FOF_ALLOWUNDO`. Restorable via the standard shell APIs.
pub fn recycle(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
        FO_DELETE, SHFILEOPSTRUCTW,
    };

    // Path must be double-null-terminated.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide.push(0);

    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE as u32,
        pFrom: PCWSTR(wide.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT) as u16,
        ..Default::default()
    };
    // SAFETY: SHFileOperationW reads `op` immutably during the call,
    // including the `pFrom` pointer. `wide` outlives the call (we keep
    // it in scope until after) and is correctly double-null-terminated
    // as required by the API contract.
    let rc = unsafe { SHFileOperationW(&mut op) };
    if rc != 0 {
        return Err(Error::Other(format!(
            "SHFileOperationW returned {rc} for {}",
            path.display()
        )));
    }
    Ok(())
}

/// Atomically replace `target` with a hardlink to `keeper`. Steps:
/// 1. Rename `target` aside to a `.superdupe.tmp` neighbour.
/// 2. `CreateHardLinkW(keeper, target)`.
/// 3. On success, delete the renamed original.
/// 4. On any failure, rename the original back into place.
pub fn replace_with_hardlink(target: &Path, keeper: &Path) -> Result<()> {
    use std::fs;

    let tmp = target.with_extension("superdupe.tmp");
    if tmp.exists() {
        fs::remove_file(&tmp)?;
    }
    fs::rename(target, &tmp)?;

    match create_hard_link(target, keeper) {
        Ok(()) => {
            fs::remove_file(&tmp).ok();
            Ok(())
        }
        Err(e) => {
            // Best-effort rollback.
            fs::rename(&tmp, target).ok();
            Err(e)
        }
    }
}

fn create_hard_link(new_link: &Path, existing: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::CreateHardLinkW;

    let mut nl: Vec<u16> = new_link.as_os_str().encode_wide().collect();
    nl.push(0);
    let mut ex: Vec<u16> = existing.as_os_str().encode_wide().collect();
    ex.push(0);

    // SAFETY: both buffers are valid, null-terminated, and outlive the
    // call. lpSecurityAttributes is None which is permitted by the API.
    let ok = unsafe { CreateHardLinkW(PCWSTR(nl.as_ptr()), PCWSTR(ex.as_ptr()), None) };
    ok.map_err(|e| Error::Other(format!("CreateHardLinkW failed: {e}")))
}
