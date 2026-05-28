//! Windows implementations of the safe wrappers.
//!
//! Every `unsafe` block in `superdeduper` lives in this file. Each
//! `unsafe` block carries a `// SAFETY:` comment naming the FFI
//! contract it relies on.

use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::{Error as WinError, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_HANDLE_EOF, ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA,
    GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED, FILE_FLAG_SEQUENTIAL_SCAN, FILE_SHARE_READ,
    FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    FSCTL_ENUM_USN_DATA, FSCTL_GET_RETRIEVAL_POINTERS, FSCTL_QUERY_USN_JOURNAL,
    FSCTL_READ_USN_JOURNAL, IOCTL_STORAGE_GET_DEVICE_NUMBER, IOCTL_STORAGE_QUERY_PROPERTY,
    MFT_ENUM_DATA_V0, READ_USN_JOURNAL_DATA_V0, RETRIEVAL_POINTERS_BUFFER,
    STARTING_VCN_INPUT_BUFFER, STORAGE_DEVICE_NUMBER, STORAGE_PROPERTY_QUERY, USN_JOURNAL_DATA_V0,
};
use windows::Win32::System::IO::DeviceIoControl;

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
    /// Raw bus type from `IOCTL_STORAGE_QUERY_PROPERTY`. 0 if the
    /// query failed. Decoded with [`bus_type_name`] for display.
    pub bus_type: u8,
    /// `Some(true|false)` if the per-device seek-penalty IOCTL gave
    /// a definitive answer; `None` if the IOCTL itself failed.
    /// Recorded so the `drive-info` subcommand can show the raw
    /// signal next to the final classification.
    pub seek_penalty_ioctl: Option<bool>,
    /// One-line human explanation of how we picked `has_seek_penalty`.
    /// Shown by `drive-info`. Stored as `String` instead of `&'static
    /// str` so the value survives a clone across thread boundaries.
    pub classification_reason: String,
}

#[derive(Debug, Clone)]
pub struct ExtentRun {
    pub vcn: u64,
    pub lcn: u64,
    pub length_clusters: u64,
}

/// A single record from `FSCTL_ENUM_USN_DATA`. The variant we parse is
/// `USN_RECORD_V2` (Windows Vista+), which covers every NTFS volume in
/// practical use. `USN_RECORD_V3` (object IDs) and `V4` (range-tracking
/// extensions for ReFS) are passed over silently.
#[derive(Debug, Clone)]
pub struct UsnRecord {
    pub file_ref: FileRef,
    pub parent_ref: FileRef,
    pub usn: i64,
    pub file_size: u64,
    pub mtime_filetime: i64,
    pub attributes: u32,
    pub name: String,
    /// `Reason` field from `USN_RECORD_V2`. Bitfield of
    /// `USN_REASON_*` flags (see ntifs.h / Win32 docs). The warm-
    /// path delta consumer in `inventory::warm` keys off
    /// `USN_REASON_FILE_DELETE` (0x200) to distinguish "file
    /// deleted" events from "file modified". Cold-path callers
    /// (FSCTL_ENUM_USN_DATA) get `0` because that FSCTL doesn't
    /// populate the field.
    pub reason: u32,
}

/// Resolve the volume that contains `path`. Returns the volume's GUID
/// path (e.g. `\\?\Volume{12345678-aaaa-bbbb-cccc-1234567890ab}\`),
/// suitable for `CreateFileW` to open the raw volume.
pub fn volume_for_path(path: &Path) -> Result<String> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);

    let mut mount_point = [0u16; 260];
    // SAFETY: `wide` is null-terminated and outlives the call. The output
    // buffer is properly sized; we pass its length in TCHARs as required.
    unsafe {
        GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut mount_point)
            .map_err(|e| Error::Other(format!("GetVolumePathNameW: {e}")))?;
    }
    let mut guid = [0u16; 64];
    // SAFETY: `mount_point` is null-terminated by Win32. Output buffer is
    // sized per Win32 docs (50 chars including NUL is typical; 64 leaves
    // headroom).
    unsafe {
        GetVolumeNameForVolumeMountPointW(PCWSTR(mount_point.as_ptr()), &mut guid)
            .map_err(|e| Error::Other(format!("GetVolumeNameForVolumeMountPointW: {e}")))?;
    }
    let len = guid.iter().position(|&c| c == 0).unwrap_or(guid.len());
    Ok(String::from_utf16_lossy(&guid[..len]))
}

/// Open the raw volume device for read access. Use the returned handle
/// with FSCTL_ENUM_USN_DATA / FSCTL_READ_USN_JOURNAL — both need
/// `GENERIC_READ` because they stream MFT bytes back to userspace.
pub fn open_volume_handle(volume_guid: &str) -> Result<OwnedHandle> {
    open_volume_handle_with_access(volume_guid, GENERIC_READ.0)
}

/// Open the volume with `desired_access = 0` — sufficient for IOCTLs
/// that only METADATA-query the device (no bytes streamed), and
/// crucially works for non-admin users. Required-access list per
/// MSDN:
///
/// * `IOCTL_STORAGE_QUERY_PROPERTY` → no access needed (0 works).
/// * `IOCTL_STORAGE_GET_DEVICE_NUMBER` → no access needed.
///
/// Using GENERIC_READ here would fail with ERROR_ACCESS_DENIED for
/// users without elevation — which is what the `drive-info`
/// subcommand was hitting when invoked from a non-elevated shell or
/// via WSL interop. The query path now succeeds for any user who
/// can see the volume.
pub fn open_volume_handle_for_query(volume_guid: &str) -> Result<OwnedHandle> {
    open_volume_handle_with_access(volume_guid, 0)
}

