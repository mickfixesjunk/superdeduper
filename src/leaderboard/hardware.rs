//! Hardware-detect surface per the live backend schema at
//! `https://api.superdeduper.io/api/v1/submit/schema.json`.
//!
//! Field shape is dictated by the backend's Zod schema; we MUST emit
//! exactly the keys it lists in `hardware.required` (and no extras —
//! `additionalProperties: false`). Fields we can't reliably detect
//! today fall back to documented defaults rather than failing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "telemetry", derive(schemars::JsonSchema))]
pub struct HardwareFingerprint {
    /// Vendor + model as reported by the OS, e.g.
    /// `"AMD Ryzen 9 9950X3D 16-Core Processor"`.
    pub cpu_model_string: String,
    /// Physical cores. Distinct from `cpu_threads` on
    /// hyperthreaded CPUs (cores < threads).
    pub cpu_cores: u32,
    /// `std::thread::available_parallelism()` — total logical
    /// threads visible to the scheduler.
    pub cpu_threads: u32,
    /// CPUID ISA flag names, lowercase + dash-or-digit-only per
    /// the backend pattern `^[a-z0-9-]{1,20}$`. Sorted.
    pub cpu_isa_flags: Vec<String>,
    /// Snapped enum: `"4" | "8" | "16" | "32" | "64" | "128" |
    /// "256" | "512" | "1024"`. Round-down to the nearest enum
    /// value (so 30 GB → "16"). Defaults to "16" if detection
    /// fails (sensible mid-range guess).
    pub ram_total_gb_bucket: String,
    /// OS-specific version label, e.g. `"Windows 11 25H2"`,
    /// `"macOS 14.5"`, `"Ubuntu 22.04"`. Free-form string.
    pub os_version: String,
    /// Enum: `"Home" | "Pro" | "Enterprise" | "Education" |
    /// "Server" | "Other"`. Parsed out of the full Windows
    /// edition string when possible; "Other" on macOS / Linux.
    pub os_edition: String,
    /// Enum: `"NVMe-Gen5" | "NVMe-Gen4" | "NVMe-Gen3" |
    /// "SATA-SSD" | "HDD" | "network" | "mixed"`. Defaults to
    /// "mixed" — proper detection is a future enhancement.
    pub disk_class: String,
    /// Enum: `"NTFS" | "ReFS" | "exFAT" | "FAT32" |
    /// "network-SMB" | "other"`. Defaults to "NTFS" on Windows,
    /// "other" elsewhere.
    pub filesystem: String,
    /// NTFS cluster size in KB. Defaults to 4 (the NTFS default
    /// for volumes ≤ 16 TB).
    pub cluster_size_kb: u32,
    /// Snapped enum power-of-two from "1" to "32768" (GB).
    /// Defaults to "1024" (1 TB — common modern drive) when
    /// volume probe isn't wired.
    pub volume_size_gb_bucket: String,
    /// #130 Pathfinder Phase 3 — `true` when ANY local volume on
    /// this machine is a Windows Dev Drive (ReFS + developer-
    /// workload flag set, Windows 11 24H2+). Defaults to false
    /// on non-Windows and when no Dev Drive is detected.
    /// `#[serde(default)]` keeps old engine submissions readable
    /// (web sees them as false).
    #[serde(default)]
    pub is_dev_drive: bool,
}

pub fn detect() -> HardwareFingerprint {
    detect_with_root_hint(None)
}

/// #88 Phase 1 — detect() with optional scan-root path used as a
/// hint for filesystem-class detection. When the caller knows which
/// path they're about to scan (the GUI submit flow, the CLI submit
/// flow, etc.), passing the first root here unlocks the
/// pathfinder-refs / network-pioneer signal classes by letting the
/// engine probe that path's filesystem instead of returning the
/// default "NTFS"-on-Windows / "other"-elsewhere placeholder.
///
/// Settings-modal previews + tests that want a hardware fingerprint
/// without a scan-root context call [`detect`] directly; their
/// `filesystem` field stays the platform default.
pub fn detect_with_root_hint(root_hint: Option<&std::path::Path>) -> HardwareFingerprint {
    let cpu_threads = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    // Without a robust per-platform physical-core probe, approximate
    // as half threads (most modern x86 CPUs have 2-way SMT). Clamp to
    // at least 1.
    let cpu_cores = (cpu_threads / 2).max(1);
    let ram_gb_raw = detect_ram_gb().unwrap_or(16);
    let filesystem = root_hint
        .and_then(detect_filesystem)
        .unwrap_or_else(|| default_filesystem().to_string());
    HardwareFingerprint {
        cpu_model_string: normalize_cpu_brand(&detect_cpu_model()),
        cpu_cores,
        cpu_threads,
        cpu_isa_flags: detect_isa_flags(),
        ram_total_gb_bucket: snap_ram_bucket(ram_gb_raw),
        os_version: detect_os_version(),
        os_edition: detect_os_edition_enum(),
        disk_class: detect_disk_class(),
        filesystem,
        cluster_size_kb: 4,
        volume_size_gb_bucket: "1024".to_string(),
        is_dev_drive: detect_is_dev_drive(root_hint),
    }
}

