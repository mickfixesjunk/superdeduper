//! Hermetic test for the channel-resolution precedence layer
//! (#30 — #13's deferred layer-3 AC).
//!
//! Per dev-channel-spec.md §3.3, channel resolution order is:
//!   1. CLI flag `--channel` (highest)
//!   2. ENV var `SUPERDEDUPER_CHANNEL`
//!   3. Persisted config `~/.config/superdeduper/config.toml`
//!      `[network] channel`
//!   4. Default: `prod` (lowest)
//!
//! testrunner's #13 first-pass verified 1, 2, and 4. Layer 3 was
//! deferred because testrunner doesn't write persistent config state
//! into the test box without a hermetic teardown harness. This test
//! pins layer 3 at the lib level — runs entirely in tempdir-rooted
//! state, never touches the real user config.
//!
//! Linux-only for now: macOS config_dir() ignores XDG_CONFIG_HOME +
//! Windows uses APPDATA; pinning both would need per-platform shims.
//! Linux is the testrunner platform that flagged the gap.

#![cfg(target_os = "linux")]

use std::fs;
use std::sync::Mutex;

use superdeduper::channel::{resolve_active_channel, Channel, ENV_VAR};

/// Process-wide serial gate. Every test sets `XDG_CONFIG_HOME` +
/// `SUPERDEDUPER_CHANNEL` env vars to a per-test value, then reads
/// them back via `resolve_active_channel`. Cargo runs these tests in
/// parallel within the same binary, so without serialisation test A
/// can see test B's env state. Same pattern as
/// `tests/scan_resume_e2e.rs::SERIAL`.
///
/// PoisonError doesn't kill us — each test sets a fresh env on
/// entry, so a panicked predecessor leaves the world recoverable.
static SERIAL: Mutex<()> = Mutex::new(());

/// Per-test setup: a fresh tempdir for XDG_CONFIG_HOME, with the
/// env var pointed at it. Returns the TempDir handle that must
/// outlive the test (drop deletes the temp dir).
///
/// IMPORTANT: also CLEARS `SUPERDEDUPER_CHANNEL` so a prior test
/// can't leak its env into ours.
fn isolate(label: &str) -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix(&format!("sd-#30-{label}-"))
        .tempdir()
        .unwrap();
    // SAFETY: single-threaded test setup under SERIAL.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        std::env::remove_var(ENV_VAR);
    }
    dir
}

/// Write a config.toml with `[network] channel = <slug>` inside the
/// XDG_CONFIG_HOME tempdir. Returns the full path to the file.
fn write_config_with_channel(xdg: &tempfile::TempDir, slug: &str) -> std::path::PathBuf {
    let dir = xdg.path().join("superdeduper");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.toml");
    fs::write(
        &path,
        format!("[network]\nchannel = \"{slug}\"\n").as_bytes(),
    )
    .unwrap();
    path
}

#[test]
fn layer_3_config_toml_dev_channel_reads_dev() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("config-dev");
    write_config_with_channel(&_td, "dev");
    let resolved = resolve_active_channel(None).expect("resolve");
    assert_eq!(
        resolved,
        Channel::Dev,
        "config.toml with channel=dev should resolve Dev when no CLI/ENV override"
    );
}

#[test]
fn layer_3_config_toml_local_channel_reads_local() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("config-local");
    write_config_with_channel(&_td, "local");
    let resolved = resolve_active_channel(None).expect("resolve");
    assert_eq!(resolved, Channel::Local);
}

#[test]
fn cli_flag_overrides_config_toml() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("cli-vs-config");
    write_config_with_channel(&_td, "dev");
    // CLI flag = prod against config.toml = dev. CLI must win.
    let resolved = resolve_active_channel(Some("prod")).expect("resolve");
    assert_eq!(
        resolved,
        Channel::Prod,
        "CLI --channel=prod should override config.toml channel=dev"
    );
}

#[test]
fn env_var_overrides_config_toml() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("env-vs-config");
    write_config_with_channel(&_td, "dev");
    // SAFETY: single-threaded under SERIAL.
    unsafe { std::env::set_var(ENV_VAR, "prod") };
    let resolved = resolve_active_channel(None).expect("resolve");
    assert_eq!(
        resolved,
        Channel::Prod,
        "SUPERDEDUPER_CHANNEL=prod should override config.toml channel=dev"
    );
}

#[test]
fn cli_flag_overrides_env_var() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("cli-vs-env");
    // SAFETY: single-threaded under SERIAL.
    unsafe { std::env::set_var(ENV_VAR, "dev") };
    // CLI flag = local against env = dev. CLI must win.
    let resolved = resolve_active_channel(Some("local")).expect("resolve");
    assert_eq!(
        resolved,
        Channel::Local,
        "CLI --channel=local should override SUPERDEDUPER_CHANNEL=dev"
    );
}

#[test]
fn no_config_no_env_no_cli_defaults_to_prod() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("default");
    // No config file written, env cleared by isolate(), no CLI override.
    let resolved = resolve_active_channel(None).expect("resolve");
    assert_eq!(
        resolved,
        Channel::Prod,
        "with nothing set, the default-prod fallback should apply"
    );
}

#[test]
fn malformed_config_toml_surfaces_error_does_not_silently_fall_back() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("malformed");
    let dir = _td.path().join("superdeduper");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("config.toml"), b"not valid toml at all = [").unwrap();
    let result = resolve_active_channel(None);
    assert!(
        result.is_err(),
        "malformed config.toml must surface an error (silent fall-back would mask misconfig)"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("config.toml"),
        "error message should name the source ({err})"
    );
}

#[test]
fn unknown_channel_slug_in_config_toml_surfaces_error() {
    let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let _td = isolate("unknown-slug");
    write_config_with_channel(&_td, "staging");
    let result = resolve_active_channel(None);
    assert!(
        result.is_err(),
        "unknown channel slug must surface an error, not fall back to default"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("staging"),
        "error message should name the rejected slug ({err})"
    );
}
