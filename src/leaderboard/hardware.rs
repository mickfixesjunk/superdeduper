//! Hardware-detect surface per the live backend schema at
//! `https://api.superdeduper.io/api/v1/submit/schema.json`.
//!
//! Field shape is dictated by the backend's Zod schema; we MUST emit
//! exactly the keys it lists in `hardware.required` (and no extras —
//! `additionalProperties: false`). Fields we can't reliably detect
//! today fall back to documented defaults rather than failing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

pub fn detect() -> HardwareFingerprint {
    let cpu_threads = std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1);
    // Without a robust per-platform physical-core probe, approximate
    // as half threads (most modern x86 CPUs have 2-way SMT). Clamp to
    // at least 1.
    let cpu_cores = (cpu_threads / 2).max(1);
    let ram_gb_raw = detect_ram_gb().unwrap_or(16);
    HardwareFingerprint {
        cpu_model_string: detect_cpu_model(),
        cpu_cores,
        cpu_threads,
        cpu_isa_flags: detect_isa_flags(),
        ram_total_gb_bucket: snap_ram_bucket(ram_gb_raw),
        os_version: detect_os_version(),
        os_edition: detect_os_edition_enum(),
        disk_class: "mixed".to_string(),
        filesystem: default_filesystem().to_string(),
        cluster_size_kb: 4,
        volume_size_gb_bucket: "1024".to_string(),
    }
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

// ============================================================
// CPU model — best-effort across platforms.
// ============================================================

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
        if (r.ecx & (1 << 19)) != 0 { flags.push("sse4-1".into()); }
        if (r.ecx & (1 << 20)) != 0 { flags.push("sse4-2".into()); }
        // Backend catalog uses Intel marketing name "aes-ni" (not
        // the bare CPUID-convention "aes"); without this rename the
        // server-side hardware-self-consistency check rejects with
        // "claimed CPU ships with ISA flags not present in payload".
        if (r.ecx & (1 << 25)) != 0 { flags.push("aes-ni".into()); }
        if (r.ecx & (1 << 28)) != 0 { flags.push("avx".into()); }
        if (r.ecx & (1 << 30)) != 0 { flags.push("rdrand".into()); }
        if (r.edx & (1 << 26)) != 0 { flags.push("sse2".into()); }
    }
    if max_basic >= 7 {
        let r = unsafe { cpuid(7, 0) };
        if (r.ebx & (1 << 3))  != 0 { flags.push("bmi1".into()); }
        if (r.ebx & (1 << 5))  != 0 { flags.push("avx2".into()); }
        if (r.ebx & (1 << 8))  != 0 { flags.push("bmi2".into()); }
        if (r.ebx & (1 << 16)) != 0 { flags.push("avx512f".into()); }
        if (r.ebx & (1 << 17)) != 0 { flags.push("avx512dq".into()); }
        if (r.ebx & (1 << 29)) != 0 { flags.push("sha".into()); }
        if (r.ecx & (1 << 9))  != 0 { flags.push("vaes".into()); }
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
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
}

#[cfg(target_arch = "x86")]
#[inline]
unsafe fn cpuid(leaf: u32, sub_leaf: u32) -> CpuidResult {
    let r = std::arch::x86::__cpuid_count(leaf, sub_leaf);
    CpuidResult { eax: r.eax, ebx: r.ebx, ecx: r.ecx, edx: r.edx }
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
            let kb: u64 = rest
                .trim()
                .trim_end_matches(" kB")
                .trim()
                .parse()
                .ok()?;
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
        .and_then(|o| o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string()))
        .unwrap_or_else(|| "macOS".to_string());
    let version = Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string()));
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
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(map_windows_edition_to_enum("CoreSingleLanguage".into()), "Home");
        assert_eq!(map_windows_edition_to_enum("Enterprise".into()), "Enterprise");
        assert_eq!(map_windows_edition_to_enum("Education".into()), "Education");
        assert_eq!(map_windows_edition_to_enum("ServerStandard".into()), "Server");
        assert_eq!(map_windows_edition_to_enum("WindowsRT".into()), "Other");
    }
}