// ============================================================
// is_dev_drive — #130 Pathfinder Phase 3. Detects whether ANY
// local volume is a Windows Dev Drive (ReFS + developer-workload
// flag, Windows 11 24H2+). Detection via FSCTL_QUERY_PERSISTENT_
// VOLUME_STATE on the system volume — returns the volume's flags
// including PERSISTENT_VOLUME_STATE_DEV_VOLUME (0x2000).
//
// Per-volume schema would be richer but HardwareFingerprint is
// machine-wide; a single boolean answers "does this machine have
// a Dev Drive at all?" which is what the
// `pathfinder-dev-drive` web-side achievement consumes.
// ============================================================

fn detect_is_dev_drive(root_hint: Option<&std::path::Path>) -> bool {
    platform_detect_is_dev_drive(root_hint).unwrap_or(false)
}

#[cfg(target_os = "windows")]
fn platform_detect_is_dev_drive(root_hint: Option<&std::path::Path>) -> Option<bool> {
    // F-CLI-5 — probe the volume being SCANNED, not the boot volume.
    // A Dev Drive is typically a separate trusted ReFS sidecar volume
    // (e.g. F:), not C:; the prior system-volume-only probe read
    // DEV_VOLUME=0 because it asked C: (which genuinely isn't a Dev
    // Drive). Derive the device path from the scanned root's drive
    // letter; fall back to the system volume when there's no root hint
    // (e.g. a hardware-only detect with no scan in flight).
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, INVALID_HANDLE_VALUE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{
        FILE_FS_PERSISTENT_VOLUME_INFORMATION, FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
        PERSISTENT_VOLUME_STATE_DEV_VOLUME,
    };
    use windows::Win32::System::IO::DeviceIoControl;

    // `\\.\<letter>:` VOLUME-DEVICE path — the device namespace, NOT the
    // `\\?\` file/long-path namespace. The persistent-volume-state
    // FSCTL needs a volume-device handle; a `\\?\F:` open yields a
    // dir/file-style handle and the FSCTL fails (ERROR_INVALID_PARAMETER
    // 87 on F:, INVALID_USER_BUFFER 1784 on C:) — which the old code
    // swallowed to is_dev_drive=false, masking it on C: but breaking it
    // on a real Dev Drive. Mirrors open_physical_drive_zero's
    // \\.\PhysicalDrive0 + FILE_ATTRIBUTE_NORMAL device-open pattern.
    let device_path = root_hint
        .and_then(volume_device_path_for)
        .or_else(system_volume_device_path)?;
    let path: Vec<u16> = device_path
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            // F-CLI-5 — GENERIC_READ, not 0: the persistent-volume-state
            // FSCTL needs read access on the volume-device handle.
            // access=0 opens but the FSCTL then fails ERROR_INVALID_FUNCTION
            // (1) — sdd-testwin verified DEV_VOLUME=true with GENERIC_READ.
            GENERIC_READ.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .ok()?;
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // Input: ask for the DEV_VOLUME flag (Version 1).
    let mut input = FILE_FS_PERSISTENT_VOLUME_INFORMATION {
        Version: 1,
        FlagMask: PERSISTENT_VOLUME_STATE_DEV_VOLUME,
        ..Default::default()
    };
    let mut output = FILE_FS_PERSISTENT_VOLUME_INFORMATION::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_QUERY_PERSISTENT_VOLUME_STATE,
            Some(&mut input as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<FILE_FS_PERSISTENT_VOLUME_INFORMATION>() as u32,
            Some(&mut output as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<FILE_FS_PERSISTENT_VOLUME_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    };
    unsafe {
        let _ = CloseHandle(handle);
    }
    ok.ok()?;
    Some((output.VolumeFlags & PERSISTENT_VOLUME_STATE_DEV_VOLUME) != 0)
}

/// Resolve the Windows system volume's device path —
/// `\\.\<letter>:` form. Reads the system directory via
/// `GetSystemDirectoryW`, extracts the first character (drive
/// letter), and assembles the device-path form CreateFileW + the
/// FSCTL_QUERY_PERSISTENT_VOLUME_STATE wire expect.
///
/// Returns `None` if the system directory is shorter than 2 chars
/// (impossible on any real Windows install) or doesn't start
/// with a drive letter (e.g. a UNC system root — vanishingly
/// rare; not worth the extra parsing here).
#[cfg(target_os = "windows")]
fn system_volume_device_path() -> Option<String> {
    use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
    let mut buf = [0u16; 260]; // MAX_PATH
    let written = unsafe { GetSystemDirectoryW(Some(&mut buf)) };
    if written == 0 {
        return None;
    }
    let sysdir = String::from_utf16_lossy(&buf[..written as usize]);
    let mut chars = sysdir.chars();
    let letter = chars.next()?;
    if chars.next() != Some(':') {
        return None;
    }
    if !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(format!("\\\\.\\{}:", letter.to_ascii_uppercase()))
}

/// F-CLI-5 — derive the `\\.\<letter>:` volume device path for an
/// arbitrary (scanned) path, so is_dev_drive probes the volume the
/// user actually scanned rather than the boot volume. Normalizes the
/// Windows verbatim prefix first (S15) so a `\\?\F:\…` root reads the
/// real drive letter. Returns `None` for non-drive-letter roots (UNC
/// shares etc.) — those aren't Dev Drives.
#[cfg(target_os = "windows")]
fn volume_device_path_for(path: &std::path::Path) -> Option<String> {
    let s = crate::path_display::for_user_display(path);
    let mut chars = s.chars();
    let letter = chars.next()?;
    if chars.next() != Some(':') || !letter.is_ascii_alphabetic() {
        return None;
    }
    Some(format!("\\\\.\\{}:", letter.to_ascii_uppercase()))
}

#[cfg(not(target_os = "windows"))]
fn platform_detect_is_dev_drive(_root_hint: Option<&std::path::Path>) -> Option<bool> {
    // Dev Drive is a Windows-only feature; always false elsewhere.
    Some(false)
}

// ============================================================
// Disk class — #129 Pathfinder Phase 2. Discriminates the
// schema enum `"NVMe-Gen5" | "NVMe-Gen4" | "NVMe-Gen3" |
// "SATA-SSD" | "HDD" | "network" | "mixed"` so web's
// pathfinder achievement evaluator can fire `pathfinder-nvme-gen5`
// and related signals.
// ============================================================

/// Detect the disk class for the *primary* boot/data device.
///
/// A real multi-disk machine may host both NVMe + SATA + HDD;
/// schema-wise we still need a single label, so we pick the
/// fastest detected class (NVMe-Gen5 > Gen4 > Gen3 > SATA-SSD >
/// HDD). Falls back to "mixed" when nothing can be detected.
fn detect_disk_class() -> String {
    if let Some(class) = platform_detect_disk_class() {
        return class;
    }
    "mixed".to_string()
}

#[cfg(target_os = "linux")]
fn platform_detect_disk_class() -> Option<String> {
    // Walk /sys/class/nvme/. Each child is an NVMe controller
    // (nvme0, nvme1, …). Its `device/current_link_speed` reports
    // the PCIe link speed in GT/s — e.g. "32.0 GT/s PCIe" for
    // Gen5, "16.0 GT/s PCIe" for Gen4, "8.0 GT/s PCIe" for Gen3.
    // Pick the FASTEST detected controller so a Gen5 NVMe + a
    // sidecar Gen3 still reports "NVMe-Gen5".
    let mut best_gen: Option<u8> = None;
    if let Ok(entries) = std::fs::read_dir("/sys/class/nvme") {
        for entry in entries.flatten() {
            let speed_path = entry.path().join("device").join("current_link_speed");
            if let Ok(raw) = std::fs::read_to_string(&speed_path) {
                let trimmed = raw.trim();
                // Parse the "X.Y GT/s" prefix.
                let gen = trimmed
                    .split_whitespace()
                    .next()
                    .and_then(|tok| tok.parse::<f64>().ok())
                    .and_then(pcie_gts_to_gen);
                if let Some(g) = gen {
                    best_gen = Some(best_gen.map_or(g, |prev| prev.max(g)));
                }
            }
        }
    }
    if let Some(g) = best_gen {
        return Some(format!("NVMe-Gen{g}"));
    }
    // No NVMe found. Check for rotational vs SSD via
    // /sys/block/*/queue/rotational (0 = SSD, 1 = HDD).
    let mut any_ssd = false;
    let mut any_hdd = false;
    if let Ok(entries) = std::fs::read_dir("/sys/block") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            // Skip loop/ram/dm-* virtual devices.
            if s.starts_with("loop") || s.starts_with("ram") || s.starts_with("dm-") {
                continue;
            }
            let rotational_path = entry.path().join("queue").join("rotational");
            match std::fs::read_to_string(&rotational_path)
                .ok()
                .as_deref()
                .map(str::trim)
            {
                Some("0") => any_ssd = true,
                Some("1") => any_hdd = true,
                _ => {}
            }
        }
    }
    if any_ssd {
        Some("SATA-SSD".to_string())
    } else if any_hdd {
        Some("HDD".to_string())
    } else {
        None
    }
}

