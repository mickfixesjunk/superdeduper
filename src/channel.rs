//! Server-channel selection — `prod` / `dev` / `local`.
//!
//! Per `~/sd-bench-local/design/dev-channel-spec.md`. Each invocation
//! of sd talks to ONE channel; the channel determines every server
//! URL (`server_url_for`) and the install-identity file path
//! (`install::install_path_for`).
//!
//! ## Resolution precedence
//!
//! Higher beats lower:
//!
//! 1. **CLI flag**: `--channel <name>` on the active subcommand
//! 2. **Environment**: `SUPERDEDUPER_CHANNEL=<name>`
//! 3. **Config file**: `[network] channel = "<name>"` in the
//!    user-config dir
//! 4. **Default**: `prod`
//!
//! ## Why a tiny dedicated module
//!
//! The channel concept is cross-cutting: it's read by the
//! telemetry submit path, the CLI footer, the GUI banner, the
//! install-state loader, and the OAuth flow. Centralising the
//! enum + parser + resolver here keeps each consumer from
//! re-inventing the precedence chain (a common foot-gun: dev
//! installs that forget the ENV var override and silently
//! talk to prod).

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

use serde::{Deserialize, Serialize};

/// Environment variable that overrides the persisted channel.
/// Used by CI / testdesign-style scenarios that want a one-shot
/// dev / local override without mutating the user's config file.
pub const ENV_VAR: &str = "SUPERDEDUPER_CHANNEL";

/// The three channels supported in v1. `staging` deferred per
/// dev-channel-spec.md §2.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    #[default]
    Prod,
    Dev,
    Local,
}

impl Channel {
    /// Stable lowercase string slug used in CLI args, env vars,
    /// config files, install.json filenames, and the GUI dropdown.
    /// **Never localise** — these are identifiers, not user-facing
    /// labels.
    pub fn as_slug(&self) -> &'static str {
        match self {
            Channel::Prod => "prod",
            Channel::Dev => "dev",
            Channel::Local => "local",
        }
    }

    /// Long-form description for the GUI Settings → Network dropdown
    /// per spec §5.4.
    pub fn description(&self) -> &'static str {
        match self {
            Channel::Prod => "Production. All submissions appear on the public leaderboard.",
            Channel::Dev => "Development. For testing only. Data is periodically reset.",
            Channel::Local => "Local development server. For sd developers only.",
        }
    }

    /// True for any channel that's NOT prod — drives the visibility
    /// surface (GUI banner + CLI footer line + `(channel: dev)` hints).
    pub fn is_non_prod(&self) -> bool {
        !matches!(self, Channel::Prod)
    }

    /// All channels, in canonical display order (prod first as the
    /// safe default).
    pub fn all() -> &'static [Channel] {
        &[Channel::Prod, Channel::Dev, Channel::Local]
    }
}

impl FromStr for Channel {
    type Err = ChannelParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "prod" => Ok(Channel::Prod),
            "dev" => Ok(Channel::Dev),
            "local" => Ok(Channel::Local),
            other => Err(ChannelParseError {
                input: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_slug())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelParseError {
    pub input: String,
}

impl std::fmt::Display for ChannelParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown channel {:?} — expected one of: prod, dev, local",
            self.input
        )
    }
}

impl std::error::Error for ChannelParseError {}

/// Map a channel to its public backend base URL. Per
/// dev-channel-spec.md §5.1.
///
/// The dev channel uses first-level subdomains
/// (`dev-api.superdeduper.io`) — locked 2026-05-24 because Cloudflare
/// Free-plan Universal SSL only covers the first level of the
/// wildcard. Don't move dev under `*.dev.superdeduper.io` without
/// also paying for Advanced Certificate Manager.
pub fn server_url_for(channel: Channel) -> &'static str {
    match channel {
        Channel::Prod => "https://api.superdeduper.io",
        Channel::Dev => "https://dev-api.superdeduper.io",
        Channel::Local => "http://localhost:3000",
    }
}

/// #58 — Map a channel to its public FRONTEND base URL (the user-
/// facing web site, distinct from the API backend). Used by every
/// GUI affordance that opens the browser at a sd web property:
/// "View on leaderboard", "Open profile", vanity-URL links, etc.
///
/// Without this, GUI buttons on a dev install navigate to
/// `superdeduper.io` (prod) where the install_id can't be found —
/// surfaced to the user as a 404.
///
/// Channel → frontend URL mapping mirrors the API URL pattern:
/// the wildcard-cert constraint that drove `dev-api.superdeduper.io`
/// doesn't apply to the frontend (which sits behind Cloudflare
/// Pages on the apex domain), so the frontend gets the more natural
/// `dev.superdeduper.io` form.
pub fn frontend_url_for(channel: Channel) -> &'static str {
    match channel {
        Channel::Prod => "https://superdeduper.io",
        Channel::Dev => "https://dev.superdeduper.io",
        Channel::Local => "http://localhost:5173",
    }
}

