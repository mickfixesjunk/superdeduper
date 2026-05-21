//! Persisted per-volume HDD/SSD render overrides.
//!
//! The detection in `winapi_wrappers::query_storage_device` gets the
//! HDD-vs-SSD answer right most of the time, but external USB
//! enclosures whose firmware doesn't pass the seek-penalty IOCTL
//! through end up classified by the conservative "ambiguous bus →
//! HDD" fallback. Once the user has clicked the badge to override
//! that to SSD (or vice versa), the choice should stick across runs
//! so they don't have to re-do it every session.
//!
//! Storage shape: a single JSON file at
//! `%LOCALAPPDATA%\superdupe\drive-overrides.json`, mapping
//! **volume GUID** to override-bool. We key on volume GUID rather
//! than drive letter because external drives reshuffle letters
//! depending on plug order — the GUID is stable across mounts.
//!
//! The bool semantics match the in-memory `drive_render_overrides`:
//!   `Some(true)`  ⇒ force SSD render
//!   `Some(false)` ⇒ force HDD render
//!   missing key  ⇒ use auto-detection
//!
//! The file is read on every drive-discovery event so manually
//! editing it from outside the app takes effect on the next scan.
//! Atomic writes (`.tmp` → rename) keep it crash-safe.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Result;

/// Backed by `std::collections::HashMap` (rather than `hashbrown::`)
/// because we want the stock serde derive — hashbrown's serde
/// feature isn't pulled in here and the override file is tiny, so
/// the marginal hash-perf difference doesn't matter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DriveOverrides {
    pub schema: String,
    /// volume GUID → override. `true` = force SSD, `false` = force
    /// HDD. Missing key means "use auto-detection".
    pub overrides: HashMap<String, bool>,
}

pub const OVERRIDES_SCHEMA: &str = "superdupe.drive-overrides.v1";

pub fn overrides_path() -> Result<PathBuf> {
    let mut p = crate::cache::default_cache_path()?;
    p.set_file_name("drive-overrides.json");
    Ok(p)
}

pub fn load() -> Result<DriveOverrides> {
    let path = overrides_path()?;
    match std::fs::read(&path) {
        Ok(b) => Ok(serde_json::from_slice(&b).unwrap_or_default()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DriveOverrides {
            schema: OVERRIDES_SCHEMA.into(),
            overrides: HashMap::new(),
        }),
        Err(e) => Err(crate::Error::Io(e)),
    }
}

/// Set (or unset, when `value` is `None`) the override for a single
/// volume. Reads the current file, applies the change, writes it
/// back atomically. We round-trip the file rather than holding state
/// in memory because the override is small and infrequent —
/// keeping it as the source of truth means concurrent processes (or
/// the user's text editor) can't get out of sync.
pub fn set(volume_guid: &str, value: Option<bool>) -> Result<()> {
    let mut current = load()?;
    if current.schema.is_empty() {
        current.schema = OVERRIDES_SCHEMA.into();
    }
    match value {
        Some(v) => {
            current.overrides.insert(volume_guid.to_string(), v);
        }
        None => {
            current.overrides.remove(volume_guid);
        }
    }
    write_atomic(&overrides_path()?, &current)
}

fn write_atomic(path: &Path, value: &DriveOverrides) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| crate::Error::other(format!("drive-overrides encode: {e}")))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