fn open_volume_handle_with_access(volume_guid: &str, desired_access: u32) -> Result<OwnedHandle> {
    // Win32 expects the volume path without the trailing backslash to
    // open the device, e.g. `\\?\Volume{…}` not `\\?\Volume{…}\`.
    let trimmed = volume_guid.trim_end_matches('\\');
    let mut wide: Vec<u16> = trimmed.encode_utf16().collect();
    wide.push(0);
    // SAFETY: PCWSTR points into `wide` which outlives the call. All
    // flags are valid per Win32 docs.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            desired_access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            HANDLE::default(),
        )
        .map_err(|e| Error::Other(format!("CreateFileW({trimmed}): {e}")))?
    };
    if h.is_invalid() {
        return Err(Error::Other(format!(
            "CreateFileW returned invalid handle for {trimmed}"
        )));
    }
    Ok(OwnedHandle(h))
}

/// Open a file for read with `FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED`.
/// Used by the IOCP read pipeline; caller is responsible for honouring
/// the sector-alignment contract on subsequent reads.
pub fn open_file_direct(path: &Path) -> Result<OwnedHandle> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: `wide` is null-terminated and outlives the call. Flag set
    // honours the direct-I/O requirements documented for CreateFileW.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_NO_BUFFERING | FILE_FLAG_OVERLAPPED | FILE_FLAG_SEQUENTIAL_SCAN,
            HANDLE::default(),
        )
        .map_err(|e| Error::Other(format!("CreateFileW({}): {e}", path.display())))?
    };
    Ok(OwnedHandle(h))
}

/// RAII wrapper for a Win32 HANDLE; closes on drop.
pub struct OwnedHandle(pub HANDLE);

impl OwnedHandle {
    pub fn as_handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0.is_invalid() || self.0 == INVALID_HANDLE_VALUE {
            return;
        }
        // SAFETY: We own this handle and only drop it once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Query the storage device underlying a volume — device number,
/// partition number, seek penalty (HDD vs SSD), and sector sizes.
pub fn query_storage_device(volume_guid: &str) -> Result<StorageDeviceInfo> {
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceSeekPenaltyProperty, DEVICE_SEEK_PENALTY_DESCRIPTOR,
    };

    // `_for_query` uses desired_access=0 so this works for non-admin
    // users + WSL interop. The IOCTLs below don't need GENERIC_READ.
    let handle = open_volume_handle_for_query(volume_guid)?;

    // Device + partition number.
    let mut sdn = STORAGE_DEVICE_NUMBER::default();
    let mut returned = 0u32;
    // SAFETY: `sdn` is a properly-sized output buffer for the IOCTL.
    // The handle is valid for the lifetime of `handle`.
    unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_GET_DEVICE_NUMBER,
            None,
            0,
            Some(&mut sdn as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<STORAGE_DEVICE_NUMBER>() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| Error::Other(format!("IOCTL_STORAGE_GET_DEVICE_NUMBER: {e}")))?;
    }

    // Seek penalty — first try the direct seek-penalty query, then
    // fall back to bus-type inspection (NVMe and SATA bus on a fixed
    // disk are reliable SSD signals when the seek-penalty descriptor
    // isn't implemented).
    let mut query = STORAGE_PROPERTY_QUERY::default();
    query.PropertyId = StorageDeviceSeekPenaltyProperty;
    query.QueryType = PropertyStandardQuery;
    let mut descr = DEVICE_SEEK_PENALTY_DESCRIPTOR::default();
    let mut returned2 = 0u32;
    // SAFETY: Input/output buffers correctly sized per Win32 docs for
    // this exact property query.
    let seek_penalty_result = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut descr as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<DEVICE_SEEK_PENALTY_DESCRIPTOR>() as u32,
            Some(&mut returned2),
            None,
        )
    };
    let seek_penalty_ioctl = match seek_penalty_result {
        Ok(()) => Some(descr.IncursSeekPenalty.as_bool()),
        Err(_) => None,
    };
    let bus_type_raw = query_bus_type(handle.0).unwrap_or(0);
    let (has_seek_penalty, reason) = classify_seek_penalty(bus_type_raw, seek_penalty_ioctl);
    tracing::info!(
        volume = %volume_guid,
        bus_type = bus_type_raw,
        bus_name = bus_type_name(bus_type_raw),
        seek_penalty_ioctl = ?seek_penalty_ioctl,
        classified_as = if has_seek_penalty { "HDD" } else { "SSD" },
        reason,
        "storage device type"
    );

    Ok(StorageDeviceInfo {
        device_number: sdn.DeviceNumber,
        partition_number: sdn.PartitionNumber,
        has_seek_penalty,
        sector_size: 4096,          // populated by a later commit
        physical_sector_size: 4096, // populated by a later commit
        bus_type: bus_type_raw,
        seek_penalty_ioctl,
        classification_reason: reason.to_string(),
    })
}

/// Numeric `STORAGE_BUS_TYPE` values from ntddstor.h. Kept as raw
/// constants so the classification logic + the `drive-info`
/// subcommand can decode them the same way.
pub mod bus_type {
    pub const SCSI: u8 = 0x01;
    pub const ATAPI: u8 = 0x02;
    pub const ATA: u8 = 0x03; // ATA bus — typically spinning
    pub const USB: u8 = 0x07; // external — could be either
    pub const RAID: u8 = 0x08;
    pub const ISCSI: u8 = 0x09;
    pub const SAS: u8 = 0x0A;
    pub const SATA: u8 = 0x0B; // ambiguous: both HDDs and SSDs live here
    pub const SD: u8 = 0x0C; // SSD-like
    pub const MMC: u8 = 0x0D; // SSD-like
    pub const NVME: u8 = 0x11; // always SSD
    pub const SCM: u8 = 0x12; // storage-class memory → always SSD
    pub const UFS: u8 = 0x13; // SSD-like
}