/// Default cap path for the user-facing TOML config file holding
/// the persisted channel (and any future `[network]` settings).
///
/// On Linux this lives at `$XDG_CONFIG_HOME/superdeduper/config.toml`
/// (`~/.config/superdeduper/config.toml`); on macOS at
/// `~/Library/Application Support/superdeduper/config.toml`; on
/// Windows at `%APPDATA%\superdeduper\config.toml`.
///
/// Distinct from the install-state directory (which holds the
/// per-channel `install.{channel}.json` files) — the config file
/// is user-editable preferences; install state is opaque
/// crypto material.
pub fn config_file_path() -> io::Result<PathBuf> {
    let mut p = config_dir()?;
    p.push("config.toml");
    Ok(p)
}

#[cfg(target_os = "linux")]
fn config_dir() -> io::Result<PathBuf> {
    if let Some(xdg) = env::var_os("XDG_CONFIG_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("superdeduper");
        return Ok(p);
    }
    let home = env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push(".config");
    p.push("superdeduper");
    Ok(p)
}

#[cfg(target_os = "macos")]
fn config_dir() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push("Library");
    p.push("Application Support");
    p.push("superdeduper");
    Ok(p)
}

#[cfg(windows)]
fn config_dir() -> io::Result<PathBuf> {
    let appdata = env::var_os("APPDATA")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "APPDATA not set"))?;
    let mut p = PathBuf::from(appdata);
    p.push("superdeduper");
    Ok(p)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn config_dir() -> io::Result<PathBuf> {
    let home = env::var_os("HOME")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push(".superdeduper");
    Ok(p)
}

/// On-disk shape of the user config TOML. Sub-section under
/// `[network]` per spec §5.1 so future network knobs (proxy,
/// timeouts) can land alongside `channel` without bumping the schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PersistedConfig {
    #[serde(default)]
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default)]
    pub channel: Option<String>,
}

/// Read the persisted channel from disk. Returns `Ok(None)` if the
/// config file doesn't exist (fresh install — defaults apply) or
/// if the `[network] channel` key isn't set. Returns `Err` if the
/// file exists but couldn't be parsed (caller surfaces to user;
/// don't silently fall back, since that masks misconfigurations).
pub fn read_persisted_channel() -> io::Result<Option<Channel>> {
    let path = match config_file_path() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let text = std::str::from_utf8(&bytes).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("config.toml utf8: {e}"))
    })?;
    let parsed: PersistedConfig = toml::from_str(text).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.toml parse: {e}"),
        )
    })?;
    let Some(slug) = parsed.network.channel else {
        return Ok(None);
    };
    match Channel::from_str(&slug) {
        Ok(c) => Ok(Some(c)),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
    }
}

/// Persist a channel to the user config TOML. Creates the parent
/// directory if missing. Atomic write via tmp + rename.
pub fn write_persisted_channel(channel: Channel) -> io::Result<()> {
    let path = config_file_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    // Round-trip via the typed shape so we preserve any future
    // sibling keys (proxy etc.) that we don't know about yet.
    let mut current = match fs::read_to_string(&path) {
        Ok(s) => toml::from_str::<PersistedConfig>(&s).unwrap_or_default(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => PersistedConfig::default(),
        Err(e) => return Err(e),
    };
    current.network.channel = Some(channel.as_slug().to_string());
    let serialised = toml::to_string_pretty(&current).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("config.toml encode: {e}"),
        )
    })?;
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, serialised.as_bytes())?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the channel from the env var. Returns `Ok(None)` if unset;
/// `Err` if set to an invalid value (so a typo doesn't silently fall
/// back to prod).
pub fn read_env_channel() -> io::Result<Option<Channel>> {
    let Some(raw) = env::var_os(ENV_VAR) else {
        return Ok(None);
    };
    let Some(s) = raw.to_str() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{ENV_VAR} contains invalid UTF-8"),
        ));
    };
    if s.is_empty() {
        return Ok(None);
    }
    Channel::from_str(s)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Process-global active channel — written once at startup by
/// `main.rs` (after parsing the `--channel` CLI flag + walking the
/// precedence chain), read by every consumer (telemetry submit,
/// install.json loader, GUI banner, CLI footer) via
/// [`active_channel`]. Using an atomic instead of an OnceLock keeps
/// the API const-friendly + lets tests reset it between cases
/// without a `Mutex`. Encoded as `u8`: 0=prod, 1=dev, 2=local,
/// 255=unset (treated as prod default).
static ACTIVE_CHANNEL: AtomicU8 = AtomicU8::new(0xff);

/// Set the process-global active channel. Called by `main.rs` once
/// after the CLI + ENV + config precedence chain has resolved.
/// Subsequent calls overwrite the previous value (the only caller
/// that legitimately does this is the GUI's Settings panel when the
/// user switches channels mid-session — and only with the user's
/// explicit confirmation via the channel-switch modal).
pub fn set_active_channel(channel: Channel) {
    let byte = match channel {
        Channel::Prod => 0,
        Channel::Dev => 1,
        Channel::Local => 2,
    };
    ACTIVE_CHANNEL.store(byte, Ordering::Relaxed);
}

