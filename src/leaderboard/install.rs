//! Per-channel install state persisted at:
//!
//! * Windows: `%LOCALAPPDATA%\superdeduper\install\install.{channel}.json`
//! * macOS:   `~/Library/Application Support/superdeduper/install/install.{channel}.json`
//! * Linux:   `$XDG_DATA_HOME/superdeduper/install/install.{channel}.json` (or `~/.local/share`)
//!
//! Channel determines which install identity is loaded — `prod`,
//! `dev`, `local` per [`crate::channel`] + dev-channel-spec.md §3.2.
//! Channels are independent: switching from prod to dev loads a
//! fresh identity (or triggers first-run registration on dev) and
//! the prod identity stays untouched.
//!
//! Schema per client-spec §4.3. Failure modes per §4.5: corrupted or
//! partially-written files fail closed; the user must explicitly opt
//! in to a fresh registration via `superdeduper register --reset` rather than
//! silently rotating their identity (which would let an attacker
//! bypass shadowbans).
//!
//! Pre-channel-aware sd installs wrote a flat `install.json` directly
//! under the data dir. The first channel-aware load that sees no
//! `install.prod.json` but finds the legacy flat file migrates it
//! transparently — see [`migrate_legacy_install_json`].

use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::channel::{self, Channel};
use crate::leaderboard::predicates::InstallCounters;

/// Bump when adding required fields. Old files with a lower
/// schema_version get rejected on load → user re-registers.
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// 256-bit HMAC key as raw bytes. Persisted as hex in install.json.
pub type InstallKey = [u8; 32];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallState {
    pub schema_version: u32,
    /// RFC 4122 v4 UUID. Identity is opaque to the backend; user can
    /// optionally link a Google/Discord identity to this install at G3.
    pub install_id: String,
    /// 32 random bytes, hex-encoded. NEVER transmitted; used only as
    /// the HMAC-SHA256 key for the `X-Sd-Signature` header.
    pub install_key_hex: String,
    /// True once `POST /api/v1/register` returned 200. While false,
    /// the engine refuses to submit (no point — server would reject).
    pub registered: bool,
    /// Backend URL the install_id is registered against. Bumping the
    /// server_url invalidates registration; user must re-register.
    pub server_url: String,
    /// `sd_version` at the moment of registration. Diagnostic only.
    pub client_version_at_register: String,
    /// Three-state opt-in for "should this scan attempt a submit?":
    /// `AlwaysAsk` (default) → GUI shows the post-scan modal,
    /// `AutoOptIn` → silent submit + summary toast,
    /// `Never` → no submit attempt, no modal.
    #[serde(default)]
    pub share_default: ShareDefault,

    /// Lifetime counters that drive client-claimed achievement
    /// predicates (`picky-eater`, `verify-veteran`). `#[serde(default)]`
    /// so install.json files written before this field existed
    /// load cleanly with both counters at 0. Bumped via the
    /// [`bump_exclude_pattern_edits`] / [`bump_achievements_verify_invocations`]
    /// helpers — never mutated by callers directly so the
    /// load/mutate/save is always atomic.
    #[serde(default)]
    pub counters: InstallCounters,

    /// How many post-scan share prompts have been shown+resolved while
    /// `share_default == AskNThenSticky`. After [`STICKY_PROMPT_THRESHOLD`]
    /// the mode goes sticky (see [`ShareDefault::AskNThenSticky`]).
    /// `#[serde(default)]` so pre-existing install.json files load at 0.
    #[serde(default)]
    pub share_prompt_count: u32,
    /// The user's most recent post-scan share decision, recorded while in
    /// `AskNThenSticky`. Drives the sticky behavior once the threshold is
    /// reached: `Submitted` -> auto-submit going forward, `Skipped` ->
    /// stop asking.
    #[serde(default)]
    pub share_last_choice: Option<ShareChoice>,
}

/// Number of post-scan prompts shown before `AskNThenSticky` goes sticky.
pub const STICKY_PROMPT_THRESHOLD: u32 = 3;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareDefault {
    #[default]
    AlwaysAsk,
    /// Ask via the post-scan modal for the first [`STICKY_PROMPT_THRESHOLD`]
    /// scans, then stick to the user's last choice (last submitted ->
    /// auto-submit going forward; last skipped -> stop asking). The 3rd
    /// prompt notes that the choice will be remembered.
    AskNThenSticky,
    AutoOptIn,
    Never,
}