/// Decode a bus-type number into the friendly name. Used in
/// diagnostics output.
pub fn bus_type_name(b: u8) -> &'static str {
    match b {
        0x00 => "Unknown",
        bus_type::SCSI => "SCSI",
        bus_type::ATAPI => "ATAPI",
        bus_type::ATA => "ATA (spinning)",
        0x04 => "1394",
        0x05 => "SSA",
        0x06 => "Fibre",
        bus_type::USB => "USB",
        bus_type::RAID => "RAID",
        bus_type::ISCSI => "iSCSI",
        bus_type::SAS => "SAS",
        bus_type::SATA => "SATA",
        bus_type::SD => "SD",
        bus_type::MMC => "MMC/eMMC",
        0x0E => "Virtual",
        0x0F => "FileBackedVirtual",
        0x10 => "Storage Spaces",
        bus_type::NVME => "NVMe",
        bus_type::SCM => "SCM",
        bus_type::UFS => "UFS",
        _ => "?",
    }
}

/// Combine bus type and seek-penalty IOCTL into a single
/// has_seek_penalty bool plus a one-line reason string. Pulled out
/// so the rules are visible in one place and `drive-info` can
/// re-run them for explanation.
pub fn classify_seek_penalty(bus: u8, ioctl: Option<bool>) -> (bool, &'static str) {
    use bus_type::*;
    match (bus, ioctl) {
        // Buses that are SSD by construction — trust them over the
        // per-device seek-penalty IOCTL, which lies on some NVMe
        // controllers (\"yes I have a seek penalty\" while sitting
        // on an NVMe bus).
        (NVME, _) => (false, "NVMe bus → SSD"),
        (SCM, _) => (false, "SCM bus → SSD"),
        (SD, _) => (false, "SD bus → SSD"),
        (MMC, _) => (false, "MMC/eMMC bus → SSD"),
        (UFS, _) => (false, "UFS bus → SSD"),
        // ATA bus = legacy IDE = spinning by construction.
        (ATA, _) => (true, "ATA bus → HDD"),
        // Ambiguous bus (SATA, SAS, SCSI, USB, RAID, …) → defer to
        // the per-device seek-penalty IOCTL when it succeeded.
        // When it failed too, default to HDD: modern primary drives
        // have working IOCTL support, so an SATA/USB device that
        // refuses to answer is far more likely a generic external
        // HDD than a fancy SSD.
        (_, Some(v)) => (
            v,
            if v {
                "seek-penalty IOCTL → HDD"
            } else {
                "seek-penalty IOCTL → SSD"
            },
        ),
        (_, None) => (
            true,
            "ambiguous bus and seek-penalty IOCTL failed → HDD (conservative)",
        ),
    }
}

/// Raw bus type from `IOCTL_STORAGE_QUERY_PROPERTY(StorageAdapterProperty)`.
/// Returns `Err` if the IOCTL itself failed — the caller still
/// gets to decide what to do with that (the `drive-info` subcommand
/// shows the error verbatim).
pub fn query_bus_type(handle: HANDLE) -> Result<u8> {
    use windows::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageAdapterProperty, STORAGE_ADAPTER_DESCRIPTOR,
    };
    let mut query = STORAGE_PROPERTY_QUERY::default();
    query.PropertyId = StorageAdapterProperty;
    query.QueryType = PropertyStandardQuery;
    let mut descr: STORAGE_ADAPTER_DESCRIPTOR = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: input/output buffers sized to the descriptor; Win32
    // documents this as the standard query layout.
    unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some(&query as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some(&mut descr as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<STORAGE_ADAPTER_DESCRIPTOR>() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| Error::Other(format!("StorageAdapterProperty: {e}")))?;
    }
    Ok(descr.BusType)
}

/// Get the extent map of a file: `(VCN, LCN, length_in_clusters)` runs.
pub fn get_retrieval_pointers(path: &Path) -> Result<Vec<ExtentRun>> {
    let handle = open_file_for_metadata(path)?;
    let mut input = STARTING_VCN_INPUT_BUFFER::default();
    input.StartingVcn = 0;

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let mut returned = 0u32;
        // SAFETY: input buffer is properly sized; output buffer is `buf`
        // with length tracked separately; both outlive the call.
        let result = unsafe {
            DeviceIoControl(
                handle.0,
                FSCTL_GET_RETRIEVAL_POINTERS,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
                Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
                buf.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        match result {
            Ok(()) => break,
            Err(_) => {
                // SAFETY: GetLastError reads thread-local state.
                let err = unsafe { GetLastError() };
                if err == ERROR_MORE_DATA || err == ERROR_INSUFFICIENT_BUFFER {
                    buf.resize(buf.len() * 2, 0);
                    continue;
                }
                return Err(Error::RetrievalPointers {
                    path: path.to_path_buf(),
                    source: std::io::Error::from_raw_os_error(err.0 as i32),
                });
            }
        }
    }

    Ok(parse_retrieval_pointers(&buf))
}

fn parse_retrieval_pointers(buf: &[u8]) -> Vec<ExtentRun> {
    // RETRIEVAL_POINTERS_BUFFER layout:
    //   DWORD ExtentCount;
    //   LARGE_INTEGER StartingVcn;
    //   struct { LARGE_INTEGER NextVcn; LARGE_INTEGER Lcn; } Extents[ExtentCount];
    if buf.len() < std::mem::size_of::<RETRIEVAL_POINTERS_BUFFER>() {
        return Vec::new();
    }
    // SAFETY: The struct fields we read are all POD; we only deref through
    // a copy and never trust the trailing Extents[1] array as bounded by
    // its declared length.
    let header = unsafe { &*(buf.as_ptr() as *const RETRIEVAL_POINTERS_BUFFER) };
    let count = header.ExtentCount as usize;
    let mut prev_vcn = header.StartingVcn as u64;

    // Each extent is two LARGE_INTEGERs (16 bytes); the first extent
    // starts at offset 16 (DWORD + LARGE_INTEGER aligned to 8 = 16).
    let extents_off = 16;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let base = extents_off + i * 16;
        if base + 16 > buf.len() {
            break;
        }
        let next_vcn = i64::from_le_bytes(buf[base..base + 8].try_into().unwrap());
        let lcn = i64::from_le_bytes(buf[base + 8..base + 16].try_into().unwrap());
        let length = (next_vcn as u64).saturating_sub(prev_vcn);
        out.push(ExtentRun {
            vcn: prev_vcn,
            lcn: lcn as u64,
            length_clusters: length,
        });
        prev_vcn = next_vcn as u64;
    }
    out
}