/// Read the process-global active channel. Returns `Channel::Prod`
/// if `set_active_channel` hasn't been called yet — the safe
/// default that matches §5.1's "fresh install talks to prod."
pub fn active_channel() -> Channel {
    match ACTIVE_CHANNEL.load(Ordering::Relaxed) {
        1 => Channel::Dev,
        2 => Channel::Local,
        _ => Channel::Prod,
    }
}

/// Apply the precedence chain (CLI > ENV > config > default) and
/// return the active channel. `cli_override` is `Some(slug)` when
/// the user passed `--channel`; `None` otherwise.
///
/// Errors are surfaced (not swallowed) so a typo in `--channel`,
/// `$SUPERDEDUPER_CHANNEL`, or `config.toml` produces a clear
/// "you said X, X isn't valid" message instead of a silent fall-back
/// to prod (which would be exactly the wrong default if the user
/// was trying to avoid prod).
pub fn resolve_active_channel(cli_override: Option<&str>) -> io::Result<Channel> {
    if let Some(s) = cli_override {
        return Channel::from_str(s)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()));
    }
    if let Some(c) = read_env_channel()? {
        return Ok(c);
    }
    if let Some(c) = read_persisted_channel()? {
        return Ok(c);
    }
    Ok(Channel::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_default_is_prod() {
        assert_eq!(Channel::default(), Channel::Prod);
    }

    #[test]
    fn from_str_recognises_canonical_slugs() {
        assert_eq!(Channel::from_str("prod").unwrap(), Channel::Prod);
        assert_eq!(Channel::from_str("dev").unwrap(), Channel::Dev);
        assert_eq!(Channel::from_str("local").unwrap(), Channel::Local);
    }

    #[test]
    fn from_str_is_case_insensitive_and_trims() {
        assert_eq!(Channel::from_str("PROD").unwrap(), Channel::Prod);
        assert_eq!(Channel::from_str("  Dev  ").unwrap(), Channel::Dev);
        assert_eq!(Channel::from_str("LOCAL").unwrap(), Channel::Local);
    }

    #[test]
    fn from_str_rejects_unknown() {
        let err = Channel::from_str("staging").unwrap_err();
        assert!(err.to_string().contains("staging"));
        assert!(err.to_string().contains("prod"));
    }

    #[test]
    fn server_url_for_matches_spec_5_1() {
        assert_eq!(server_url_for(Channel::Prod), "https://api.superdeduper.io");
        assert_eq!(
            server_url_for(Channel::Dev),
            "https://dev-api.superdeduper.io"
        );
        assert_eq!(server_url_for(Channel::Local), "http://localhost:3000");
    }

    #[test]
    fn is_non_prod_only_true_for_dev_and_local() {
        assert!(!Channel::Prod.is_non_prod());
        assert!(Channel::Dev.is_non_prod());
        assert!(Channel::Local.is_non_prod());
    }

    #[test]
    fn slug_round_trips_through_from_str() {
        for &c in Channel::all() {
            assert_eq!(Channel::from_str(c.as_slug()).unwrap(), c);
        }
    }

    #[test]
    fn resolve_cli_override_wins() {
        // Even when ENV var would set something else, --channel wins.
        // (We can't sanely test the env-var-set case here without
        // sequencing test threads, so we test the cli-override branch
        // in isolation.)
        assert_eq!(resolve_active_channel(Some("dev")).unwrap(), Channel::Dev);
        assert_eq!(
            resolve_active_channel(Some("local")).unwrap(),
            Channel::Local
        );
    }

    #[test]
    fn resolve_cli_override_rejects_invalid() {
        let err = resolve_active_channel(Some("staging")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn persisted_config_round_trips() {
        let cfg = PersistedConfig {
            network: NetworkConfig {
                channel: Some("dev".into()),
            },
        };
        let s = toml::to_string_pretty(&cfg).unwrap();
        let back: PersistedConfig = toml::from_str(&s).unwrap();
        assert_eq!(back.network.channel.as_deref(), Some("dev"));
    }

    #[test]
    fn persisted_config_missing_network_uses_default() {
        // Pre-channel config files (empty or with other sections)
        // must still load — give defaults for missing `[network]`.
        let s = "";
        let back: PersistedConfig = toml::from_str(s).unwrap();
        assert!(back.network.channel.is_none());
    }

    #[test]
    fn channel_descriptions_are_non_empty_and_distinct() {
        let descs: Vec<&'static str> = Channel::all().iter().map(|c| c.description()).collect();
        for d in &descs {
            assert!(!d.is_empty());
        }
        let mut sorted = descs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), descs.len(), "descriptions must be distinct");
    }
}