/// PCIe link speed in GT/s → PCIe generation number (3, 4, 5).
/// Returns `None` for unknown / unsupported speeds (Gen1 = 2.5,
/// Gen2 = 5.0 — both far too slow to be a primary NVMe today; we
/// leave them as None so the caller falls back to "SATA-SSD" or
/// "HDD" via the rotational check).
#[cfg(target_os = "linux")]
fn pcie_gts_to_gen(gts: f64) -> Option<u8> {
    // Use an epsilon comparison; sysfs sometimes reports 32.0 vs
    // 31.5 etc.
    if (gts - 32.0).abs() < 0.5 {
        Some(5)
    } else if (gts - 16.0).abs() < 0.5 {
        Some(4)
    } else if (gts - 8.0).abs() < 0.5 {
        Some(3)
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn platform_detect_disk_class() -> Option<String> {
    // Lean Phase 2 detection: open \\.\PhysicalDrive0 + query its
    // bus type via the existing `winapi_wrappers::query_bus_type`
    // helper (already used by the `drive-info` subcommand). For
    // NVMe-class buses we return "NVMe-Gen4" as a conservative
    // default — accurate Gen3/4/5 distinction requires querying
    // the NVMe identify-controller structure via
    // IOCTL_STORAGE_PROTOCOL_COMMAND, which is Phase 2.1 work
    // (filed as a separate TODO). Non-NVMe buses don't have a
    // cheap SATA-SSD-vs-HDD signal without per-volume seek-penalty
    // IOCTLs that need a real volume GUID; falling back to
    // None → "mixed" is honest.
    use crate::winapi_wrappers::{bus_type, query_bus_type};
    let handle = open_physical_drive_zero().ok()?;
    let bus = query_bus_type(handle.0).ok()?;
    drop(handle);
    let class = match bus {
        bus_type::NVME | bus_type::SCM | bus_type::UFS => "NVMe-Gen4",
        bus_type::ATA => "HDD",
        _ => return None,
    };
    Some(class.to_string())
}

#[cfg(target_os = "windows")]
struct PhysicalDriveHandle(windows::Win32::Foundation::HANDLE);

#[cfg(target_os = "windows")]
impl Drop for PhysicalDriveHandle {
    fn drop(&mut self) {
        use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
        if !self.0.is_invalid() && self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                let _ = windows::Win32::Foundation::CloseHandle(self.0);
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn open_physical_drive_zero() -> std::io::Result<PhysicalDriveHandle> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    let path: Vec<u16> = "\\\\.\\PhysicalDrive0\0".encode_utf16().collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(std::io::Error::other)?;
    if handle == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::other("invalid handle to PhysicalDrive0"));
    }
    Ok(PhysicalDriveHandle(handle))
}

#[cfg(target_os = "macos")]
fn platform_detect_disk_class() -> Option<String> {
    // macOS detection deferred — sysctl + IOKit work but isn't on
    // the burn-queue scope. Returns None so detect_disk_class
    // falls back to "mixed".
    None
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn platform_detect_disk_class() -> Option<String> {
    None
}

/// Snap raw GB down to the schema's enum buckets.
fn snap_ram_bucket(gb: u32) -> String {
    // Schema-allowed values, descending.
    const BUCKETS: &[u32] = &[1024, 512, 256, 128, 64, 32, 16, 8, 4];
    for &b in BUCKETS {
        if gb >= b {
            return b.to_string();
        }
    }
    "4".to_string()
}

fn default_filesystem() -> &'static str {
    if cfg!(target_os = "windows") {
        "NTFS"
    } else {
        "other"
    }
}

/// #88 Phase 1 — Detect the filesystem hosting `path` and map it to
/// the backend schema's enum: `"NTFS" | "ReFS" | "exFAT" | "FAT32" |
/// "network-SMB" | "other"`.
///
/// Path-prefix-based network detection runs first because it's
/// cheap and doesn't require an FS syscall: UNC `\\server\share`,
/// `smb://...`, `nfs://...` short-circuit to `"network-SMB"`.
///
/// Returns `None` when detection can't classify the path at all
/// (path doesn't exist, permission denied, unknown FS); caller
/// falls back to `default_filesystem()`.
///
/// Per-platform fallback:
/// - Windows: `GetVolumeInformationW` returns the FS name string
///   directly ("NTFS" / "ReFS" / "exFAT" / "FAT32" / etc.).
/// - Linux: `statfs` exposes a 32-bit `f_type` magic; compare
///   against known values (NTFS via ntfs-3g / ntfs3, SMB, NFS, etc).
/// - macOS + other Unix: deferred — returns `None` so default
///   ("other") wins.
fn detect_filesystem(path: &std::path::Path) -> Option<String> {
    // Network-share path-prefix check (works on every platform).
    if let Some(s) = path.to_str() {
        let s_lower = s.to_ascii_lowercase();
        if s_lower.starts_with("smb://") || s_lower.starts_with("nfs://") {
            return Some("network-SMB".to_string());
        }
        // Windows UNC: `\\server\share\...` (but NOT `\\?\` verbatim
        // or `\\.\` device which are local-path forms).
        if let Some(rest) = s.strip_prefix("\\\\") {
            let first = rest.chars().next();
            if first != Some('?') && first != Some('.') && !rest.is_empty() {
                return Some("network-SMB".to_string());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        detect_filesystem_windows(path)
    }
    #[cfg(target_os = "linux")]
    {
        detect_filesystem_linux(path)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = path;
        None
    }
}

#[cfg(target_os = "windows")]
fn detect_filesystem_windows(path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetVolumeInformationW;

    // GetVolumeInformationW takes a root-path string ending in `\`.
    // Normalize: take the volume root from the supplied path. For
    // `C:\foo\bar` → `C:\`. For UNC `\\server\share\...` →
    // `\\server\share\`. For verbatim `\\?\C:\foo` → `\\?\C:\`.
    let path_str = path.to_string_lossy();
    let root = volume_root_from_path(&path_str)?;
    let mut wide: Vec<u16> = std::ffi::OsString::from(&root)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // GetVolumeInformationW writes the FS name into a caller-
    // supplied buffer. MAX_PATH (260) is the documented max for
    // the FS name.
    let mut fs_name_buf: [u16; 260] = [0; 260];
    let mut volume_serial: u32 = 0;
    let mut max_component: u32 = 0;
    let mut fs_flags: u32 = 0;
    // SAFETY: PCWSTR points at our null-terminated wide buffer
    // which outlives the call. All output pointers are valid
    // stack locations.
    let result = unsafe {
        GetVolumeInformationW(
            PCWSTR(wide.as_mut_ptr()),
            None,
            Some(&mut volume_serial as *mut u32),
            Some(&mut max_component as *mut u32),
            Some(&mut fs_flags as *mut u32),
            Some(&mut fs_name_buf),
        )
    };
    if result.is_err() {
        return None;
    }
    // Find the null terminator + decode to String.
    let len = fs_name_buf.iter().position(|&c| c == 0).unwrap_or(0);
    let fs_name = String::from_utf16_lossy(&fs_name_buf[..len]);
    // Map raw Windows FS names to the schema's enum. Anything we
    // don't recognise as a first-class type falls back to "other".
    let mapped = match fs_name.as_str() {
        "NTFS" => "NTFS",
        "ReFS" => "ReFS",
        "exFAT" => "exFAT",
        "FAT32" | "FAT" => "FAT32",
        _ => "other",
    };
    Some(mapped.to_string())
}

#[cfg(target_os = "windows")]
fn volume_root_from_path(path: &str) -> Option<String> {
    // `\\?\C:\foo` → `\\?\C:\`
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        if rest.len() >= 2 && rest.as_bytes()[1] == b':' {
            return Some(format!(r"\\?\{}:\\", &rest[..1]));
        }
    }
    // `\\server\share\...` → `\\server\share\` (but this is also
    // the network-share case handled at the top of detect_filesystem;
    // this is a defensive fallback if the prefix check missed).
    if let Some(rest) = path.strip_prefix(r"\\") {
        let mut parts = rest.splitn(3, '\\');
        let (Some(server), Some(share)) = (parts.next(), parts.next()) else {
            return None;
        };
        if server.is_empty() || share.is_empty() {
            return None;
        }
        return Some(format!(r"\\{}\{}\", server, share));
    }
    // `C:\foo\bar` → `C:\`
    if path.len() >= 2 && path.as_bytes()[1] == b':' {
        return Some(format!(r"{}:\\", &path[..1]));
    }
    None
}

#[cfg(target_os = "linux")]
fn detect_filesystem_linux(path: &std::path::Path) -> Option<String> {
    use std::os::unix::ffi::OsStrExt;
    // statfs f_type magic numbers from linux/magic.h. NTFS goes
    // through FUSE-ntfs3 / kernel-ntfs3; both report distinguishable
    // magics. ReFS doesn't have a Linux driver in mainline; deferred.
    // Coverage targets pathfinder's signal classes; rare local FS
    // types fall back to "other" which is also a valid schema value.
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const SMB2_MAGIC_NUMBER: i64 = 0xFE534D42;
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const NTFS_SB_MAGIC: i64 = 0x5346544E; // "NTFS" little-endian
    const NTFS3_SUPER_MAGIC: i64 = 0x7366746E;
    const FAT_FS_MAGIC: i64 = 0x4006;
    const MSDOS_SUPER_MAGIC: i64 = 0x4D44;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: c_path is a valid null-terminated C string;
    // buf is a stack-allocated zeroed statfs.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf as *mut libc::statfs) };
    if rc != 0 {
        return None;
    }
    let ty = buf.f_type as i64;
    let mapped = match ty {
        SMB_SUPER_MAGIC | SMB2_MAGIC_NUMBER | NFS_SUPER_MAGIC => "network-SMB",
        NTFS_SB_MAGIC | NTFS3_SUPER_MAGIC => "NTFS",
        FAT_FS_MAGIC | MSDOS_SUPER_MAGIC => "FAT32",
        _ => "other",
    };
    Some(mapped.to_string())
}

// ============================================================
// CPU model — best-effort across platforms.
// ============================================================

/// Strip the trailing " <N>-Core Processor" suffix from the raw
/// OS-reported brand so the achievements catalog can match the
/// brand alone (pathfinder-ryzen-9-9950x3d et al). Raw AMD reports
/// on Windows + Linux include the core count; Intel and the
/// "unknown" fallback don't, and pass through unchanged.
///
/// Strip rule: walks the string from the end, matching the literal
/// "-Core Processor" suffix (case-sensitive — every observed brand
/// uses it exactly), then walks back over the digit run before it.
/// Anything that doesn't match the shape is left alone so the
/// function is a no-op for future brand formats we haven't seen.
fn normalize_cpu_brand(raw: &str) -> String {
    let trimmed = raw.trim_end();
    let Some(prefix) = trimmed.strip_suffix("-Core Processor") else {
        return trimmed.to_string();
    };
    let digits_end = prefix.trim_end();
    let mut cut = digits_end.len();
    while cut > 0
        && digits_end
            .as_bytes()
            .get(cut - 1)
            .is_some_and(|b| b.is_ascii_digit())
    {
        cut -= 1;
    }
    if cut == digits_end.len() {
        // "-Core Processor" without a preceding digit run; leave
        // the input alone so a future engineer notices the
        // surprise instead of having the function silently mangle
        // it.
        return trimmed.to_string();
    }
    digits_end[..cut].trim_end().to_string()
}

#[cfg(target_os = "linux")]
fn detect_cpu_model() -> String {
    use std::io::BufRead;
    if let Ok(f) = std::fs::File::open("/proc/cpuinfo") {
        for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
            if let Some(rest) = line.strip_prefix("model name") {
                if let Some((_, value)) = rest.split_once(':') {
                    return value.trim().to_string();
                }
            }
        }
    }
    "unknown".to_string()
}

#[cfg(target_os = "macos")]
fn detect_cpu_model() -> String {
    use std::process::Command;
    Command::new("sysctl")
        .args(["-n", "machdep.cpu.brand_string"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(target_os = "windows")]
fn detect_cpu_model() -> String {
    read_registry_string(
        "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0",
        "ProcessorNameString",
    )
    .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_cpu_model() -> String {
    "unknown".to_string()
}

// ============================================================
// ISA flags — CPUID; lowercase + dashes only per backend regex.
// ============================================================

fn detect_isa_flags() -> Vec<String> {
    detect_isa_flags_impl()
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn detect_isa_flags_impl() -> Vec<String> {
    // Inline CPUID; probe leaves 1 + 7 sub-leaf 0 per Intel/AMD
    // vol 2 §3.2 + §3.3. Bit positions are stable. Flag names use
    // dashes (NOT underscores) to satisfy the backend's
    // `^[a-z0-9-]{1,20}$` validator.
    let mut flags: Vec<String> = Vec::new();
    let max_basic = unsafe { cpuid(0, 0) }.eax;

    if max_basic >= 1 {
        let r = unsafe { cpuid(1, 0) };
        if (r.ecx & (1 << 19)) != 0 {
            flags.push("sse4-1".into());
        }
        if (r.ecx & (1 << 20)) != 0 {
            flags.push("sse4-2".into());
        }
        // Backend catalog uses Intel marketing name "aes-ni" (not
        // the bare CPUID-convention "aes"); without this rename the
        // server-side hardware-self-consistency check rejects with
        // "claimed CPU ships with ISA flags not present in payload".
        if (r.ecx & (1 << 25)) != 0 {
            flags.push("aes-ni".into());
        }
        if (r.ecx & (1 << 28)) != 0 {
            flags.push("avx".into());
        }
        if (r.ecx & (1 << 30)) != 0 {
            flags.push("rdrand".into());
        }
        if (r.edx & (1 << 26)) != 0 {
            flags.push("sse2".into());
        }
    }
    if max_basic >= 7 {
        let r = unsafe { cpuid(7, 0) };
        if (r.ebx & (1 << 3)) != 0 {
            flags.push("bmi1".into());
        }
        if (r.ebx & (1 << 5)) != 0 {
            flags.push("avx2".into());
        }
        if (r.ebx & (1 << 8)) != 0 {
            flags.push("bmi2".into());
        }
        if (r.ebx & (1 << 16)) != 0 {
            flags.push("avx512f".into());
        }
        if (r.ebx & (1 << 17)) != 0 {
            flags.push("avx512dq".into());
        }
        if (r.ebx & (1 << 29)) != 0 {
            flags.push("sha".into());
        }
        if (r.ecx & (1 << 9)) != 0 {
            flags.push("vaes".into());
        }
    }
    flags.sort();
    flags.dedup();
    flags
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "x86")))]
fn detect_isa_flags_impl() -> Vec<String> {
    Vec::new()
}

#[derive(Clone, Copy)]
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
struct CpuidResult {
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn cpuid(leaf: u32, sub_leaf: u32) -> CpuidResult {
    let r = std::arch::x86_64::__cpuid_count(leaf, sub_leaf);
    CpuidResult {
        eax: r.eax,
        ebx: r.ebx,
        ecx: r.ecx,
        edx: r.edx,
    }
}

#[cfg(target_arch = "x86")]
#[inline]
unsafe fn cpuid(leaf: u32, sub_leaf: u32) -> CpuidResult {
    let r = std::arch::x86::__cpuid_count(leaf, sub_leaf);
    CpuidResult {
        eax: r.eax,
        ebx: r.ebx,
        ecx: r.ecx,
        edx: r.edx,
    }
}

// ============================================================
// RAM total — raw GiB; caller snaps via snap_ram_bucket.
// ============================================================

#[cfg(target_os = "linux")]
fn detect_ram_gb() -> Option<u32> {
    use std::io::BufRead;
    let f = std::fs::File::open("/proc/meminfo").ok()?;
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb: u64 = rest.trim().trim_end_matches(" kB").trim().parse().ok()?;
            return Some((kb / (1024 * 1024)) as u32);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn detect_ram_gb() -> Option<u32> {
    use std::process::Command;
    let out = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let bytes: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some((bytes / (1024 * 1024 * 1024)) as u32)
}

#[cfg(target_os = "windows")]
fn detect_ram_gb() -> Option<u32> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let mut status = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    unsafe {
        GlobalMemoryStatusEx(&mut status).ok()?;
    }
    Some((status.ullTotalPhys / (1024 * 1024 * 1024)) as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_ram_gb() -> Option<u32> {
    None
}

// ============================================================
// OS version + edition.
//
// `os_version` is a free-form string the backend stores as-is.
// `os_edition` MUST be one of the enum values:
// `"Home" | "Pro" | "Enterprise" | "Education" | "Server" | "Other"`.
// On non-Windows we always say "Other" since the enum is
// Windows-shaped.
// ============================================================

fn detect_os_version() -> String {
    detect_os_version_impl().unwrap_or_else(|| "Unknown".to_string())
}

#[cfg(target_os = "linux")]
fn detect_os_version_impl() -> Option<String> {
    if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
        for line in s.lines() {
            if let Some(rest) = line.strip_prefix("PRETTY_NAME=") {
                let trimmed = rest.trim().trim_matches('"');
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
    }
    std::fs::read_to_string("/proc/version")
        .ok()
        .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "))
}

#[cfg(target_os = "macos")]
fn detect_os_version_impl() -> Option<String> {
    use std::process::Command;
    let name = Command::new("sw_vers")
        .arg("-productName")
        .output()
        .ok()
        .and_then(|o| {
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .unwrap_or_else(|| "macOS".to_string());
    let version = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| {
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        });
    Some(match version {
        Some(v) if !v.is_empty() => format!("{name} {v}"),
        _ => name,
    })
}

#[cfg(target_os = "windows")]
fn detect_os_version_impl() -> Option<String> {
    let key = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
    let product = read_registry_string(key, "ProductName")?;
    let display = read_registry_string(key, "DisplayVersion");
    let build_str = read_registry_string(key, "CurrentBuild");
    let build_num: Option<u32> = build_str.as_deref().and_then(|s| s.parse().ok());
    // ProductName is stuck at "Windows 10 ..." on Win11; substitute
    // when CurrentBuild >= 22000.
    let product = match build_num {
        Some(n) if n >= 22000 => product.replacen("Windows 10", "Windows 11", 1),
        _ => product,
    };
    Some(match display {
        Some(v) if !v.is_empty() => format!("{product} {v}"),
        _ => product,
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_os_version_impl() -> Option<String> {
    None
}

fn detect_os_edition_enum() -> String {
    detect_os_edition_enum_impl().unwrap_or_else(|| "Other".to_string())
}

#[cfg(target_os = "windows")]
fn detect_os_edition_enum_impl() -> Option<String> {
    // Read EditionID directly — it's the canonical Microsoft-side
    // edition identifier (no version-noise to parse around).
    // Common values: "Professional", "Core" (Home), "Enterprise",
    // "Education", "ServerStandard", "ServerDatacenter".
    let key = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
    let edition_id = read_registry_string(key, "EditionID")
        .or_else(|| read_registry_string(key, "InstallationType"));
    edition_id.map(map_windows_edition_to_enum)
}

#[cfg(target_os = "windows")]
fn map_windows_edition_to_enum(edition_id: String) -> String {
    let lower = edition_id.to_lowercase();
    if lower.contains("server") {
        "Server".to_string()
    } else if lower.contains("enterprise") {
        "Enterprise".to_string()
    } else if lower.contains("education") {
        "Education".to_string()
    } else if lower.contains("professional") || lower.contains("pro") {
        "Pro".to_string()
    } else if lower == "core" || lower.contains("home") {
        "Home".to_string()
    } else {
        "Other".to_string()
    }
}

#[cfg(not(target_os = "windows"))]
fn detect_os_edition_enum_impl() -> Option<String> {
    // The edition enum is Windows-shaped; non-Windows installs use
    // "Other" by spec convention.
    Some("Other".to_string())
}

// ============================================================
// Windows registry helper — used by CPU model + OS detection.
// ============================================================

#[cfg(target_os = "windows")]
fn read_registry_string(subkey: &str, value: &str) -> Option<String> {
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    };

    let subkey_w: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let value_w: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let mut key = HKEY::default();
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey_w.as_ptr()),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if opened.is_err() {
        return None;
    }
    let mut buf = [0u16; 256];
    let mut buf_bytes = (buf.len() * 2) as u32;
    let mut kind = REG_SZ;
    let queried = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(value_w.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut buf_bytes),
        )
    };
    unsafe {
        let _ = RegCloseKey(key);
    }
    if queried.is_err() {
        return None;
    }
    let chars = (buf_bytes as usize) / 2;
    let len = buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
    let s = String::from_utf16_lossy(&buf[..len]).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #88 Phase 1 — path-prefix-based network-share detection is
    /// platform-independent + cheap; pin its behaviour across the
    /// known patterns. Covers pathfinder's `network-pioneer` signal
    /// class without needing a real network mount.
    #[test]
    fn detect_filesystem_network_share_prefix() {
        let cases = [
            (r"\\fileserver\public\report.pdf", true),
            (r"\\corp.local\dfs\team", true),
            ("smb://NAS/Media/movie.mkv", true),
            ("nfs://10.0.0.1/export/home", true),
            // verbatim `\\?\` prefix is local, not network
            (r"\\?\C:\Users\Mick\Documents\photo.jpg", false),
            // device-namespace `\\.\PhysicalDrive0` is local
            (r"\\.\PhysicalDrive0", false),
            // local Windows path
            (r"C:\Users\Mick\Documents", false),
            // local Linux path
            ("/home/mick/Documents", false),
        ];
        for (path, expect_network) in cases {
            let fs = detect_filesystem(std::path::Path::new(path));
            let is_network = matches!(fs.as_deref(), Some("network-SMB"));
            assert_eq!(
                is_network, expect_network,
                "path {path:?} expected network={expect_network}, got fs={fs:?}",
            );
        }
    }

    #[test]
    fn normalize_cpu_brand_strips_amd_core_count_suffix() {
        // Pathfinder catalog drift #3 — Windows + Linux both
        // emit the trailing "<N>-Core Processor" string for AMD
        // brand reports; catalog rules match on the brand alone.
        assert_eq!(
            normalize_cpu_brand("AMD Ryzen 9 9950X3D 16-Core Processor"),
            "AMD Ryzen 9 9950X3D",
        );
        assert_eq!(
            normalize_cpu_brand("AMD Ryzen 5 7600X 6-Core Processor"),
            "AMD Ryzen 5 7600X",
        );
        assert_eq!(
            normalize_cpu_brand("AMD Ryzen 9 9950X3D 16-Core Processor   "),
            "AMD Ryzen 9 9950X3D",
        );
    }

    #[test]
    fn normalize_cpu_brand_leaves_intel_brand_unchanged() {
        let intel = "Intel(R) Core(TM) i9-14900K CPU @ 3.20GHz";
        assert_eq!(normalize_cpu_brand(intel), intel);
    }

    #[test]
    fn normalize_cpu_brand_leaves_unknown_unchanged() {
        assert_eq!(normalize_cpu_brand("unknown"), "unknown");
        assert_eq!(normalize_cpu_brand(""), "");
    }

    #[test]
    fn normalize_cpu_brand_no_digits_before_suffix_is_noop() {
        let weird = "Some Brand X-Core Processor";
        assert_eq!(normalize_cpu_brand(weird), weird);
    }

    #[test]
    fn snap_ram_bucket_rounds_down() {
        assert_eq!(snap_ram_bucket(0), "4");
        assert_eq!(snap_ram_bucket(3), "4");
        assert_eq!(snap_ram_bucket(4), "4");
        assert_eq!(snap_ram_bucket(7), "4");
        assert_eq!(snap_ram_bucket(8), "8");
        assert_eq!(snap_ram_bucket(15), "8");
        assert_eq!(snap_ram_bucket(16), "16");
        assert_eq!(snap_ram_bucket(30), "16");
        assert_eq!(snap_ram_bucket(32), "32");
        assert_eq!(snap_ram_bucket(127), "64");
        assert_eq!(snap_ram_bucket(128), "128");
        assert_eq!(snap_ram_bucket(2048), "1024");
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    fn isa_flags_match_backend_regex() {
        // Backend pattern: ^[a-z0-9-]{1,20}$. No underscores.
        let h = detect();
        let re = regex_like_check;
        for f in &h.cpu_isa_flags {
            assert!(
                re(f),
                "ISA flag '{f}' violates backend regex ^[a-z0-9-]{{1,20}}$"
            );
        }
        // SSE2 is x86_64 baseline.
        assert!(h.cpu_isa_flags.iter().any(|f| f == "sse2"));
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    fn regex_like_check(s: &str) -> bool {
        if s.is_empty() || s.len() > 20 {
            return false;
        }
        s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    }

    #[test]
    fn detect_populates_required_fields() {
        let h = detect();
        assert!(h.cpu_threads >= 1);
        assert!(h.cpu_cores >= 1);
        assert!(!h.cpu_model_string.is_empty());
        assert!(!h.ram_total_gb_bucket.is_empty());
        assert!(!h.os_version.is_empty());
        assert!(!h.os_edition.is_empty());
        assert!(!h.disk_class.is_empty());
        assert!(!h.filesystem.is_empty());
        assert!(h.cluster_size_kb >= 1);
        assert!(!h.volume_size_gb_bucket.is_empty());
    }

    #[test]
    fn detect_serialises_with_no_extra_keys() {
        let h = detect();
        let v: serde_json::Value = serde_json::to_value(&h).unwrap();
        let obj = v.as_object().unwrap();
        let allowed: std::collections::HashSet<&str> = [
            "cpu_model_string",
            "cpu_cores",
            "cpu_threads",
            "cpu_isa_flags",
            "ram_total_gb_bucket",
            "os_version",
            "os_edition",
            "disk_class",
            "filesystem",
            "cluster_size_kb",
            "volume_size_gb_bucket",
            "is_dev_drive",
        ]
        .into_iter()
        .collect();
        for k in obj.keys() {
            assert!(
                allowed.contains(k.as_str()),
                "extra key '{k}' not in schema (additionalProperties: false)"
            );
        }
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn map_windows_edition_buckets_known_values() {
        assert_eq!(map_windows_edition_to_enum("Professional".into()), "Pro");
        assert_eq!(map_windows_edition_to_enum("Pro".into()), "Pro");
        assert_eq!(map_windows_edition_to_enum("Core".into()), "Home");
        assert_eq!(
            map_windows_edition_to_enum("CoreSingleLanguage".into()),
            "Home"
        );
        assert_eq!(
            map_windows_edition_to_enum("Enterprise".into()),
            "Enterprise"
        );
        assert_eq!(map_windows_edition_to_enum("Education".into()), "Education");
        assert_eq!(
            map_windows_edition_to_enum("ServerStandard".into()),
            "Server"
        );
        assert_eq!(map_windows_edition_to_enum("WindowsRT".into()), "Other");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn volume_device_path_extracts_drive_from_scanned_root() {
        use std::path::Path;
        // F-CLI-5: derive \\.\<letter>: from the SCANNED root so
        // is_dev_drive probes that volume, not the boot volume.
        assert_eq!(
            volume_device_path_for(Path::new(r"F:\Projects\foo")).as_deref(),
            Some(r"\\.\F:")
        );
        // Verbatim-prefixed root reads the real drive letter (S15).
        assert_eq!(
            volume_device_path_for(Path::new(r"\\?\F:\Projects")).as_deref(),
            Some(r"\\.\F:")
        );
        // Lowercase drive letter is normalised to upper.
        assert_eq!(
            volume_device_path_for(Path::new(r"d:\data")).as_deref(),
            Some(r"\\.\D:")
        );
        // UNC / non-drive roots aren't Dev Drives → None.
        assert!(volume_device_path_for(Path::new(r"\\server\share\x")).is_none());
    }
}