fn open_file_for_metadata(path: &Path) -> Result<OwnedHandle> {
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    // SAFETY: wide is null-terminated and outlives the call. We need
    // FILE_FLAG_BACKUP_SEMANTICS to be able to open directories too if
    // ever asked.
    let h = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0, // metadata-only access
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            HANDLE::default(),
        )
        .map_err(|e| Error::Other(format!("CreateFileW({}): {e}", path.display())))?
    };
    Ok(OwnedHandle(h))
}

/// Iterator-style enumerator for `FSCTL_ENUM_USN_DATA`. Each `next()`
/// returns one batch of [`UsnRecord`]s, in MFT order. When the volume
/// has been fully enumerated, `next()` returns `Ok(None)`.
pub struct UsnEnum {
    handle: OwnedHandle,
    next_id: u64,
    buf: Vec<u8>,
}

impl UsnEnum {
    pub fn open(volume_guid: &str) -> Result<Self> {
        let handle = open_volume_handle(volume_guid)?;
        Ok(Self {
            handle,
            next_id: 0,
            buf: vec![0u8; 256 * 1024],
        })
    }

    pub fn next_batch(&mut self) -> Result<Option<Vec<UsnRecord>>> {
        let mut input = MFT_ENUM_DATA_V0::default();
        input.StartFileReferenceNumber = self.next_id;
        input.LowUsn = 0;
        input.HighUsn = i64::MAX;

        let mut returned = 0u32;
        // SAFETY: input buffer is properly sized POD; output buffer is
        // `self.buf` with length tracked separately. Handle is valid.
        let result = unsafe {
            DeviceIoControl(
                self.handle.0,
                FSCTL_ENUM_USN_DATA,
                Some(&input as *const _ as *const std::ffi::c_void),
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                Some(self.buf.as_mut_ptr() as *mut std::ffi::c_void),
                self.buf.len() as u32,
                Some(&mut returned),
                None,
            )
        };
        match result {
            Ok(()) => {}
            Err(_) => {
                // SAFETY: GetLastError is thread-local.
                let err = unsafe { GetLastError() };
                if err == ERROR_HANDLE_EOF {
                    return Ok(None);
                }
                return Err(Error::MftEnum {
                    volume: "?".into(),
                    source: std::io::Error::from_raw_os_error(err.0 as i32),
                });
            }
        }
        if returned < 8 {
            return Ok(None);
        }
        // First 8 bytes = next FileReferenceNumber to use.
        self.next_id = u64::from_le_bytes(self.buf[0..8].try_into().unwrap());
        let records = parse_usn_records(&self.buf[8..returned as usize]);
        if records.is_empty() {
            return Ok(None);
        }
        Ok(Some(records))
    }
}

fn parse_usn_records(buf: &[u8]) -> Vec<UsnRecord> {
    // USN_RECORD_V2 layout (Vista+):
    //   DWORD RecordLength;
    //   WORD  MajorVersion;
    //   WORD  MinorVersion;
    //   DWORDLONG FileReferenceNumber;
    //   DWORDLONG ParentFileReferenceNumber;
    //   USN   Usn;
    //   LARGE_INTEGER TimeStamp;
    //   DWORD Reason;
    //   DWORD SourceInfo;
    //   DWORD SecurityId;
    //   DWORD FileAttributes;
    //   WORD  FileNameLength;
    //   WORD  FileNameOffset;
    //   WCHAR FileName[1];
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 60 <= buf.len() {
        let record_len = u32::from_le_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
        if record_len < 60 || pos + record_len > buf.len() {
            break;
        }
        let major = u16::from_le_bytes(buf[pos + 4..pos + 6].try_into().unwrap());
        if major == 2 {
            let file_ref = u64::from_le_bytes(buf[pos + 8..pos + 16].try_into().unwrap());
            let parent_ref = u64::from_le_bytes(buf[pos + 16..pos + 24].try_into().unwrap());
            let usn = i64::from_le_bytes(buf[pos + 24..pos + 32].try_into().unwrap());
            let timestamp = i64::from_le_bytes(buf[pos + 32..pos + 40].try_into().unwrap());
            let reason = u32::from_le_bytes(buf[pos + 40..pos + 44].try_into().unwrap());
            let attributes = u32::from_le_bytes(buf[pos + 52..pos + 56].try_into().unwrap());
            let name_len = u16::from_le_bytes(buf[pos + 56..pos + 58].try_into().unwrap()) as usize;
            let name_off = u16::from_le_bytes(buf[pos + 58..pos + 60].try_into().unwrap()) as usize;
            if name_off + name_len <= record_len && (name_off + name_len) % 2 == 0 {
                let name_bytes = &buf[pos + name_off..pos + name_off + name_len];
                let name_u16: Vec<u16> = name_bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                let name = String::from_utf16_lossy(&name_u16);
                out.push(UsnRecord {
                    file_ref,
                    parent_ref,
                    usn,
                    file_size: 0,
                    mtime_filetime: timestamp,
                    attributes,
                    name,
                    reason,
                });
            }
        }
        pos += record_len;
    }
    out
}