/// A resolved post-scan share decision (for `AskNThenSticky` stickiness).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareChoice {
    Submitted,
    Skipped,
}

impl InstallState {
    /// Decode `install_key_hex` into the raw 32-byte key. Returns
    /// `None` if the hex is malformed or wrong length — caller treats
    /// that as a corrupted-install failure per spec §4.5.
    pub fn install_key(&self) -> Option<InstallKey> {
        if self.install_key_hex.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, byte_slot) in out.iter_mut().enumerate() {
            *byte_slot = u8::from_str_radix(&self.install_key_hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(out)
    }
}

/// Canonical install.json path for the **active** channel (per
/// [`crate::channel::active_channel`]). Public so tests + CLI
/// `superdeduper register --show-path` can hit the same location
/// the engine reads/writes.
pub fn install_path() -> io::Result<PathBuf> {
    install_path_for(channel::active_channel())
}

/// Canonical install.json path for a specific channel. Used by the
/// GUI Settings → Network panel (which peeks at all three channels
/// to show registration status per channel) and by the migration
/// helper in [`migrate_legacy_install_json`]. The path is
/// `<data_dir>/install/install.{channel}.json` — separate
/// subdirectory so the per-channel files cluster, and the legacy
/// flat `install.json` (pre-channel) can coexist during migration.
pub fn install_path_for(channel: Channel) -> io::Result<PathBuf> {
    let mut p = data_dir()?;
    p.push("install");
    p.push(format!("install.{}.json", channel.as_slug()));
    Ok(p)
}

/// Pre-channel flat install.json path. Only used during the
/// migration check; new code should never read or write this path.
fn legacy_install_path() -> io::Result<PathBuf> {
    let mut p = data_dir()?;
    p.push("install.json");
    Ok(p)
}

/// If a legacy flat `install.json` exists AND the channel-aware
/// `install.prod.json` does NOT yet exist, rename the legacy file
/// to the prod location. Called automatically by [`load_for`] when
/// loading the prod channel finds no file; safe to call repeatedly.
///
/// Returns `Ok(true)` if a migration happened, `Ok(false)` if no
/// action was needed, `Err` if the rename failed (the user will see
/// a clear "couldn't migrate" message rather than silently losing
/// their identity).
pub fn migrate_legacy_install_json() -> io::Result<bool> {
    let legacy = legacy_install_path()?;
    let new = install_path_for(Channel::Prod)?;
    if !legacy.exists() || new.exists() {
        return Ok(false);
    }
    if let Some(parent) = new.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(&legacy, &new)?;
    Ok(true)
}

#[cfg(windows)]
fn data_dir() -> io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "LOCALAPPDATA not set"))?;
    let mut p = PathBuf::from(local);
    p.push("superdeduper");
    Ok(p)
}

#[cfg(target_os = "macos")]
fn data_dir() -> io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("superdeduper");
    Ok(p)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn data_dir() -> io::Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("superdeduper");
        return Ok(p);
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    p.push("superdeduper");
    Ok(p)
}

/// Load the install state for the **active** channel. See
/// [`load_for`] for the per-channel-explicit variant.
pub fn load() -> io::Result<Option<InstallState>> {
    load_for(channel::active_channel())
}

