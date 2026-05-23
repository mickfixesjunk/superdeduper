//! Hardware-detect surface per client-spec §3.
//!
//! Emits raw values (CPU model string, ISA flag list, RAM bucket,
//! disk class, OS edition, filesystem, etc.). Backend derives the
//! `hardware_class` bracket from these — engine does NOT classify.
//!
//! Privacy-hard: no usernames, no computer names, no IPs, no drive
//! letters. Disk class comes from the existing IOCTL surface used
//! by the preflight probe (`src/winapi_wrappers/windows_impl.rs`).
//!
//! TODO(g1): implement against client-spec §3.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct HardwareFingerprint {
    // Populated by `detect()` per client-spec §3 field list.
    // 14 fields total; see spec for the canonical schema.
    pub schema_version: u32,
    // ... rest TBD during G1 implementation.
}

pub fn detect() -> HardwareFingerprint {
    todo!("g1: hardware-detect per client-spec §3")
}