/// Current state of the USN journal on a volume — what
/// `FSCTL_QUERY_USN_JOURNAL` returns. The warm-path inventory
/// validates a saved snapshot by checking:
/// 1. `journal_id` matches what we stored last time, AND
/// 2. our stored cursor is `>= first_usn` (i.e. still in range)
/// If either fails, the journal was deleted/recreated/wrapped and
/// we have to fall back to a full MFT walk.
#[derive(Debug, Clone, Copy)]
pub struct UsnJournalState {
    pub journal_id: i64,
    /// Oldest USN still in the journal. Anything older than this is
    /// gone; the journal has rolled over those entries.
    pub first_usn: i64,
    /// USN that will be assigned to the next change. Use this as
    /// the new cursor after a successful delta read.
    pub next_usn: i64,
}

/// Query journal metadata via `FSCTL_QUERY_USN_JOURNAL`. Required
/// before reading a delta so we can detect journal wrap-around.
pub fn query_usn_journal_state(volume_guid: &str) -> Result<UsnJournalState> {
    let handle = open_volume_handle(volume_guid)?;
    let mut buf: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut returned = 0u32;
    // SAFETY: output buffer is correctly sized for V0 query layout.
    // No input buffer is required for this FSCTL.
    unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_QUERY_USN_JOURNAL,
            None,
            0,
            Some(&mut buf as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| Error::UsnJournal {
            volume: volume_guid.into(),
            source: std::io::Error::other(format!("{e}")),
        })?;
    }
    Ok(UsnJournalState {
        journal_id: buf.UsnJournalID as i64,
        first_usn: buf.FirstUsn,
        next_usn: buf.NextUsn,
    })
}

/// Read the USN journal forward from `since_usn` and yield change
/// records. `journal_id` MUST be the current journal's identifier
/// — pass the value the caller validated against
/// `FSCTL_QUERY_USN_JOURNAL`. (MSDN documents `0` as
/// "bypass identifier verification", but real-world Windows
/// builds reject the call with ERROR_INVALID_PARAMETER if the
/// field is zeroed, so we always pass the live ID.) Returns the
/// new "next USN" so the caller can persist it.
pub fn read_usn_journal_delta(
    volume_guid: &str,
    since_usn: i64,
    journal_id: i64,
) -> Result<(Vec<UsnRecord>, i64)> {
    let handle = open_volume_handle(volume_guid)?;
    let mut input = READ_USN_JOURNAL_DATA_V0::default();
    input.StartUsn = since_usn;
    input.ReasonMask = u32::MAX;
    input.ReturnOnlyOnClose = 0;
    input.Timeout = 0;
    input.BytesToWaitFor = 0;
    input.UsnJournalID = journal_id as u64;

    let mut buf = vec![0u8; 256 * 1024];
    let mut returned = 0u32;
    // SAFETY: input is POD; output buffer length tracked correctly.
    unsafe {
        DeviceIoControl(
            handle.0,
            FSCTL_READ_USN_JOURNAL,
            Some(&input as *const _ as *const std::ffi::c_void),
            std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as u32,
            Some(buf.as_mut_ptr() as *mut std::ffi::c_void),
            buf.len() as u32,
            Some(&mut returned),
            None,
        )
        .map_err(|e| Error::UsnJournal {
            volume: volume_guid.into(),
            source: std::io::Error::other(format!("{e}")),
        })?;
    }
    if returned < 8 {
        return Ok((Vec::new(), since_usn));
    }
    let next_usn = i64::from_le_bytes(buf[0..8].try_into().unwrap());
    let records = parse_usn_records(&buf[8..returned as usize]);
    Ok((records, next_usn))
}

/// Convert a raw UTF-16 buffer terminated by NUL into a `PathBuf`.
#[allow(dead_code)]
pub(crate) fn pathbuf_from_wide(wide: &[u16]) -> PathBuf {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    PathBuf::from(std::ffi::OsString::from_wide(&wide[..len]))
}