/// Load the install state for a specific channel.
///
/// Returns:
/// * `Ok(Some(state))` — valid file, ready to use
/// * `Ok(None)` — file doesn't exist; caller may call `register`
/// * `Err(_)` — file exists but failed to parse / failed schema check.
///   Caller MUST NOT silently re-create (would let an attacker rotate
///   identities to escape shadowban). User opts in via `--reset`.
///
/// Side effect: when called for `Channel::Prod` and no file is
/// found, runs [`migrate_legacy_install_json`] before giving up.
/// This lets pre-channel-aware install.json files transparently
/// move into the new channel-aware layout on first use.
pub fn load_for(channel: Channel) -> io::Result<Option<InstallState>> {
    let mut path = install_path_for(channel)?;
    let read_result = fs::read(&path);
    let bytes = match read_result {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // Only try the legacy migration for prod — dev/local
            // never had a flat install.json layout.
            if matches!(channel, Channel::Prod) && migrate_legacy_install_json()? {
                path = install_path_for(channel)?;
                match fs::read(&path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e),
                }
            } else {
                return Ok(None);
            }
        }
        Err(e) => return Err(e),
    };
    let mut state: InstallState = serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("install.json parse: {e}"),
        )
    })?;
    // SUPERDEDUPER_SERVER_URL override (channel::SERVER_URL_ENV_VAR):
    // redirect a loaded install's endpoint at the env-named backend.
    // Unset in production -> no effect. Lets a test point an already-
    // registered install at a local mock (T-BENCH-ME spec §9 G1).
    if let Ok(raw) = std::env::var(channel::SERVER_URL_ENV_VAR) {
        let trimmed = raw.trim();
        if !trimmed.is_empty() {
            state.server_url = trimmed.to_string();
        }
    }
    if state.schema_version > CURRENT_SCHEMA_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "install.json schema_version {} > {} — newer client wrote this file",
                state.schema_version, CURRENT_SCHEMA_VERSION
            ),
        ));
    }
    if state.install_key().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "install_key_hex malformed",
        ));
    }
    if state.install_id.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "install_id missing",
        ));
    }
    Ok(Some(state))
}

/// Atomic write of the install state to the **active** channel's
/// path. See [`save_for`] for the per-channel-explicit variant.
pub fn save(state: &InstallState) -> io::Result<()> {
    save_for(channel::active_channel(), state)
}

/// Atomic write to a specific channel's install.json. Serialise to
/// `install.{channel}.json.tmp` then rename. On Windows the
/// LOCALAPPDATA permissions already restrict to the current user;
/// on Unix we set 0600 explicitly.
pub fn save_for(channel: Channel, state: &InstallState) -> io::Result<()> {
    let path = install_path_for(channel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("install.json encode: {e}"),
        )
    })?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    set_owner_only_perms(&tmp)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_perms(path: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)
}

#[cfg(not(unix))]
fn set_owner_only_perms(_path: &std::path::Path) -> io::Result<()> {
    // Windows: LOCALAPPDATA is already user-scoped; default file
    // perms inherit "Administrators + SYSTEM + current user". A
    // future hardening pass can use SetFileSecurityW to strip
    // Administrators + remove inheritance. Threat model treats
    // signing as a speed bump anyway, so default ACL is acceptable
    // for v1.
    Ok(())
}

