//! Directory-batched inode-id resolution.
//!
//! Win32's `GetFileInformationByHandleEx(handle, FileIdBothDirectoryInfo,
//! ...)` returns `FILE_ID_BOTH_DIR_INFO` entries for every child of a
//! directory in a single call (or a few, for large dirs). Each entry
//! carries the NTFS `FileId` (the inode id we use for hardlink
//! detection) AND the file name, with no per-file `CreateFile`
//! involved.
//!
//! This is the fast path used by `pipeline::grouping::resolve_file_ids`
//! when many files share a parent directory: one syscall per
//! directory instead of one per file. For a flat 10 000-file
//! directory the syscall count drops from 10 000 to 1.
//!
//! The per-file `CreateFile + GetFileInformationByHandle` fallback in
//! `walk::file_id_for` is kept for cases where the dir-batched path
//! fails (e.g. permission to open the dir as `FILE_LIST_DIRECTORY`
//! denied, network volumes with quirky support, exotic filesystems).

#![cfg(windows)]

use hashbrown::HashMap;
use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_NO_MORE_FILES, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, BY_HANDLE_FILE_INFORMATION,
    FILE_ATTRIBUTE_DIRECTORY, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};

/// Single-directory enumeration result.
pub struct DirInodeMap {
    /// Volume serial of the directory's volume, encoded as the
    /// per-file `volume_guid` would be ("vol-serial:0x...").
    pub volume_guid: Option<String>,
    /// Lowercase filename → 64-bit NTFS file id (the inode). Two
    /// hardlinks of the same file in this dir share the same value.
    /// We lowercase keys because NTFS is case-insensitive by default;
    /// the walker enumerates with case-preserving names so a
    /// case-mismatched lookup would otherwise miss.
    pub by_name: HashMap<OsString, u64>,
}

/// Open `dir` and enumerate its children via
/// `FileIdBothDirectoryInfo`, building a name → inode-id map. Returns
/// `None` if any step fails — caller should fall back to per-file
/// resolution.
pub fn enumerate_dir(dir: &Path) -> Option<DirInodeMap> {
    // Encode dir path as a null-terminated UTF-16 string for
    // CreateFileW. Use FILE_FLAG_BACKUP_SEMANTICS so we can open
    // directories (without it, CreateFileW refuses dir handles for
    // most operations).
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_LIST_DIRECTORY.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        )
        .ok()?
    };
    if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
        return None;
    }

    // Volume serial via GetFileInformationByHandle on the dir
    // handle. All children of this dir share this volume.
    let mut by_handle_info = BY_HANDLE_FILE_INFORMATION::default();
    let serial_ok = unsafe { GetFileInformationByHandle(handle, &mut by_handle_info).is_ok() };
    let volume_guid = if serial_ok {
        Some(format!(
            "vol-serial:0x{:08x}",
            by_handle_info.dwVolumeSerialNumber
        ))
    } else {
        None
    };

    // Buffer for FileIdBothDirectoryInfo entries. 64 KiB holds a few
    // hundred small entries (variable-length filename means the
    // count depends on names). We loop calling
    // GetFileInformationByHandleEx until ERROR_NO_MORE_FILES.
    const BUF_SIZE: usize = 64 * 1024;
    let mut buf = vec![0u8; BUF_SIZE];
    let mut by_name: HashMap<OsString, u64> = HashMap::new();
    let mut first_call = true;

    loop {
        let class = if first_call {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        let ok = unsafe {
            GetFileInformationByHandleEx(handle, class, buf.as_mut_ptr() as _, BUF_SIZE as u32)
                .is_ok()
        };
        if !ok {
            // ERROR_NO_MORE_FILES is the normal loop terminator.
            // Anything else is a real error — return what we have
            // (caller falls back per-file for missing entries).
            let err = unsafe { GetLastError() };
            if err == ERROR_NO_MORE_FILES {
                break;
            }
            // Soft fail: keep partial map and continue. The caller's
            // per-file fallback will resolve anything not in the map.
            break;
        }
        first_call = false;

        // Walk the packed FILE_ID_BOTH_DIR_INFO entries. Each entry
        // points to the next via NextEntryOffset (in bytes from this
        // entry's start). NextEntryOffset == 0 marks the last entry
        // in this batch.
        let mut offset: usize = 0;
        loop {
            if offset >= BUF_SIZE {
                break;
            }
            let entry_ptr = unsafe { buf.as_ptr().add(offset) as *const FILE_ID_BOTH_DIR_INFO };
            let entry = unsafe { &*entry_ptr };
            let name_len_chars = (entry.FileNameLength / 2) as usize;
            // FileName is declared [u16; 1] but the real array is
            // FileNameLength/2 chars long, starting at the same
            // offset as the FileName field. Construct a slice with
            // the correct length.
            let name_slice =
                unsafe { std::slice::from_raw_parts(entry.FileName.as_ptr(), name_len_chars) };
            // Skip `.` and `..` directory entries.
            let is_dot = matches!(name_slice, [46] | [46, 46]);
            // Skip child directories — only files contribute file IDs
            // for our hardlink-detection purpose.
            let is_dir = (entry.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
            if !is_dot && !is_dir {
                let name = OsString::from_wide(name_slice);
                by_name.insert(name, entry.FileId as u64);
            }
            let next = entry.NextEntryOffset as usize;
            if next == 0 {
                break;
            }
            offset += next;
        }
    }

    unsafe {
        let _ = CloseHandle(handle);
    }

    Some(DirInodeMap {
        volume_guid,
        by_name,
    })
}

// Suppress the unused-imports lint when the windows crate's enum
// helpers aren't exercised by callers in this file.
#[allow(dead_code)]
const _UNUSED_HOOK: FILE_FLAGS_AND_ATTRIBUTES = FILE_FLAG_BACKUP_SEMANTICS;