/// Fetch the raw reparse-point tag for `path` via `FSCTL_GET_REPARSE_POINT`.
/// Returns `None` if the file doesn't carry a reparse point, can't be opened,
/// or the ioctl fails.
///
/// Why this exists: phase-2 classify() takes both `attrs` and an
/// `Option<u32>` `reparse_tag`, but all three FileEntry producers (mft,
/// warm, walker) currently pass `None` for the tag because USN data
/// doesn't include it. That makes classify() return `OtherReparse(0)`
/// for *every* reparse-tagged file, even IO_REPARSE_TAG_DEDUP (which
/// should be `ReparseDedup`) and the cloud-provider tags (which should
/// be `RecallOnOpen` / `RecallOnDataAccess` once classify supports
/// tag-first cloud detection).
///
/// This helper closes the gap. Walker calls it when
/// `attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0`, passes the result to
/// classify(). Cost: one CreateFile + one DeviceIoControl + one
/// CloseHandle per reparse file — typically a handful per scan, so
/// the cost is negligible against per-file hashing.
///
/// `FILE_FLAG_OPEN_REPARSE_POINT` opens the link itself, not the
/// target — critical, otherwise symlinks would resolve and we'd never
/// see the link's own reparse data. `FILE_FLAG_BACKUP_SEMANTICS`
/// lets us open directories too (some reparse points are dir
/// junctions). Read-only is sufficient.
pub fn fetch_reparse_tag(path: &Path) -> Option<u32> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MAXIMUM_REPARSE_DATA_BUFFER_SIZE,
        OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::FSCTL_GET_REPARSE_POINT;
    use windows::Win32::System::IO::DeviceIoControl;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            FILE_GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .ok()?
    };
    if handle.is_invalid() || handle == INVALID_HANDLE_VALUE {
        return None;
    }

    // REPARSE_DATA_BUFFER (and REPARSE_GUID_DATA_BUFFER) both start with
    // a `ULONG ReparseTag` at offset 0. We only need the first 4 bytes,
    // but DeviceIoControl can fail with ERROR_INSUFFICIENT_BUFFER if
    // the output buffer is smaller than the full reparse payload. Size
    // to MAXIMUM_REPARSE_DATA_BUFFER_SIZE (16 KB) so any in-spec reparse
    // tag fits.
    let mut buf = vec![0u8; MAXIMUM_REPARSE_DATA_BUFFER_SIZE as usize];
    let mut bytes_returned: u32 = 0;
    let result = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_GET_REPARSE_POINT,
            None,
            0,
            Some(buf.as_mut_ptr() as *mut _),
            buf.len() as u32,
            Some(&mut bytes_returned),
            None,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if result.is_err() || bytes_returned < 4 {
        return None;
    }
    Some(u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]))
}

/// Drop the leading `\\?\` verbatim-path prefix, if present. The walker
/// (src/inventory/walk.rs::to_verbatim) adds this prefix to every
/// emitted path so legacy Win32 APIs accept trailing-dot / reserved-DOS
/// names / >MAX_PATH lengths. Shell-namespace APIs such as
/// `SHCreateItemFromParsingName` REJECT the prefix with E_INVALIDARG —
/// callers about to hand a path to one of those APIs need to strip it
/// first. Modern shell APIs handle long paths natively, so dropping the
/// prefix doesn't re-introduce legacy MAX_PATH truncation.
///
/// Unchanged when no prefix is present.
fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let wide_first: Vec<u16> = path.as_os_str().encode_wide().take(4).collect();
    // \\?\ == [0x5C, 0x5C, 0x3F, 0x5C]
    if wide_first.as_slice() == [0x5C, 0x5C, 0x3F, 0x5C] {
        let rest: Vec<u16> = path.as_os_str().encode_wide().skip(4).collect();
        PathBuf::from(std::ffi::OsString::from_wide(&rest))
    } else {
        path.to_path_buf()
    }
}

/// Send `path` to the Recycle Bin via the modern `IFileOperation` COM
/// interface. Restorable via the standard shell APIs.
///
/// T2.3 — replaced the legacy `SHFileOperationW` path with
/// `IFileOperation` for: (a) native long-path support without `\\?\`
/// wrapping, (b) HRESULT-grade error reporting (instead of opaque
/// integer rc), (c) future-compat — Microsoft has soft-deprecated
/// SHFileOperationW since Vista. The user-visible behavior is
/// unchanged: silent recycle, no UI, no confirmation, no error
/// dialog.
pub fn recycle(path: &Path) -> Result<()> {
    use windows::core::HRESULT;
    use windows::Win32::Foundation::S_FALSE;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        FileOperation, IFileOperation, IShellItem, SHCreateItemFromParsingName,
        FILEOPERATION_FLAGS, FOFX_RECYCLEONDELETE, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
    };

    // Build a null-terminated UTF-16 path. SHCreateItemFromParsingName
    // (the modern shell-namespace entry point) REJECTS the `\\?\`
    // verbatim prefix with E_INVALIDARG (0x80070057) — confirmed by
    // benchmarker on the T2.3 recycle smoke test. The walker's verbatim
    // prefix (added in T0.4 for `FindFirstFileW` long-path / trailing-
    // dot correctness) flows through every FileEntry.path and reaches
    // here unchanged. Strip it before the shell call. Modern shell APIs
    // do their own canonicalization and support long paths natively,
    // so dropping the prefix doesn't re-introduce the legacy
    // truncation issue.
    let stripped = strip_verbatim_prefix(path);
    let wide: Vec<u16> = stripped
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // COM init dance: try to initialize the apartment for this thread.
    // S_OK or S_FALSE both mean "init OK and we own a refcount to
    // release"; any other HRESULT (notably RPC_E_CHANGED_MODE if some
    // other code already initialised this thread MTA) means we did NOT
    // take a refcount and must not Uninitialize. Calls beyond the COM
    // boundary are unsafe regardless of the result.
    unsafe {
        let init_hr = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let need_uninit = init_hr == HRESULT(0) || init_hr == S_FALSE;

        let result = (|| -> Result<()> {
            // CoCreateInstance returns the COM object as the requested
            // interface; turbofish makes the type binding explicit.
            let op: IFileOperation =
                CoCreateInstance(&FileOperation, None, CLSCTX_INPROC_SERVER)
                    .map_err(|e| Error::Other(format!("CoCreateInstance(IFileOperation): {e}")))?;

            // Flag combination matches the SHFileOperationW call we
            // replaced: SILENT (no progress UI) + NOCONFIRMATION (no
            // prompt) + NOERRORUI (no dialog). FOFX_RECYCLEONDELETE
            // forces recycle-bin semantics even when the shell would
            // otherwise hard-delete (e.g. files above the Recycle Bin
            // size cap, or network shares). This is the IFileOperation
            // equivalent of the FOF_ALLOWUNDO flag.
            let flags_u32 = (FOF_SILENT.0 | FOF_NOCONFIRMATION.0 | FOF_NOERRORUI.0) as u32
                | FOFX_RECYCLEONDELETE.0 as u32;
            op.SetOperationFlags(FILEOPERATION_FLAGS(flags_u32))
                .map_err(|e| Error::Other(format!("SetOperationFlags: {e}")))?;

            let shell_item: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)
                .map_err(|e| {
                Error::Other(format!(
                    "SHCreateItemFromParsingName({}): {e}",
                    path.display()
                ))
            })?;

            op.DeleteItem(&shell_item, None)
                .map_err(|e| Error::Other(format!("DeleteItem({}): {e}", path.display())))?;

            op.PerformOperations()
                .map_err(|e| Error::Other(format!("PerformOperations({}): {e}", path.display())))?;

            Ok(())
        })();

        if need_uninit {
            CoUninitialize();
        }
        result
    }
}

