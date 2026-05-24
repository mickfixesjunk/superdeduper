//! Hardware-detect surface per client-spec §3.
//!
//! Emits raw values (CPU model string, thread count, RAM total,
//! OS family). Backend derives the `hardware_class` bracket. Privacy-
//! hard: no usernames, no computer names, no IPs, no drive letters.
//!
//! Platform-specific bits (ISA flags via CPUID, per-volume disk
//! class via IOCTL, OS edition strings) are TODOs — the wire-format
//! Optional fields are populated when available, omitted otherwise.
//! Backend treats missing fields as "unknown" rather than failing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareFingerprint {
    /// Bump when adding required fields; backend dispatches on this.
    pub schema_version: u32,

    /// Vendor + model as reported by the OS, e.g.
    /// `"AMD Ryzen 9 9950X3D 16-Core Processor"`.
    pub cpu_model_string: String,

    /// `std::thread::available_parallelism()` — total logical threads
    /// visible to the scheduler.
    pub cpu_threads: usize,

    /// CPUID ISA flag names sorted lexically, e.g.
    /// `["aes", "avx2", "avx512f", "sse4_2"]`. Empty until CPUID
    /// wiring lands.
    #[serde(default)]
    pub cpu_isa_flags: Vec<String>,

    /// Total physical RAM in GiB, rounded down. `None` if the OS
    /// query failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_gb_total: Option<u32>,

    /// OS family: `"windows" | "linux" | "macos" | "other"`. Per
    /// `std::env::consts::OS`.
    pub os_family: String,

    /// OS-specific edition / version string, e.g. `"Windows 11 24H2"`,
    /// `"Linux 6.6.114"`. `None` if not yet wired for that platform.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_edition: Option<String>,
}

pub const HARDWARE_SCHEMA_VERSION: u32 = 1;

pub fn detect() -> HardwareFingerprint {
    HardwareFingerprint {
        schema_version: HARDWARE_SCHEMA_VERSION,
        cpu_model_string: detect_cpu_model(),
        cpu_threads: std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
        cpu_isa_flags: detect_isa_flags(),
        ram_gb_total: detect_ram_gb(),
        os_family: std::env::consts::OS.to_string(),
        os_edition: detect_os_edition(),
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
    // sysctl machdep.cpu.brand_string is the canonical source on
    // macOS. Shell out via `sysctl -n` for now; future hardening
    // can use libc::sysctlbyname directly.
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
    // Registry: HKLM\HARDWARE\DESCRIPTION\System\CentralProcessor\0
    // → "ProcessorNameString". Shell-free; uses the windows crate's
    // Registry API. CREATE_NO_WINDOW not needed — RegOpenKeyEx is
    // a direct API call, no subprocess.
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ,
    };

    let subkey: Vec<u16> = "HARDWARE\\DESCRIPTION\\System\\CentralProcessor\\0"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "ProcessorNameString"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut key = HKEY::default();
    // SAFETY: PCWSTRs are null-terminated and outlive the calls.
    let opened = unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            0,
            KEY_READ,
            &mut key,
        )
    };
    if opened.is_err() {
        return "unknown".to_string();
    }
    let mut buf = [0u16; 256];
    let mut buf_bytes = (buf.len() * 2) as u32;
    let mut kind = REG_SZ;
    // SAFETY: buffer and length passed correctly.
    let queried = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(value.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut buf_bytes),
        )
    };
    // SAFETY: handle is open.
    unsafe {
        let _ = RegCloseKey(key);
    }
    if queried.is_err() {
        return "unknown".to_string();
    }
    let chars = (buf_bytes as usize) / 2;
    let len = buf[..chars].iter().position(|&c| c == 0).unwrap_or(chars);
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn detect_cpu_model() -> String {
    "unknown".to_string()
}

// ============================================================
// ISA flags — CPUID parsing; deferred. Empty until wired.
// ============================================================

fn detect_isa_flags() -> Vec<String> {
    detect_isa_flags_impl()
}

#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
fn detect_isa_flags_impl() -> Vec<String> {
    // Inline CPUID — no external crate. Probe leaves 1 + 7 sub-leaf 0
    // per Intel/AMD vol 2 §3.2 + §3.3. Bit positions are stable.
    let mut flags: Vec<String> = Vec::new();
    let max_basic = unsafe { cpuid(0, 0) }.eax;

    if max_basic >= 1 {
        let r = unsafe { cpuid(1, 0) };
        // ECX
        if (r.ecx & (1 << 19)) != 0 { flags.push("sse4_1".into()); }
        if (r.ecx & (1 << 20)) != 0 { flags.push("sse4_2".into()); }
        if (r.ecx & (1 << 25)) != 0 { flags.push("aes".into()); }
        if (r.ecx & (1 << 28)) != 0 { flags.push("avx".into()); }
        if (r.ecx & (1 << 30)) != 0 { flags.push("rdrand".into()); }
        // EDX
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
    // ARM / RISC-V: empty for now. Future enhancement: inspect
    // HWCAP (Linux) / sysctl `hw.optional.*` (macOS) for NEON / SVE.
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
// RAM total — best-effort across platforms.
// ============================================================

#[cfg(target_os = "linux")]
fn detect_ram_gb() -> Option<u32> {
    use std::io::BufRead;
    let f = std::fs::File::open("/proc/meminfo").ok()?;
    for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            // "MemTotal:       16334364 kB"
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
    // SAFETY: status has its dwLength field set as required by the API.
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
// OS edition — best-effort; deferred to follow-up commits per
// platform. Linux gets the kernel uname for now.
// ============================================================

#[cfg(target_os = "linux")]
fn detect_os_edition() -> Option<String> {
    std::fs::read_to_string("/proc/version")
        .ok()
        .and_then(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" ").into())
        .map(|s: String| s)
        .map(Some)
        .unwrap_or(None)
}

#[cfg(not(target_os = "linux"))]
fn detect_os_edition() -> Option<String> {
    // TODO(g1-followup): Windows registry CurrentVersion +
    // DisplayVersion; macOS sw_vers -productVersion.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
    fn isa_flags_contain_sse2_on_x86() {
        // Every x86_64 CPU has SSE2 (it's part of the baseline ISA).
        let h = detect();
        assert!(
            h.cpu_isa_flags.iter().any(|f| f == "sse2"),
            "x86_64 CPU should always report sse2; got {:?}",
            h.cpu_isa_flags
        );
    }

    #[test]
    fn isa_flags_are_sorted_and_deduped() {
        let h = detect();
        let mut sorted = h.cpu_isa_flags.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            h.cpu_isa_flags, sorted,
            "isa_flags must be pre-sorted + deduped on the wire"
        );
    }

    #[test]
    fn detect_populates_required_fields() {
        let h = detect();
        assert_eq!(h.schema_version, HARDWARE_SCHEMA_VERSION);
        assert!(h.cpu_threads >= 1);
        assert!(!h.os_family.is_empty());
        // cpu_model_string is best-effort; should at least not be empty
        // (returns "unknown" when detection fails).
        assert!(!h.cpu_model_string.is_empty());
    }

    #[test]
    fn detect_serialises_cleanly() {
        let h = detect();
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("schema_version"));
        assert!(json.contains("cpu_model_string"));
        assert!(json.contains("cpu_threads"));
        // ram_gb_total + os_edition skip-if-none; cpu_isa_flags
        // always present (default to empty vec).
        assert!(json.contains("cpu_isa_flags"));
    }
}