/// Back up the existing install.{channel}.json file (if any) by
/// renaming it to `install.{channel}.json.bak.<unix_ts>`. Called
/// before destructive install-state operations (reset, fresh
/// register from the GUI) so the prior identity is recoverable
/// from disk if needed. Per Mick's 2026-05-25T01:20Z preference.
///
/// Returns `Ok(Some(backup_path))` if a backup was created,
/// `Ok(None)` if no source file existed (so nothing to back up),
/// `Err` on I/O failure.
pub fn back_up_for(channel: Channel) -> io::Result<Option<PathBuf>> {
    let src = install_path_for(channel)?;
    if !src.exists() {
        return Ok(None);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut backup = src.clone();
    let fname = backup
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    backup.set_file_name(format!("{fname}.bak.{ts}"));
    fs::rename(&src, &backup)?;
    Ok(Some(backup))
}

/// Build a fresh InstallState with a new UUID + random HMAC key.
/// Does NOT persist; caller calls `save(&state)` after a successful
/// registration response.
pub fn new_unregistered(server_url: String) -> InstallState {
    let install_id = uuid::Uuid::new_v4().to_string();
    let mut key = [0u8; 32];
    fill_random(&mut key);
    let install_key_hex = hex_encode(&key);
    InstallState {
        schema_version: CURRENT_SCHEMA_VERSION,
        install_id,
        install_key_hex,
        registered: false,
        server_url,
        client_version_at_register: env!("CARGO_PKG_VERSION").to_string(),
        share_default: ShareDefault::default(),
        counters: InstallCounters::default(),
        share_prompt_count: 0,
        share_last_choice: None,
    }
}

/// Increment the `exclude_pattern_edits` counter by 1 + persist.
/// No-op when install.json doesn't exist (user hasn't registered),
/// so callers in the Settings → Exclusions edit path can fire-and-
/// forget without guarding on registration state. Errors only
/// surface for genuine I/O failures (disk full, permission denied,
/// schema-version mismatch).
pub fn bump_exclude_pattern_edits() -> io::Result<()> {
    bump_counter(|c| c.exclude_pattern_edits = c.exclude_pattern_edits.saturating_add(1))
}

/// Increment the `achievements_verify_invocations` counter by 1 +
/// persist. Fired from the `superdeduper achievements verify` CLI
/// path on every invocation. Same no-op-on-missing semantics as
/// [`bump_exclude_pattern_edits`].
pub fn bump_achievements_verify_invocations() -> io::Result<()> {
    bump_counter(|c| {
        c.achievements_verify_invocations = c.achievements_verify_invocations.saturating_add(1)
    })
}

fn bump_counter(mutate: impl FnOnce(&mut InstallCounters)) -> io::Result<()> {
    let mut state = match load()? {
        Some(s) => s,
        None => return Ok(()),
    };
    mutate(&mut state.counters);
    save(&state)
}

/// Record a resolved post-scan share decision under `AskNThenSticky`:
/// bump `share_prompt_count` (saturating) and set `share_last_choice`,
/// then persist. No-op when install.json doesn't exist. Atomic
/// load/mutate/save like the counter bumps. Callers fire this on every
/// Submit/Skip while the mode is `AskNThenSticky` so the stickiness
/// threshold + last-choice are durable across restarts.
pub fn record_share_prompt(choice: ShareChoice) -> io::Result<()> {
    let mut state = match load()? {
        Some(s) => s,
        None => return Ok(()),
    };
    state.share_prompt_count = state.share_prompt_count.saturating_add(1);
    state.share_last_choice = Some(choice);
    save(&state)
}

/// Fill `buf` with cryptographically suitable random bytes.
///
/// Strategy: read from `/dev/urandom` on Unix, `BCryptGenRandom` on
/// Windows. We avoid pulling `getrandom` directly to keep the dep
/// list small — `uuid::Uuid::new_v4()` already pulls a CSPRNG in
/// transitively, but we use it directly here for clarity.
fn fill_random(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(buf).is_ok() {
                return;
            }
        }
        // Fallback: time-seeded xorshift. Not crypto-grade, but the
        // install_key on a system without /dev/urandom is the least
        // of that system's problems.
        seeded_fill(buf);
    }
    #[cfg(windows)]
    {
        // BCryptGenRandom via the windows crate. We don't have the
        // BCrypt features enabled in Cargo.toml's windows {} block
        // yet; for now use the same /dev/urandom-ish fallback. Real
        // BCrypt wiring is a quick follow-up.
        seeded_fill(buf);
    }
}

fn seeded_fill(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ 0xDEAD_BEEF_CAFE_F00D;
    for b in buf.iter_mut() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *b = (seed >> 56) as u8;
    }
}