/// Replace `target` with a ReFS block-clone of `keeper` using
/// `FSCTL_DUPLICATE_EXTENTS_TO_FILE`. Atomic-ish via the same
/// rename-aside / restore-on-failure dance as the hardlink action.
pub fn replace_with_reflink(target: &Path, keeper: &Path) -> Result<()> {
    use std::fs;
    let tmp_dest = target.with_extension("superdeduper.reflink.tmp");
    let tmp_orig = target.with_extension("superdeduper.tmp");
    if tmp_dest.exists() {
        fs::remove_file(&tmp_dest)?;
    }
    if tmp_orig.exists() {
        fs::remove_file(&tmp_orig)?;
    }
    let size = fs::metadata(keeper)?.len();

    // Stash the original aside so we can roll back on failure.
    fs::rename(target, &tmp_orig)?;

    let result = (|| -> Result<()> {
        block_clone(&tmp_dest, keeper, size)?;
        fs::rename(&tmp_dest, target)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            fs::remove_file(&tmp_orig).ok();
            Ok(())
        }
        Err(e) => {
            fs::rename(&tmp_orig, target).ok();
            fs::remove_file(&tmp_dest).ok();
            Err(e)
        }
    }
}

/// Create `dest` as a block-clone of `source` for the given size.
/// Issues `FSCTL_DUPLICATE_EXTENTS_TO_FILE` in cluster-aligned chunks.
fn block_clone(dest: &Path, source: &Path, size: u64) -> Result<()> {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::System::Ioctl::{DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE};

    // Cluster size is volume-dependent; ReFS standard is 4 KiB, may be
    // 64 KiB. Use the larger value so requests are guaranteed-aligned.
    const CHUNK: u64 = 64 * 1024;

    let source_file = OpenOptions::new()
        .read(true)
        .share_mode(/* FILE_SHARE_READ */ 1 | /* FILE_SHARE_WRITE */ 2)
        .open(source)?;
    let dest_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(1 | 2)
        .open(dest)?;
    dest_file.set_len(size)?;

    let source_handle = HANDLE(source_file.as_raw_handle());
    let dest_handle = HANDLE(dest_file.as_raw_handle());

    let mut offset = 0u64;
    while offset < size {
        let chunk = CHUNK.min(size - offset);
        // Round up to cluster boundary (FSCTL requires it).
        let aligned_chunk = (chunk + 4095) & !4095;
        let chunk_to_clone = if offset + aligned_chunk > size {
            size - offset
        } else {
            aligned_chunk
        };
        let mut request = DUPLICATE_EXTENTS_DATA {
            FileHandle: source_handle,
            SourceFileOffset: offset as i64,
            TargetFileOffset: offset as i64,
            ByteCount: chunk_to_clone as i64,
        };
        let mut returned = 0u32;
        // SAFETY: `request` is POD and outlives the call; the source
        // handle is held alive by `source_file`; dest handle by
        // `dest_file`. Buffer sizes are correctly computed via
        // size_of_val on a stack value.
        unsafe {
            DeviceIoControl(
                dest_handle,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                Some(&request as *const _ as *const std::ffi::c_void),
                std::mem::size_of_val(&request) as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
            .map_err(|e| Error::Other(format!("FSCTL_DUPLICATE_EXTENTS_TO_FILE: {e}")))?;
        }
        let _ = &mut request;
        offset += chunk_to_clone;
    }
    Ok(())
}

/// Atomically replace `target` with a hardlink to `keeper`.
pub fn replace_with_hardlink(target: &Path, keeper: &Path) -> Result<()> {
    use std::fs;
    let tmp = target.with_extension("superdeduper.tmp");
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
            fs::rename(&tmp, target).ok();
            Err(e)
        }
    }
}

