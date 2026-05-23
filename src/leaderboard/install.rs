//! Per-install state persisted at:
//!
//! * Windows: `%LOCALAPPDATA%\superdeduper\install.json`
//! * macOS:   `~/Library/Application Support/superdeduper/install.json`
//! * Linux:   `$XDG_DATA_HOME/superdeduper/install.json`
//!
//! File permissions: `0600` on Unix; NTFS ACL restricted to the
//! current SID on Windows (no inheritance, no Authenticated Users).
//!
//! Schema per client-spec §4.3. Failure modes per §4.5 — corrupted
//! / partially-written files fail closed; user must `sd register
//! --reset` after explicit consent to bypass.
//!
//! TODO(g1): implement against client-spec §4.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub schema_version: u32,
    pub install_id: String,            // UUID v4
    pub install_key_hex: String,       // 32 random bytes, hex-encoded
    pub registered: bool,
    pub server_url: String,
    pub client_version_at_register: String,
    pub share_default: ShareDefault,
    // pub per_field_overrides: HashMap<String, FieldOverride>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ShareDefault {
    AlwaysAsk,
    AutoOptIn,
    Never,
}

pub fn load_or_create() -> std::io::Result<InstallState> {
    todo!("g1: load + verify + lazily create install.json")
}

pub fn save(_state: &InstallState) -> std::io::Result<()> {
    todo!("g1: atomic write with 0600/ACL")
}