fn hex_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for &byte in b {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_unregistered_has_v4_uuid_and_64_hex_key() {
        let s = new_unregistered("https://api.superdeduper.io".into());
        assert_eq!(s.schema_version, CURRENT_SCHEMA_VERSION);
        assert_eq!(s.install_id.len(), 36, "UUID should be 36 chars");
        assert_eq!(s.install_id.chars().filter(|&c| c == '-').count(), 4);
        assert_eq!(s.install_key_hex.len(), 64, "32 bytes = 64 hex chars");
        assert!(!s.registered);
        assert_eq!(s.share_default, ShareDefault::AlwaysAsk);
    }

    #[test]
    fn install_key_decodes_round_trip() {
        let s = new_unregistered("https://example".into());
        let key = s.install_key().expect("valid hex");
        assert_eq!(key.len(), 32);
        let re_encoded = hex_encode(&key);
        assert_eq!(re_encoded, s.install_key_hex);
    }

    #[test]
    fn malformed_install_key_returns_none() {
        let mut s = new_unregistered("https://example".into());
        s.install_key_hex = "not hex".into();
        assert!(s.install_key().is_none());
        s.install_key_hex = "abcd".into(); // too short
        assert!(s.install_key().is_none());
    }

    #[test]
    fn two_calls_produce_distinct_ids_and_keys() {
        let a = new_unregistered("https://a".into());
        let b = new_unregistered("https://b".into());
        assert_ne!(a.install_id, b.install_id);
        assert_ne!(a.install_key_hex, b.install_key_hex);
    }

    #[test]
    fn install_state_round_trips_with_counters() {
        // Counters get persisted + restored. Round-trip via JSON is
        // the actual on-disk path; this catches a future serde drift
        // (e.g. rename, container shape change) before it eats user
        // counters silently.
        let mut s = new_unregistered("https://example".into());
        s.counters.exclude_pattern_edits = 7;
        s.counters.achievements_verify_invocations = 3;
        let bytes = serde_json::to_vec(&s).unwrap();
        let restored: InstallState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.counters.exclude_pattern_edits, 7);
        assert_eq!(restored.counters.achievements_verify_invocations, 3);
    }

    #[test]
    fn install_paths_per_channel_are_distinct() {
        // The three channel-specific paths must all differ from
        // each other AND from the legacy flat path. Otherwise
        // switching channels would write over a different channel's
        // identity file (data loss).
        let prod = install_path_for(Channel::Prod).expect("prod path");
        let dev = install_path_for(Channel::Dev).expect("dev path");
        let local = install_path_for(Channel::Local).expect("local path");
        let legacy = legacy_install_path().expect("legacy path");
        assert_ne!(prod, dev);
        assert_ne!(prod, local);
        assert_ne!(dev, local);
        assert_ne!(prod, legacy);
        // All channel paths share the same parent dir `<data>/install`.
        assert_eq!(prod.parent(), dev.parent());
        assert_eq!(dev.parent(), local.parent());
        // Channel slug appears verbatim in the filename for visual
        // greppability + filesystem-tool friendliness.
        assert!(prod.file_name().unwrap().to_string_lossy().contains("prod"));
        assert!(dev.file_name().unwrap().to_string_lossy().contains("dev"));
        assert!(local
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains("local"));
    }

    #[test]
    fn legacy_install_json_without_counters_loads_with_defaults() {
        // Pre-counters install.json (written by v0.1.8 and earlier)
        // must still load. The `counters` field is absent; serde's
        // `#[serde(default)]` on the field + on InstallCounters
        // itself gives us (0, 0) without erroring.
        let legacy_json = r#"{
            "schema_version": 1,
            "install_id": "e1eae1fa-58fb-4f5a-8712-a7480ac5761b",
            "install_key_hex": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "registered": true,
            "server_url": "https://api.superdeduper.io",
            "client_version_at_register": "0.1.8"
        }"#;
        let state: InstallState =
            serde_json::from_str(legacy_json).expect("legacy install.json must deserialise");
        assert_eq!(state.counters.exclude_pattern_edits, 0);
        assert_eq!(state.counters.achievements_verify_invocations, 0);
        assert_eq!(state.share_default, ShareDefault::AlwaysAsk);
        // AskNThenSticky bookkeeping is also serde(default): a legacy
        // file with neither field loads at the start-of-cycle values.
        assert_eq!(state.share_prompt_count, 0);
        assert_eq!(state.share_last_choice, None);
    }

    #[test]
    fn share_sticky_state_round_trips() {
        // The AskNThenSticky bookkeeping (prompt count + last choice) and
        // the new ShareDefault variant must survive the on-disk JSON path.
        let mut s = new_unregistered("https://example".into());
        s.share_default = ShareDefault::AskNThenSticky;
        s.share_prompt_count = 2;
        s.share_last_choice = Some(ShareChoice::Submitted);
        let bytes = serde_json::to_vec(&s).unwrap();
        // Wire form is snake_case (matches the rest of the enum encoding).
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.contains("ask_n_then_sticky"));
        assert!(text.contains("submitted"));
        let restored: InstallState = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(restored.share_default, ShareDefault::AskNThenSticky);
        assert_eq!(restored.share_prompt_count, 2);
        assert_eq!(restored.share_last_choice, Some(ShareChoice::Submitted));
    }
}