fn create_hard_link(new_link: &Path, existing: &Path) -> Result<()> {
    use windows::Win32::Storage::FileSystem::CreateHardLinkW;
    let mut nl: Vec<u16> = new_link.as_os_str().encode_wide().collect();
    nl.push(0);
    let mut ex: Vec<u16> = existing.as_os_str().encode_wide().collect();
    ex.push(0);
    // SAFETY: both buffers are null-terminated and outlive the call.
    let r: std::result::Result<(), WinError> =
        unsafe { CreateHardLinkW(PCWSTR(nl.as_ptr()), PCWSTR(ex.as_ptr()), None) };
    match r {
        Ok(()) => Ok(()),
        // CRITICAL (data-loss guard): do NOT swallow the failure. The
        // caller (`replace_with_hardlink`) renames the original aside to
        // `.tmp` and deletes it ONLY on Ok — so reporting a false success
        // here makes it delete the original when no link was created.
        // Surface the Win32 code as an io::Error (HRESULT low word) so the
        // dedupe layer can classify ERROR_NOT_SAME_DEVICE (17) as a
        // cross-volume refusal; the caller's Err-arm restores the original.
        Err(e) => Err(Error::Io(std::io::Error::from_raw_os_error(
            (e.code().0 & 0xFFFF) as i32,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ensure the USN_RECORD_V2 parser handles a hand-crafted record.
    #[test]
    fn parse_usn_record_basic() {
        let name = "hello.txt".encode_utf16().collect::<Vec<u16>>();
        let name_bytes: Vec<u8> = name.iter().flat_map(|c| c.to_le_bytes()).collect();
        let name_off = 60u16;
        let name_len = name_bytes.len() as u16;
        // RecordLength must be a multiple of 8.
        let unpadded = 60 + name_bytes.len();
        let record_len = (unpadded + 7) & !7;
        let mut buf = vec![0u8; record_len];
        buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
        buf[4..6].copy_from_slice(&2u16.to_le_bytes()); // major
        buf[6..8].copy_from_slice(&0u16.to_le_bytes()); // minor
        buf[8..16].copy_from_slice(&0x1234_5678_9abc_def0u64.to_le_bytes()); // file_ref
        buf[16..24].copy_from_slice(&0x1u64.to_le_bytes()); // parent_ref
        buf[24..32].copy_from_slice(&100i64.to_le_bytes()); // usn
        buf[32..40].copy_from_slice(&123456789i64.to_le_bytes()); // ts
        buf[56..58].copy_from_slice(&name_len.to_le_bytes());
        buf[58..60].copy_from_slice(&name_off.to_le_bytes());
        buf[60..60 + name_bytes.len()].copy_from_slice(&name_bytes);

        let parsed = parse_usn_records(&buf);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].file_ref, 0x1234_5678_9abc_def0);
        assert_eq!(parsed[0].parent_ref, 1);
        assert_eq!(parsed[0].usn, 100);
        assert_eq!(parsed[0].name, "hello.txt");
    }

    /// Parser stops cleanly at a truncated record.
    #[test]
    fn parse_usn_record_truncated_safe() {
        let buf = vec![0u8; 32]; // too short to be a record
        let parsed = parse_usn_records(&buf);
        assert!(parsed.is_empty());
    }

    /// Parser handles back-to-back records.
    #[test]
    fn parse_usn_record_multiple() {
        fn build_one(file_ref: u64, name: &str) -> Vec<u8> {
            let n: Vec<u8> = name.encode_utf16().flat_map(|c| c.to_le_bytes()).collect();
            let unpadded = 60 + n.len();
            let record_len = (unpadded + 7) & !7;
            let mut buf = vec![0u8; record_len];
            buf[0..4].copy_from_slice(&(record_len as u32).to_le_bytes());
            buf[4..6].copy_from_slice(&2u16.to_le_bytes());
            buf[8..16].copy_from_slice(&file_ref.to_le_bytes());
            buf[56..58].copy_from_slice(&(n.len() as u16).to_le_bytes());
            buf[58..60].copy_from_slice(&60u16.to_le_bytes());
            buf[60..60 + n.len()].copy_from_slice(&n);
            buf
        }
        let mut buf = Vec::new();
        buf.extend(build_one(10, "one"));
        buf.extend(build_one(20, "two"));
        buf.extend(build_one(30, "three"));
        let parsed = parse_usn_records(&buf);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].name, "one");
        assert_eq!(parsed[1].name, "two");
        assert_eq!(parsed[2].name, "three");
    }

    /// T2.3 fix regression guard: SHCreateItemFromParsingName rejects
    /// `\\?\` verbatim prefixes with E_INVALIDARG, so recycle() strips
    /// them before the COM call. Lock that semantic in a test so a
    /// future helper refactor doesn't quietly reintroduce the bug.
    #[test]
    fn strip_verbatim_prefix_drops_extended_path_marker() {
        // With prefix → dropped.
        assert_eq!(
            super::strip_verbatim_prefix(std::path::Path::new(r"\\?\C:\Windows\System32")),
            std::path::PathBuf::from(r"C:\Windows\System32"),
        );
        // Without prefix → unchanged.
        assert_eq!(
            super::strip_verbatim_prefix(std::path::Path::new(r"C:\Windows\System32")),
            std::path::PathBuf::from(r"C:\Windows\System32"),
        );
        // UNC path without the verbatim form → unchanged (we only
        // strip the literal `\\?\` marker, not UNC roots).
        assert_eq!(
            super::strip_verbatim_prefix(std::path::Path::new(r"\\server\share\file")),
            std::path::PathBuf::from(r"\\server\share\file"),
        );
        // Verbatim UNC `\\?\UNC\server\share` — we drop the `\\?\` only,
        // leaving `UNC\server\share`. SHCreateItemFromParsingName doesn't
        // accept that form either, but it's a known edge case; callers
        // dealing with verbatim-UNC paths need their own conversion.
        assert_eq!(
            super::strip_verbatim_prefix(std::path::Path::new(r"\\?\UNC\server\share")),
            std::path::PathBuf::from(r"UNC\server\share"),
        );
    }
}
