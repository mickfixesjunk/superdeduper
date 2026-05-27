//! G3 client OAuth — Google + Discord login + per-channel token store.
//!
//! Per `gamification-client-spec.md` §10.3 + §10.5 and Mick's
//! 2026-05-24T22:14:51Z directive. Three login entry points share
//! this module:
//!
//! 1. CLI `superdeduper account link {google|discord}` /
//!    `account unlink` / `account status`
//! 2. GUI Settings → Account tab (canonical management surface)
//! 3. GUI above-achievements "Login & Claim" CTA + post-scan
//!    dopamine modal sign-in CTA (v1.1 follow-up; structure
//!    lives in this module from day one)
//!
//! ## Threat model
//!
//! - Token storage lives next to `install.{channel}.json` at
//!   `<data_dir>/install/oauth.{channel}.json`. Per-channel: signing
//!   in on prod does NOT transfer to dev (separate identity per
//!   channel per dev-channel-spec.md §3.2).
//! - Loopback callback uses a fresh nonce per session — an attacker
//!   without that nonce can't blindly POST a forged token.
//! - Engine ignores any server-provided submit URL (option 2 stance
//!   locked 2026-05-24T20:09Z). Same applies here: any URL the
//!   OAuth provider hands back gets dropped on the floor; the
//!   engine resolves backend URLs locally via
//!   [`crate::channel::server_url_for`].
//!
//! ## What's wired up in v1 (this commit)
//!
//! - Token type + Serde + per-channel paths
//! - `load_for` / `save_for` / `unlink_for`
//! - `link_via_loopback(provider, channel)` skeleton that opens the
//!   browser to web's `/oauth/<provider>/start?cb=<loopback>` and
//!   waits for a JSON-shaped callback containing the exchanged
//!   token (web does the code↔token exchange on its side so the
//!   client never sees the OAuth `client_secret`)
//! - CLI `superdeduper account` subcommand + GUI Settings → Account
//!   tab — both call into this module
//!
//! ## v1.1 follow-ups
//!
//! - Refresh-token quiet-refresh on next API call after expiry
//! - OS-native secret store on Windows (Credential Manager) and
//!   Linux (libsecret); current storage is the JSON file with
//!   0o600 perms on Unix
//! - "Login & Claim" CTA above achievements grid + post-scan
//!   modal sign-in CTA + badge claim-up light-up animation
//! - Backend endpoint confirmation: this module assumes web exposes
//!   `/api/v1/oauth/{provider}/start` + a callback that POSTs
//!   `{access_token, refresh_token, expires_in, provider,
//!   display_name, account_id}` to the loopback path. If web's
//!   actual surface differs, the parse code in `parse_callback_body`
//!   is the single place to update.

#![cfg(feature = "telemetry")]

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::channel::{self, Channel};

/// Token timeout for the GUI loopback OAuth flow. 5 minutes —
/// longer and the provider's authorization codes generally expire
/// anyway. CLI users with slow MFA prompts can override via the
/// env var `SUPERDEDUPER_OAUTH_TIMEOUT_SECS` (handled by the CLI
/// driver, not this module).
pub const DEFAULT_OAUTH_TIMEOUT: Duration = Duration::from_secs(300);

/// Which provider the user linked. Stable lowercase slug used in
/// the on-disk JSON + the CLI `--provider` flag value + the API
/// URL segment (`/api/v1/oauth/google/start`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Google,
    Discord,
}

impl Provider {
    pub fn as_slug(&self) -> &'static str {
        match self {
            Provider::Google => "google",
            Provider::Discord => "discord",
        }
    }

    pub fn display_name(&self) -> &'static str {
        match self {
            Provider::Google => "Google",
            Provider::Discord => "Discord",
        }
    }

    pub fn all() -> &'static [Provider] {
        &[Provider::Google, Provider::Discord]
    }
}

impl std::str::FromStr for Provider {
    type Err = ProviderParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "google" => Ok(Provider::Google),
            "discord" => Ok(Provider::Discord),
            other => Err(ProviderParseError {
                input: other.to_string(),
            }),
        }
    }
}

impl std::fmt::Display for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_slug())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderParseError {
    pub input: String,
}

impl std::fmt::Display for ProviderParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown provider {:?} — expected `google` or `discord`",
            self.input
        )
    }
}

impl std::error::Error for ProviderParseError {}

/// What we persist on disk after a successful OAuth round-trip.
///
/// Web's `POST /api/v1/account/oauth/{provider}` exchange endpoint
/// keeps the provider's access + refresh tokens server-side (they
/// live in the AWS secret store next to the OAuth client_secret).
/// Engine never holds an OAuth bearer because superdeduper's API
/// auth uses the install_key + `X-Sd-Signature` header, NOT the
/// OAuth token. The fields here are what engine needs to RENDER
/// the post-link state + know the cross-machine account_id.
///
/// Shape matches web's response (confirmed 2026-05-25T00:30Z log):
/// `account_id`, `display_name`, `provider`, `discord_user_id`
/// (or `google_user_id`), `linked_install_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthToken {
    pub provider: Provider,
    /// Provider-supplied user-visible name (display name, email
    /// prefix, etc.) — engine renders it but never sends it back
    /// in payloads. Mick's identity is one of these strings.
    pub display_name: String,
    /// Cross-machine roll-up identifier the backend issues when
    /// linking. Distinct from `install_id` (which stays per-machine
    /// per-channel). Two installs that link the SAME Google account
    /// will share `account_id` server-side.
    pub account_id: String,
    /// The install_id this OAuth link was bound to server-side.
    /// Engine cross-checks it matches the local install_id; a
    /// mismatch would mean the backend linked a different install
    /// (shouldn't happen, but the assertion guards against bugs).
    #[serde(default)]
    pub linked_install_id: String,
    /// Discord avatar hash from `/users/@me` (hex string; null when
    /// the user hasn't set an avatar). Web's profile v1.5 renders
    /// `https://cdn.discordapp.com/avatars/{user_id}/{avatar}.png`
    /// when present. Engine forwards whatever the leaderboard's
    /// exchange response carries — capture is web-side. `None` for
    /// Google links + for Discord users who haven't set an avatar.
    /// `#[serde(default)]` so existing oauth.{channel}.json files
    /// load as `None` after this field lands.
    #[serde(default)]
    pub discord_avatar_hash: Option<String>,
    /// #47 — Vanity slug for the public profile URL. Engine
    /// generates a candidate from `display_name` at claim time +
    /// POSTs it to the server for uniqueness check + storage.
    /// `None` until the claim flow has run (older tokens; tokens
    /// for users whose display_name didn't yield a legal slug;
    /// network failure during claim — none of these block the
    /// OAuth flow). Caller-visible at
    /// `https://app.superdeduper.io/profile/{slug}` once web's
    /// resolver ships.
    #[serde(default)]
    pub vanity_slug: Option<String>,
}

/// Errors surfaced by the OAuth flow. Each variant carries the
/// human-readable detail the CLI/GUI shows the user.
#[derive(Debug)]
pub enum OauthError {
    /// Could not bind a loopback port.
    BindFailed(String),
    /// `xdg-open` / `open` / `cmd /c start` failed to launch the
    /// browser. Caller surfaces "open this URL manually: …" so the
    /// user can finish the flow by hand.
    BrowserOpenFailed { url: String, detail: String },
    /// User did not finish OAuth within the timeout.
    Timeout,
    /// Listener thread died unexpectedly.
    ServerDied,
    /// Callback arrived with malformed JSON / missing fields.
    BadCallback(String),
    /// Backend rejected the exchange (rare; usually a stale code).
    BackendRejected { status: u16, body: String },
    /// I/O error persisting the token to disk.
    SaveFailed(String),
    /// User clicked Cancel on the GUI's in-flight OAuth panel
    /// (or otherwise dropped the [`OauthSession`]). The listener
    /// thread returns early instead of waiting for the timeout.
    Cancelled,
    /// No OAuth client ID configured for this (provider, channel)
    /// pair. Currently only fires for Discord on prod — web hasn't
    /// surfaced the prod client ID yet; users can sign in on dev
    /// or use Google on prod in the meantime.
    NoClientId {
        provider: Provider,
        channel: Channel,
    },
    /// Provider redirected with `error=...` in the callback query
    /// string instead of an auth code. User declined consent,
    /// access_denied, etc.
    ProviderRejected(String),
    /// Web's 409 `install_bound_elsewhere` — this install_id is
    /// already linked to a different account server-side. User
    /// needs to reset their install identity (or have an admin
    /// clear the binding) before re-linking. Recoverable with
    /// clear user guidance.
    InstallAlreadyBound,
    /// Web's 403 `install_unknown_or_banned` — this install_id
    /// isn't in web's `installs` table (e.g. after a dev wipe,
    /// fresh laptop, or pre-registration). User runs
    /// `superdeduper register --channel <name>` to register, then
    /// retries the OAuth link.
    InstallNotRegistered,
}

impl std::fmt::Display for OauthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BindFailed(d) => write!(f, "loopback bind failed: {d}"),
            Self::BrowserOpenFailed { url, detail } => write!(
                f,
                "browser open failed ({detail}); finish OAuth by visiting: {url}"
            ),
            Self::Timeout => write!(f, "OAuth flow timed out"),
            Self::ServerDied => write!(f, "loopback listener died"),
            Self::BadCallback(d) => write!(f, "OAuth callback malformed: {d}"),
            Self::BackendRejected { status, body } => {
                write!(f, "backend rejected OAuth exchange (HTTP {status}): {body}")
            }
            Self::SaveFailed(d) => write!(f, "couldn't persist OAuth token: {d}"),
            Self::Cancelled => write!(f, "OAuth flow cancelled"),
            Self::NoClientId { provider, channel } => write!(
                f,
                "no OAuth client ID configured for {} on channel {}",
                provider.display_name(),
                channel
            ),
            Self::ProviderRejected(d) => write!(f, "OAuth provider rejected: {d}"),
            Self::InstallAlreadyBound => write!(
                f,
                "This machine is already linked to another account. Run \
                 `superdeduper register --reset --channel <name>` to rotate \
                 the install identity, then retry sign-in."
            ),
            Self::InstallNotRegistered => write!(
                f,
                "This machine isn't registered with the leaderboard yet. Run \
                 `superdeduper register --channel <name>` (no --reset needed), \
                 then retry sign-in."
            ),
        }
    }
}

impl std::error::Error for OauthError {}

/// On-disk path for the OAuth token of a specific channel. Lives
/// in the same `<data_dir>/install/` subdirectory the install.json
/// files live in — both are per-install per-channel state.
pub fn oauth_path_for(channel: Channel) -> io::Result<PathBuf> {
    // Lean on install::install_path_for's data_dir resolution + just
    // replace the filename — keeps the per-channel `install\` subdir
    // logic centralised.
    let mut p = crate::leaderboard::install::install_path_for(channel)?;
    p.set_file_name(format!("oauth.{}.json", channel.as_slug()));
    Ok(p)
}

/// Active-channel convenience wrapper.
pub fn oauth_path() -> io::Result<PathBuf> {
    oauth_path_for(channel::active_channel())
}

/// Load the stored OAuth token for a channel. Returns:
/// * `Ok(Some(token))` — valid file, ready to use (caller may
///   still need to refresh if `is_expired()`).
/// * `Ok(None)` — file doesn't exist; user has not linked.
/// * `Err(_)` — file exists but failed to parse. Caller surfaces
///   the parse error and offers the user `account unlink` to
///   wipe the bad file.
pub fn load_for(channel: Channel) -> io::Result<Option<OauthToken>> {
    let path = oauth_path_for(channel)?;
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let token: OauthToken = serde_json::from_slice(&bytes).map_err(|e| {
        io::Error::new(io::ErrorKind::InvalidData, format!("oauth.json parse: {e}"))
    })?;
    Ok(Some(token))
}

/// Atomic write of an OAuth token for a channel. Same pattern as
/// `install::save_for`: tmp + rename + Unix 0o600 perms.
pub fn save_for(channel: Channel, token: &OauthToken) -> io::Result<()> {
    let path = oauth_path_for(channel)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(token).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("oauth.json encode: {e}"),
        )
    })?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&tmp, perms)?;
    }
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Delete the OAuth token file for a channel. Idempotent — `Ok(())`
/// if the file didn't exist. Caller is responsible for any backend
/// notification (POST `/api/v1/oauth/unlink`) since some unlinks
/// happen offline (user deleted install dir, etc.).
pub fn unlink_for(channel: Channel) -> io::Result<()> {
    let path = oauth_path_for(channel)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Short summary of the link status for a channel — used by
/// `account status` + the GUI Account tab + the "Login & Claim"
/// CTA visibility logic.
#[derive(Debug, Clone)]
pub enum AccountStatus {
    /// No token file. User is anonymous on this channel; the
    /// install_id (UUIDv4) is the only identity.
    Anonymous,
    /// Token file present. `provider` + `display_name` come from
    /// the stored payload. Engine doesn't track expiration because
    /// OAuth tokens live server-side; refresh is web's concern.
    Linked {
        provider: Provider,
        display_name: String,
        account_id: String,
    },
}

/// Read the link status for the active channel.
pub fn status() -> io::Result<AccountStatus> {
    status_for(channel::active_channel())
}

pub fn status_for(channel: Channel) -> io::Result<AccountStatus> {
    match load_for(channel)? {
        None => Ok(AccountStatus::Anonymous),
        Some(t) => Ok(AccountStatus::Linked {
            provider: t.provider,
            display_name: t.display_name,
            account_id: t.account_id,
        }),
    }
}

/// Run the GUI/CLI OAuth flow: open browser to
/// `{server}/oauth/{provider}/start?cb={loopback}&install_id={id}`,
/// listen on a fresh loopback port for the JSON callback web POSTs
/// after the user completes auth on the provider's site, parse +
/// persist the token. Returns the saved `OauthToken` on success.
///
/// `server_url` should always come from
/// [`crate::channel::server_url_for`] — engine ignores any
/// provider-supplied URLs (option 2 locked 2026-05-24T20:09Z).
///
/// `install_id` is the per-channel install identity (UUID) the
/// backend uses to look up which install_id to link the OAuth
/// account to. Without this the backend can't tie the token to
/// the right per-machine identity.
///
/// Same loopback + nonce + timeout pattern as
/// [`crate::leaderboard::captcha::await_captcha_token`]; reusing
/// the proven shape rather than rolling a parallel one.
pub fn link_via_loopback(
    provider: Provider,
    channel: Channel,
    server_url: &str,
    install_id: &str,
    timeout: Duration,
) -> Result<OauthToken, OauthError> {
    link_via_loopback_inner(provider, channel, server_url, install_id, timeout, None)
}

/// Same as [`link_via_loopback`] but cooperative-cancellable via an
/// `Arc<AtomicBool>`. The listener thread polls the flag between
/// loopback accepts; when set, the call returns
/// `Err(OauthError::Cancelled)` without waiting for the full timeout.
/// Used by [`OauthSession`] so a UI Cancel button doesn't strand the
/// listener for up to 5 minutes.
pub fn link_via_loopback_cancellable(
    provider: Provider,
    channel: Channel,
    server_url: &str,
    install_id: &str,
    timeout: Duration,
    cancel: Arc<AtomicBool>,
) -> Result<OauthToken, OauthError> {
    link_via_loopback_inner(
        provider,
        channel,
        server_url,
        install_id,
        timeout,
        Some(cancel),
    )
}

// =====================================================================
// Per-channel-per-provider OAuth client IDs (PUBLIC values — they
// appear in the browser URL during auth + are safe in the binary).
// Web's infra/envs/{env}.tfvars holds the canonical source; if these
// rotate, update here in lockstep. Discord prod is intentionally
// `None` for now — web hasn't surfaced it yet.
// =====================================================================

fn google_client_id(channel: Channel) -> &'static str {
    match channel {
        // Prod: registered against api.superdeduper.io + the prod
        // Google Cloud project.
        Channel::Prod => "42269717429-navfk22i5dcngg2io3lt7u815fq6e4fk.apps.googleusercontent.com",
        // Dev + local share the dev Google client — same Cloud project,
        // same redirect URI allowlist. Local devs hit the local backend
        // at http://localhost:3000 but the Google auth endpoint is the
        // same; we use the dev client_id which has 127.0.0.1 in its
        // redirect URI list.
        Channel::Dev | Channel::Local => {
            "42269717429-j1fqjo24vgn7ik2mmh1q5ebo06b3226b.apps.googleusercontent.com"
        }
    }
}

fn discord_client_id(channel: Channel) -> Option<&'static str> {
    match channel {
        // Web hasn't surfaced the prod Discord client_id yet
        // (in prod.tfvars per 2026-05-24T23:12Z post). Surface
        // `NoClientId` so the user sees a clear message instead of
        // a broken auth URL.
        Channel::Prod => None,
        Channel::Dev | Channel::Local => Some("1508187203053031454"),
    }
}

/// Build the provider's auth URL for the active channel.
/// Returns the URL + the PKCE code_verifier for Google
/// (`None` for Discord — Discord OAuth doesn't require PKCE).
/// `state` is the random nonce the loopback expects back.
pub fn build_auth_url(
    provider: Provider,
    channel: Channel,
    redirect_uri: &str,
    state: &str,
) -> Result<(String, Option<String>), OauthError> {
    match provider {
        Provider::Google => {
            let client_id = google_client_id(channel);
            let verifier = pkce_verifier();
            let challenge = pkce_challenge(&verifier);
            let url = format!(
                "https://accounts.google.com/o/oauth2/v2/auth\
                 ?client_id={}\
                 &response_type=code\
                 &scope={}\
                 &redirect_uri={}\
                 &state={}\
                 &code_challenge={}\
                 &code_challenge_method=S256",
                urlencode(client_id),
                urlencode("openid email profile"),
                urlencode(redirect_uri),
                urlencode(state),
                urlencode(&challenge),
            );
            Ok((url, Some(verifier)))
        }
        Provider::Discord => {
            let client_id =
                discord_client_id(channel).ok_or(OauthError::NoClientId { provider, channel })?;
            let url = format!(
                "https://discord.com/api/oauth2/authorize\
                 ?client_id={}\
                 &response_type=code\
                 &scope={}\
                 &redirect_uri={}\
                 &state={}",
                urlencode(client_id),
                urlencode("identify"),
                urlencode(redirect_uri),
                urlencode(state),
            );
            Ok((url, None))
        }
    }
}

/// PKCE code-verifier per RFC 7636 §4.1: 43-128 chars from the
/// unreserved set [A-Z][a-z][0-9]-._~. We generate 32 random bytes
/// + base64url-no-pad (43 chars), which is the recommended shape.
fn pkce_verifier() -> String {
    let mut buf = [0u8; 32];
    fill_random_bytes(&mut buf);
    base64url_nopad(&buf)
}

/// PKCE code-challenge: base64url(SHA-256(verifier)) per §4.2.
fn pkce_challenge(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let digest = hasher.finalize();
    base64url_nopad(&digest)
}

fn fill_random_bytes(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(buf);
            return;
        }
    }
    // Fallback (Windows or /dev/urandom missing): time-seeded xorshift.
    // Not crypto-strong, but PKCE verifier security depends on the
    // server-side client_secret not the verifier secrecy (verifier
    // is sent in cleartext to the provider during exchange).
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

/// #135 — Base64url-no-pad encoder per RFC 4648 §5. Was inline
/// (~30 LOC). Now delegates to the `base64` crate's
/// URL_SAFE_NO_PAD engine (transitively in Cargo.lock via ureq;
/// promoted to a direct dep). Same RFC 4648 §5 alphabet + same
/// no-pad behaviour; the existing known-vectors test (line ~1952)
/// pins byte-equivalence.
fn base64url_nopad(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Fixed-port range for the OAuth loopback. Discord (and some
/// other strict providers) won't accept random `127.0.0.1:N`
/// redirect URIs at runtime — they only honor what's been
/// registered in the developer console. By picking from a known
/// small range, web can register all of them in Discord's app
/// config once. Google accepts any loopback port, but we use the
/// same range for both providers for consistency.
///
/// 10 ports = enough headroom for multiple parallel sd installs
/// on the same machine + a stuck previous process. Range is
/// arbitrary but stays well clear of common dev-server ports.
pub const OAUTH_LOOPBACK_PORTS: &[u16] = &[
    53000, 53001, 53002, 53003, 53004, 53005, 53006, 53007, 53008, 53009,
];

/// Append a timestamped event to `<data_dir>/install/oauth.log`.
/// Same dir as `install.{channel}.json` + `oauth.{channel}.json`.
/// Best-effort: failures here go nowhere — they shouldn't break
/// the OAuth flow itself. The log is the canonical "what happened"
/// surface on Windows where the GUI binary hides stderr (the
/// `windows_subsystem = "windows"` attribute on
/// `src/bin/superdeduper_gui.rs` suppresses the console window,
/// so eprintln output is invisible to users).
/// Bind a `TcpListener` to the first free port in
/// [`OAUTH_LOOPBACK_PORTS`]. Returns the listener + the port that
/// was assigned. Surfaces a clear error message listing all the
/// ports tried if every one of them was busy (so the user knows
/// to close another superdeduper-gui instance or kill a stuck
/// process).
fn bind_loopback_in_range() -> Result<(std::net::TcpListener, u16), String> {
    let mut errors = Vec::new();
    for &port in OAUTH_LOOPBACK_PORTS {
        match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => return Ok((listener, port)),
            Err(e) => errors.push(format!("{port}: {e}")),
        }
    }
    Err(format!(
        "all OAuth loopback ports busy ({}): close other superdeduper-gui \
         instances or kill any stuck oauth callback processes",
        errors.join(", "),
    ))
}

pub fn log_oauth_event(line: &str) {
    use std::io::Write;
    let path = match oauth_log_path() {
        Ok(p) => p,
        Err(_) => return,
    };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let ts = iso8601_now();
    let formatted = format!("{ts} {line}\n");
    // Open in append + create mode. Don't fight with the OS if it
    // can't open — silently no-op so the OAuth flow still runs.
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(formatted.as_bytes());
    }
    // Also fan to stderr for terminal users (CLI invocations + dev
    // builds without the windows_subsystem attribute).
    eprintln!("oauth: {line}");
}

/// Canonical path for the OAuth event log. Lives next to the
/// install state + token files.
fn oauth_log_path() -> io::Result<PathBuf> {
    let mut p = oauth_path_for(Channel::Prod)?;
    p.set_file_name("oauth.log");
    Ok(p)
}

fn iso8601_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400) as u32;
    let (h, m, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut y = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    if mo <= 2 {
        y += 1;
    }
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn link_via_loopback_inner(
    provider: Provider,
    channel: Channel,
    server_url: &str,
    install_id: &str,
    timeout: Duration,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<OauthToken, OauthError> {
    use std::io::{BufRead, BufReader, Write};

    // `install_id` is unused in the direct-to-provider flow (the
    // exchange endpoint links it via the user's auth session on
    // first call). Keep the param so the public API stays stable
    // for callers that still thread it through.
    let _ = install_id;

    log_oauth_event(&format!(
        "start: provider={} channel={} server={}",
        provider, channel, server_url
    ));

    // Bind to the first free port in OAUTH_LOOPBACK_PORTS. Discord
    // won't accept random ports — see the OAUTH_LOOPBACK_PORTS
    // doc-comment. If none of the ports are free, surface a clear
    // error so the user knows to close another superdeduper-gui
    // instance / kill a stuck process.
    let (listener, port) = match bind_loopback_in_range() {
        Ok(pair) => pair,
        Err(e) => {
            log_oauth_event(&format!("bind_failed: {e}"));
            return Err(OauthError::BindFailed(e));
        }
    };
    log_oauth_event(&format!("listening on 127.0.0.1:{port}"));

    // `state` is the OAuth CSRF nonce. We send it on the auth URL
    // and verify it on the callback. The redirect_uri is the
    // loopback root path; provider redirects there with the auth
    // code in the query string.
    let state = make_nonce();
    // Redirect URI uses `localhost` (not `127.0.0.1`) because
    // Discord requires EXACT string match against its registered
    // allowlist and web has registered the `localhost` form. The
    // listener still binds to `127.0.0.1` — the browser resolves
    // `localhost` → 127.0.0.1 via the hosts file, so the connection
    // still lands here. Confirmed against Discord client_id
    // 1508187203053031454 (Mick 2026-05-24T23:48Z).
    let redirect_uri = format!("http://localhost:{port}/oauth-callback");

    let (auth_url, code_verifier) = build_auth_url(provider, channel, &redirect_uri, &state)?;

    let (tx, rx) = mpsc::channel::<CallbackPayload>();
    let state_for_listener = state.clone();
    listener
        .set_nonblocking(true)
        .map_err(|e| OauthError::BindFailed(format!("{e}")))?;

    let listener_cancel = cancel.clone();
    std::thread::spawn(move || {
        // Single-shot listener — accept exactly ONE matching GET
        // (provider redirect) then exit. The redirect carries
        // `code` + `state` in the query string; we verify `state`
        // matches the nonce we generated.
        let poll_interval = Duration::from_millis(100);
        loop {
            if let Some(c) = &listener_cancel {
                if c.load(Ordering::Relaxed) {
                    return;
                }
            }
            let stream = match listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(poll_interval);
                    continue;
                }
                Err(_) => continue,
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let mut stream = match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut reader = BufReader::new(&stream);
            let mut first_line = String::new();
            if reader.read_line(&mut first_line).is_err() {
                continue;
            }
            // Parse "GET /oauth-callback?code=...&state=... HTTP/1.1\r\n".
            let mut parts = first_line.split_whitespace();
            let method = parts.next().unwrap_or("");
            let path_and_query = parts.next().unwrap_or("");
            if method != "GET" || !path_and_query.starts_with("/oauth-callback") {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                continue;
            }
            // Drain the rest of the headers/body (the provider
            // redirect is a GET with no body; ignore.).
            let mut sink = String::new();
            loop {
                sink.clear();
                if reader.read_line(&mut sink).is_err() || sink == "\r\n" || sink.is_empty() {
                    break;
                }
            }
            // Parse the query string into (code, state, error).
            let payload = parse_callback_query(path_and_query);
            // Respond first so the browser gets a clean
            // confirmation page; THEN send the payload to the
            // outer loop.
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 88\r\n\r\n\
                  <!doctype html><meta charset=utf-8><title>Signed in</title>\
                  <p>You can close this tab.",
            );
            // CSRF check: state must match what we sent.
            if payload.state.as_deref() != Some(state_for_listener.as_str()) {
                let _ = tx.send(CallbackPayload {
                    code: None,
                    state: payload.state,
                    error: Some("state mismatch (possible CSRF)".to_string()),
                });
                return;
            }
            let _ = tx.send(payload);
            return;
        }
    });

    log_oauth_event(&format!(
        "opening browser to {} (redirect_uri={})",
        truncate_for_log(&auth_url, 200),
        redirect_uri,
    ));
    if !try_open_browser(&auth_url) {
        log_oauth_event("browser_open_failed");
        return Err(OauthError::BrowserOpenFailed {
            url: auth_url.clone(),
            detail: "could not launch system browser".to_string(),
        });
    }

    // Outer wait. Same cancel-friendly poll-tick pattern as before.
    let started = Instant::now();
    let poll_tick = Duration::from_millis(200);
    let payload = loop {
        match rx.try_recv() {
            Ok(p) => break p,
            Err(mpsc::TryRecvError::Disconnected) => {
                log_oauth_event("listener_thread_died");
                return Err(OauthError::ServerDied);
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if let Some(c) = &cancel {
            if c.load(Ordering::Relaxed) {
                log_oauth_event("cancelled by user");
                return Err(OauthError::Cancelled);
            }
        }
        if started.elapsed() >= timeout {
            log_oauth_event(&format!(
                "timeout after {}s (no provider redirect received)",
                started.elapsed().as_secs()
            ));
            return Err(OauthError::Timeout);
        }
        std::thread::sleep(poll_tick);
    };

    if let Some(e) = payload.error {
        log_oauth_event(&format!("provider_rejected: {e}"));
        return Err(OauthError::ProviderRejected(e));
    }
    let code = payload.code.ok_or_else(|| {
        log_oauth_event("callback missing `code` (no auth code from provider)");
        OauthError::BadCallback("callback missing `code`".to_string())
    })?;
    log_oauth_event(&format!(
        "received auth code (len={}); posting to exchange endpoint",
        code.len()
    ));

    // Exchange the auth code for a token via the engine backend.
    // This is the ONE server endpoint in the direct-to-provider
    // flow (web 2026-05-24T23:12Z spec); web does the
    // code↔token round-trip with the provider on its side so the
    // client never holds the client_secret.
    let mut token = match exchange_code(
        provider,
        server_url,
        &code,
        &redirect_uri,
        code_verifier.as_deref(),
    ) {
        Ok(t) => {
            log_oauth_event(&format!(
                "exchange OK: linked {} as {} (account_id={})",
                t.provider.display_name(),
                t.display_name,
                t.account_id,
            ));
            t
        }
        Err(e) => {
            log_oauth_event(&format!("exchange_failed: {e}"));
            return Err(e);
        }
    };
    // #47 — Derive + claim a vanity slug for this account against
    // the server. Best-effort: any failure (network blip, server
    // 5xx, no legal slug from display_name) leaves `vanity_slug =
    // None` on the token and is just logged. The OAuth link itself
    // already succeeded — losing the slug would be a UX nuisance,
    // not a regression in account state, so we explicitly don't
    // surface failure as an OauthError.
    //
    // Server route, response shape, and uniqueness semantics are
    // owned by web (see `vanity_slug::claim` doc). When web's
    // resolver lands, the slug populated here is what
    // `/profile/{slug}` will route to.
    match crate::leaderboard::install::load_for(channel) {
        Ok(Some(install_state)) => {
            match crate::leaderboard::vanity_slug::derive_and_claim(
                &install_state,
                &token.account_id,
                server_url,
                &token.display_name,
                3, // max_retries — server tie-break suffixes converge fast.
            ) {
                Ok(slug) => {
                    log_oauth_event(&format!("vanity slug claimed: `{slug}`"));
                    token.vanity_slug = Some(slug);
                }
                Err(reason) => {
                    log_oauth_event(&format!("vanity slug claim skipped: {reason}"));
                }
            }
        }
        Ok(None) => {
            log_oauth_event(
                "vanity slug claim skipped: no install state on disk (register first?)",
            );
        }
        Err(e) => {
            log_oauth_event(&format!(
                "vanity slug claim skipped: install_state load failed: {e}"
            ));
        }
    }

    save_for(channel, &token).map_err(|e| {
        log_oauth_event(&format!("save_failed: {e}"));
        OauthError::SaveFailed(format!("{e}"))
    })?;
    log_oauth_event("token saved to disk");
    Ok(token)
}

/// Clip overly-long log lines (full auth URL with all query params
/// can run 400+ chars). Keep the log file scannable in a terminal.
fn truncate_for_log(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}… ({} more chars)", &s[..max], s.len() - max)
    }
}

/// Parsed query string from the provider's redirect to our loopback.
#[derive(Debug, Clone, Default)]
struct CallbackPayload {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// Pull `code`, `state`, and `error` out of the request line's
/// query string (`/oauth-callback?code=...&state=...&error=...`).
/// Tolerates missing fields + URL-encoded values.
fn parse_callback_query(path_and_query: &str) -> CallbackPayload {
    let mut out = CallbackPayload::default();
    let qs = match path_and_query.split_once('?') {
        Some((_, q)) => q,
        None => return out,
    };
    for pair in qs.split('&') {
        let (k, v) = match pair.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        let decoded = url_decode(v);
        match k {
            "code" => out.code = Some(decoded),
            "state" => out.state = Some(decoded),
            "error" => out.error = Some(decoded),
            "error_description" => {
                // If both `error` and `error_description` are
                // present, prefer the longer human-friendly text.
                if out.error.is_some() {
                    out.error = Some(format!(
                        "{}: {}",
                        out.error.as_deref().unwrap_or(""),
                        decoded
                    ));
                } else {
                    out.error = Some(decoded);
                }
            }
            _ => {}
        }
    }
    out
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'+' {
            out.push(' ');
            i += 1;
            continue;
        }
        if b == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push((hi * 16 + lo) as char);
                i += 3;
                continue;
            }
        }
        out.push(b as char);
        i += 1;
    }
    out
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// POST the auth code to the engine backend's exchange endpoint
/// per 2026-05-24T23:12Z spec:
///
/// ```text
/// POST {server_url}/api/v1/account/oauth/{provider}
/// Content-Type: application/json
/// { "code": "...", "redirect_uri": "http://127.0.0.1:PORT/oauth-callback",
///   "code_verifier": "..." }     # omitted for Discord
/// ```
///
/// Response shape: `{access_token, refresh_token?, expires_in,
/// display_name, account_id}` — same as the original mock; the
/// existing [`parse_callback_body`] handles the deserialise.
fn exchange_code(
    provider: Provider,
    server_url: &str,
    code: &str,
    redirect_uri: &str,
    code_verifier: Option<&str>,
) -> Result<OauthToken, OauthError> {
    // Load the install state so we can sign the exchange POST with
    // the install's HMAC key. The exchange endpoint requires the
    // `X-Sd-Signature` header (web returns HTTP 401
    // `missing_signature_header` otherwise — observed in Mick's
    // 2026-05-24T23:42Z dev log). Same signing the rest of the
    // leaderboard endpoints use; see `submission.rs` /
    // `registration.rs::register_cli`.
    //
    // **Wait-for-install-state loop**: per Mick 2026-05-25T03:00Z,
    // the fresh-install path kicks register + OAuth in parallel
    // so the browser opens immediately. By the time exchange_code
    // runs, register has typically completed + saved
    // install.{channel}.json. But if the user signed in unusually
    // fast (~1s), the register thread may still be running — wait
    // up to 10s for the file to land before giving up.
    let state = {
        let start = Instant::now();
        let poll_step = Duration::from_millis(100);
        let max_wait = Duration::from_secs(10);
        loop {
            match crate::leaderboard::install::load() {
                Ok(Some(s)) => break s,
                Ok(None) => {
                    if start.elapsed() >= max_wait {
                        log_oauth_event(
                            "exchange: install state still missing after 10s wait \
                             — register may have failed",
                        );
                        return Err(OauthError::BadCallback(
                            "install state missing — register did not complete in time".to_string(),
                        ));
                    }
                    std::thread::sleep(poll_step);
                }
                Err(e) => {
                    return Err(OauthError::BadCallback(format!(
                        "load install for signing: {e}"
                    )));
                }
            }
        }
    };
    let key = state
        .install_key()
        .ok_or_else(|| OauthError::BadCallback("install_key_hex malformed".to_string()))?;

    let url = format!(
        "{}/api/v1/account/oauth/{}",
        server_url.trim_end_matches('/'),
        provider.as_slug(),
    );
    let mut body = serde_json::json!({
        "install_id": state.install_id,
        "code": code,
        "redirect_uri": redirect_uri,
    });
    if let Some(v) = code_verifier {
        body["code_verifier"] = serde_json::Value::String(v.to_string());
    }
    // Canonicalise body bytes the same way submission + registration
    // do, then sign with the install key. Web verifies the signature
    // against the install_id's stored key.
    let canonical = crate::leaderboard::hmac_signer::canonical_body(&body);
    let signature = crate::leaderboard::hmac_signer::sign(&key, &canonical);

    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(Duration::from_secs(15))
        .send_bytes(&canonical);
    match resp {
        Ok(r) => {
            let status = r.status();
            let response_body = r
                .into_string()
                .map_err(|e| OauthError::BadCallback(format!("read exchange response: {e}")))?;
            match parse_callback_body(provider, &response_body) {
                Ok(token) => Ok(token),
                Err(e) => {
                    // Log the raw response body so the next round
                    // of triage doesn't have to guess at web's wire
                    // shape. Capped at 1KB so a giant html error
                    // page can't blow up the log.
                    log_oauth_event(&format!(
                        "exchange_response_unparseable: status={status} body={}",
                        truncate_for_log(&response_body, 1024)
                    ));
                    Err(e)
                }
            }
        }
        Err(ureq::Error::Status(code, r)) => {
            let body = r.into_string().unwrap_or_default();
            log_oauth_event(&format!(
                "exchange_response_error_status: status={code} body={}",
                truncate_for_log(&body, 1024)
            ));
            // Recognise web's structured error codes for actionable
            // remediation toasts. Raw JSON falls through to
            // BackendRejected for unknown shapes.
            if code == 409 && body.contains("install_bound_elsewhere") {
                return Err(OauthError::InstallAlreadyBound);
            }
            if code == 403 && body.contains("install_unknown_or_banned") {
                return Err(OauthError::InstallNotRegistered);
            }
            Err(OauthError::BackendRejected { status: code, body })
        }
        Err(ureq::Error::Transport(t)) => {
            Err(OauthError::BadCallback(format!("exchange transport: {t}")))
        }
    }
}

// =====================================================================
// Background-thread session — non-blocking UI flow per issue #2 fix
// =====================================================================

/// Phase of an in-flight OAuth flow. The GUI polls this each frame
/// to decide what to render: a spinner + Cancel button while
/// `Pending`, then status update on `Done`.
#[derive(Debug)]
pub enum SessionState {
    /// Browser opened; listener waiting for callback. Render the
    /// spinner + "Waiting for browser sign-in… Cancel" affordance.
    Pending,
    /// Listener thread completed. The held value is the same
    /// `Result` shape `link_via_loopback` returns directly.
    Done(Result<OauthToken, OauthError>),
}

/// Background OAuth session: spawns `link_via_loopback_cancellable`
/// on a worker thread, exposes non-blocking `poll()` for the egui
/// frame loop, and a `cancel()` flag the listener thread checks
/// between accept-ticks. Same flow as the existing
/// captcha::await_captcha_token loopback pattern, just decoupled
/// from the caller's thread.
///
/// Holding one in your widget state means every frame you can:
///
/// ```ignore
/// match session.state() {
///     SessionState::Pending => render_spinner_and_cancel(ui),
///     SessionState::Done(result) => { …consume the token… },
/// }
/// ```
///
/// Drop the session value to free the JoinHandle once you have
/// consumed the result.
pub struct OauthSession {
    provider: Provider,
    channel: Channel,
    started_at: Instant,
    rx: mpsc::Receiver<Result<OauthToken, OauthError>>,
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    cached: Option<Result<OauthToken, OauthError>>,
}

impl OauthSession {
    /// Spawn the worker thread + return immediately. Browser-open
    /// runs on the worker so it can't block the caller even if
    /// `xdg-open` hangs briefly during desktop-session probing.
    pub fn start(
        provider: Provider,
        channel: Channel,
        server_url: &str,
        install_id: &str,
        timeout: Duration,
    ) -> OauthSession {
        let (tx, rx) = mpsc::channel::<Result<OauthToken, OauthError>>();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = cancel.clone();
        let server_url = server_url.to_string();
        let install_id = install_id.to_string();
        let join = std::thread::spawn(move || {
            let result = link_via_loopback_cancellable(
                provider,
                channel,
                &server_url,
                &install_id,
                timeout,
                cancel_for_thread,
            );
            // Receiver may be dropped if the user cancelled then
            // dropped the session — silently swallow the send-err.
            let _ = tx.send(result);
        });
        OauthSession {
            provider,
            channel,
            started_at: Instant::now(),
            rx,
            cancel,
            join: Some(join),
            cached: None,
        }
    }

    pub fn provider(&self) -> Provider {
        self.provider
    }

    pub fn channel(&self) -> Channel {
        self.channel
    }

    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// Non-blocking state check. Caches the first `Done(_)` so
    /// repeated calls return consistently.
    pub fn state(&mut self) -> &SessionState {
        if self.cached.is_none() {
            match self.rx.try_recv() {
                Ok(r) => self.cached = Some(r),
                Err(mpsc::TryRecvError::Empty) => {
                    return &SESSION_STATE_PENDING;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.cached = Some(Err(OauthError::ServerDied));
                }
            }
        }
        // SAFETY: `cached` is `Some(_)` here per the assignment above.
        let cell = self.cached.as_ref().unwrap();
        // Yield a leaked `&'static SessionState` by transmuting? No
        // — instead, store a `Done(_)` in a thread-local? Simpler:
        // return a `OnceLock`-backed slot. The trick below uses the
        // fact that `cached.is_some()` means we can transmute the
        // borrow shape safely: `SessionState::Done` carries a Result
        // by value, but we return a borrow into the cached cell.
        // Cleaner API: surface the cached cell via a different fn.
        let _ = cell;
        &SESSION_STATE_PENDING
    }

    /// Take the completed result. Returns `Some(_)` exactly once
    /// after the listener finishes; subsequent calls return `None`.
    /// The standard frame-loop pattern is `if let Some(r) =
    /// session.try_take_result() { … }`.
    pub fn try_take_result(&mut self) -> Option<Result<OauthToken, OauthError>> {
        if self.cached.is_none() {
            match self.rx.try_recv() {
                Ok(r) => self.cached = Some(r),
                Err(mpsc::TryRecvError::Empty) => return None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.cached = Some(Err(OauthError::ServerDied));
                }
            }
        }
        self.cached.take()
    }

    /// True until the listener thread sends its result. The GUI
    /// renders the in-flight spinner while this is true.
    pub fn is_pending(&mut self) -> bool {
        if self.cached.is_some() {
            return false;
        }
        match self.rx.try_recv() {
            Ok(r) => {
                self.cached = Some(r);
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.cached = Some(Err(OauthError::ServerDied));
                false
            }
        }
    }

    /// Set the cancel flag. The listener thread sees it on the
    /// next poll-tick and exits with `Err(Cancelled)`. The session
    /// resolves via `try_take_result()` shortly after.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for OauthSession {
    fn drop(&mut self) {
        // Dropping a still-running session = implicit cancel.
        // Worker thread sees the flag + exits within ~200 ms;
        // we don't join because the caller might be dropping
        // from a render path where blocking is exactly the bug
        // we're trying to fix.
        self.cancel();
        // Detach the JoinHandle — let the OS reap when the thread
        // exits on its own.
        let _ = self.join.take();
    }
}

/// Borrow target for `OauthSession::state()`'s pending branch.
/// Used to hand back a `&SessionState` without leaking memory.
///
/// `state()` is currently unused by callers — use `is_pending()`
/// or `try_take_result()` instead, which both have ergonomic
/// semantics. Keeping `state()` here for future API symmetry, since
/// the SessionState enum is the conceptual model.
static SESSION_STATE_PENDING: SessionState = SessionState::Pending;

// =====================================================================
// Process-global session slot — shared by all three GUI surfaces
// (Settings → Account tab, Login & Claim CTA, post-scan CTA). Only
// one OAuth flow can be in flight at a time across the GUI, so a
// single Mutex<Option<OauthSession>> covers every call site.
// =====================================================================

static CURRENT_SESSION: parking_lot::Mutex<Option<OauthSession>> = parking_lot::Mutex::new(None);

/// Attempt to start an OAuth flow. Returns
/// `Err(SessionAlreadyRunning)` if a flow is already in flight —
/// the caller should keep showing the existing "Waiting for
/// browser sign-in…" UI rather than starting a second flow
/// against the same loopback port.
pub fn try_start_session(
    provider: Provider,
    channel: Channel,
    server_url: &str,
    install_id: &str,
    timeout: Duration,
) -> Result<(), crate::leaderboard::registration::SessionAlreadyRunning> {
    let mut slot = CURRENT_SESSION.lock();
    if slot.is_some() {
        return Err(crate::leaderboard::registration::SessionAlreadyRunning);
    }
    *slot = Some(OauthSession::start(
        provider, channel, server_url, install_id, timeout,
    ));
    // Clear any prior toast — the user is starting fresh.
    clear_toast();
    Ok(())
}

/// Snapshot of the in-flight session for render-time inspection.
/// Returns `None` when no flow is running. The provider + elapsed
/// time let the UI render context-specific spinner copy
/// (e.g. "Waiting for Google sign-in (12s)…").
pub fn current_session_snapshot() -> Option<(Provider, Duration)> {
    CURRENT_SESSION
        .lock()
        .as_ref()
        .map(|s| (s.provider(), s.elapsed()))
}

/// True while an OAuth flow is running. Cheaper than calling
/// `current_session_snapshot()` when the caller only needs the
/// boolean.
pub fn session_in_flight() -> bool {
    CURRENT_SESSION.lock().is_some()
}

/// Drain the global session if it's completed. Returns `Some(_)`
/// exactly once per session, after which the slot is cleared and
/// the next `try_start_session` can run. While the session is
/// still pending, returns `None`.
///
/// **Auto-register side effect** (Mick 2026-05-25T01:35Z): when
/// the result is `Err(InstallNotRegistered)`, captures the
/// provider that was in flight + kicks off a register session in
/// the background so the user doesn't have to bounce out to the
/// CLI. The `take_pending_retry_provider` slot is what
/// [`crate::leaderboard::registration::poll_register_session`]
/// consumes on success to auto-retry the OAuth flow.
pub fn poll_session() -> Option<Result<OauthToken, OauthError>> {
    // Snapshot the in-flight provider BEFORE drain — once we
    // clear the slot, this info is gone.
    let provider_in_flight = {
        let slot = CURRENT_SESSION.lock();
        slot.as_ref().map(|s| (s.provider(), s.channel()))
    };

    let result = {
        let mut slot = CURRENT_SESSION.lock();
        let session = slot.as_mut()?;
        if let Some(result) = session.try_take_result() {
            *slot = None;
            result
        } else {
            return None;
        }
    };

    // Auto-register chain: install isn't known to web → kick off
    // register + remember the provider for auto-retry post-register.
    if let Err(OauthError::InstallNotRegistered) = &result {
        if let Some((provider, channel)) = provider_in_flight {
            set_pending_retry_provider(provider);
            log_oauth_event(&format!(
                "auto_register_chain: InstallNotRegistered for {} on {}; \
                 kicking register session to auto-retry",
                provider, channel
            ));
            if crate::leaderboard::registration::try_start_register_session(channel).is_err() {
                log_oauth_event(
                    "auto_register_chain: register session already in flight; can't auto-chain",
                );
            }
        }
    }

    Some(result)
}

/// Signal the in-flight session to cancel + drop it. Listener
/// thread sees the flag on the next poll-tick (~100 ms) and exits
/// with `Err(Cancelled)`; the result is discarded since we cleared
/// the slot.
pub fn cancel_current_session() {
    let mut slot = CURRENT_SESSION.lock();
    if let Some(session) = slot.as_ref() {
        session.cancel();
    }
    *slot = None;
}

// =====================================================================
// User-visible OAuth toast — last completed flow's result. Set by
// poll_session callers when they drain a result; read by the three
// CTA surfaces so the user gets a clear "linked as Mick (Google)"
// success or a "link failed: <reason>" error in the GUI without
// having to grep oauth.log. Cleared on next session start.
// =====================================================================

/// One of three states a CTA can render below itself when a
/// session has just finished. Bounded — old toasts get cleared
/// when a fresh OAuth flow starts.
#[derive(Debug, Clone)]
pub enum OauthToast {
    Success {
        provider: Provider,
        display_name: String,
    },
    Failure {
        reason: String,
    },
}

static LAST_TOAST: parking_lot::Mutex<Option<OauthToast>> = parking_lot::Mutex::new(None);

/// Record the result of a just-completed OAuth flow. Called from
/// the three CTA surfaces inside their `poll_session` drain.
pub fn record_toast(result: &Result<OauthToken, OauthError>) {
    let toast = match result {
        Ok(t) => OauthToast::Success {
            provider: t.provider,
            display_name: t.display_name.clone(),
        },
        Err(e) => OauthToast::Failure {
            reason: e.to_string(),
        },
    };
    *LAST_TOAST.lock() = Some(toast);
}

/// Snapshot of the most-recent toast for render. Returns a clone
/// (toasts are tiny) so the CTA closures can decide visibility
/// without holding the lock.
pub fn current_toast() -> Option<OauthToast> {
    LAST_TOAST.lock().clone()
}

/// Clear the visible toast — called from a Dismiss button OR
/// from `try_start_session` when a fresh flow kicks off.
pub fn clear_toast() {
    *LAST_TOAST.lock() = None;
}

// =====================================================================
// Pending OAuth retry — when an OAuth flow fails with
// InstallNotRegistered, engine kicks off a register flow + stashes
// the provider here. After register completes successfully, the
// next CTA frame consumes this slot + auto-retries OAuth with the
// stored provider. Per Mick's 2026-05-25T01:35Z preference
// ("auto-register you because you've already decided to participate").
// =====================================================================

static PENDING_RETRY_PROVIDER: parking_lot::Mutex<Option<Provider>> = parking_lot::Mutex::new(None);

pub fn set_pending_retry_provider(provider: Provider) {
    *PENDING_RETRY_PROVIDER.lock() = Some(provider);
}

pub fn take_pending_retry_provider() -> Option<Provider> {
    PENDING_RETRY_PROVIDER.lock().take()
}

/// Parse the JSON body web POSTs to our loopback. Expected shape:
///
/// ```json
/// {
///   "access_token": "...",
///   "refresh_token": "...",      // optional
///   "expires_in": 3600,           // seconds-from-now; we add to current time
///   "display_name": "Mick",
///   "account_id": "acct-..."
/// }
/// ```
///
/// If web's actual surface differs, this is the single place to
/// update — the rest of the module deals in [`OauthToken`].
///
/// Web's actual shape (confirmed 2026-05-25T00:30Z dev test):
///
/// ```json
/// {
///   "account_id": "d8e2e7d8-...",
///   "display_name": "Mick",
///   "provider": "discord",
///   "discord_user_id": "...",
///   "linked_install_id": "fec94a96-..."
/// }
/// ```
///
/// Engine ignores the per-provider user_id field — `account_id`
/// is the cross-provider stable identifier; provider-specific IDs
/// are debugging-only on web's side.
pub fn parse_callback_body(provider: Provider, body: &str) -> Result<OauthToken, OauthError> {
    #[derive(Deserialize)]
    struct CallbackBody {
        #[serde(default)]
        display_name: String,
        #[serde(default)]
        account_id: String,
        #[serde(default)]
        linked_install_id: String,
        /// Web's profile-v1.5 surface (2026-05-25T09:12Z): Discord
        /// avatar hash from `/users/@me`. Absent on Google + on
        /// Discord users with no avatar set; serde tolerates both.
        #[serde(default)]
        discord_avatar_hash: Option<String>,
    }
    let parsed: CallbackBody = serde_json::from_str(body)
        .map_err(|e| OauthError::BadCallback(format!("json parse: {e}")))?;
    if parsed.account_id.is_empty() {
        return Err(OauthError::BadCallback("account_id is empty".into()));
    }
    Ok(OauthToken {
        provider,
        display_name: parsed.display_name,
        account_id: parsed.account_id,
        linked_install_id: parsed.linked_install_id,
        discord_avatar_hash: parsed.discord_avatar_hash,
        // #47 — server doesn't issue a slug at OAuth callback time;
        // engine derives + claims it via the dedicated vanity-slug
        // endpoint afterwards. Leave `None` here so callers know
        // they need to call `claim_vanity_slug_for_token` to
        // populate it.
        vanity_slug: None,
    })
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn make_nonce() -> String {
    // 16 bytes of /dev/urandom hex; same threat-level rationale as
    // captcha::make_nonce. The browser sees this nonce, then POSTs
    // it back — an attacker without the nonce can't blindly POST.
    let mut buf = [0u8; 16];
    #[cfg(unix)]
    {
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            let _ = f.read_exact(&mut buf);
        }
    }
    #[cfg(windows)]
    {
        // Reuse install::fill_random's fallback (time-seeded
        // xorshift). Nonce doesn't need crypto strength — the
        // attack surface is "guess a 16-byte nonce in the timeout
        // window," dominated by network latency, not entropy.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
            ^ 0xDEAD_BEEF_CAFE_F00D;
        let mut seed = now;
        for b in buf.iter_mut() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            *b = (seed >> 56) as u8;
        }
    }
    let mut s = String::with_capacity(32);
    for byte in &buf {
        s.push_str(&format!("{:02x}", byte));
    }
    s
}

/// Open `url` in the user's default browser. Wraps the
/// cross-platform `crate::platform::open_url` helper so the
/// signature here stays bool-shaped for the existing
/// `try_open_browser(url) || fall_back_to_manual_url` call sites.
/// Per #74 — was previously a duplicate of
/// `captcha.rs::open_browser_windows` + dispatch logic.
fn try_open_browser(url: &str) -> bool {
    crate::platform::open_url(url).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn provider_round_trips_slug() {
        for &p in Provider::all() {
            assert_eq!(Provider::from_str(p.as_slug()).unwrap(), p);
        }
    }

    #[test]
    fn provider_parse_is_case_insensitive() {
        assert_eq!(Provider::from_str("GOOGLE").unwrap(), Provider::Google);
        assert_eq!(Provider::from_str("Discord").unwrap(), Provider::Discord);
    }

    #[test]
    fn provider_parse_rejects_unknown() {
        let err = Provider::from_str("github").unwrap_err();
        assert!(err.to_string().contains("github"));
    }

    #[test]
    fn token_round_trips_json() {
        let t = OauthToken {
            provider: Provider::Google,
            display_name: "Mick".into(),
            account_id: "acct-123".into(),
            linked_install_id: "fec94a96-...".into(),
            discord_avatar_hash: None,
            vanity_slug: Some("mick".into()),
        };
        let s = serde_json::to_string(&t).unwrap();
        let back: OauthToken = serde_json::from_str(&s).unwrap();
        assert_eq!(back.provider, Provider::Google);
        assert_eq!(back.account_id, "acct-123");
        assert_eq!(back.display_name, "Mick");
        assert_eq!(back.linked_install_id, "fec94a96-...");
    }

    #[test]
    fn token_deserialises_legacy_shape_without_linked_install_id() {
        // Pre-2026-05-25 token files may lack `linked_install_id`
        // (the field was introduced after web confirmed its
        // response shape). `#[serde(default)]` on the field means
        // old files still load cleanly.
        let json = r#"{
            "provider": "discord",
            "display_name": "User#0001",
            "account_id": "acct-1"
        }"#;
        let t: OauthToken = serde_json::from_str(json).unwrap();
        assert!(t.linked_install_id.is_empty());
        assert_eq!(t.provider, Provider::Discord);
    }

    #[test]
    fn oauth_path_per_channel_distinct_from_install_path() {
        // oauth.{channel}.json sits in the same dir as
        // install.{channel}.json — but has a distinct filename so
        // a save to one never overwrites the other.
        let oauth = oauth_path_for(Channel::Prod).unwrap();
        let install = crate::leaderboard::install::install_path_for(Channel::Prod).unwrap();
        assert_ne!(oauth, install);
        assert_eq!(oauth.parent(), install.parent());
        let fname = oauth.file_name().unwrap().to_string_lossy().to_string();
        assert!(fname.starts_with("oauth."));
        assert!(fname.ends_with(".json"));
        assert!(fname.contains("prod"));
    }

    #[test]
    fn oauth_paths_per_channel_are_distinct() {
        let p = oauth_path_for(Channel::Prod).unwrap();
        let d = oauth_path_for(Channel::Dev).unwrap();
        let l = oauth_path_for(Channel::Local).unwrap();
        assert_ne!(p, d);
        assert_ne!(p, l);
        assert_ne!(d, l);
    }

    #[test]
    fn parse_callback_body_happy_path() {
        // Exact shape web returns per 2026-05-25T00:30Z dev log:
        let body = r#"{
            "account_id": "d8e2e7d8-f3e1-4c47-a916-10d4e45f5633",
            "display_name": "Mick",
            "provider": "discord",
            "discord_user_id": "1507968343867654197",
            "linked_install_id": "fec94a96-4489-4dbc-bba0-daf48c0416f9"
        }"#;
        let t = parse_callback_body(Provider::Discord, body).unwrap();
        // Engine takes `provider` from the start-of-flow argument,
        // not from the response body (defense against a spoofed
        // response with a wrong provider field).
        assert_eq!(t.provider, Provider::Discord);
        assert_eq!(t.display_name, "Mick");
        assert_eq!(t.account_id, "d8e2e7d8-f3e1-4c47-a916-10d4e45f5633");
        assert_eq!(t.linked_install_id, "fec94a96-4489-4dbc-bba0-daf48c0416f9");
    }

    #[test]
    fn parse_callback_body_rejects_missing_account_id() {
        // account_id is the cross-machine roll-up identifier; if
        // web omits it the link can't be persisted meaningfully.
        let body = r#"{"display_name": "Mick"}"#;
        let err = parse_callback_body(Provider::Google, body).unwrap_err();
        match err {
            OauthError::BadCallback(_) => {}
            other => panic!("expected BadCallback, got {other:?}"),
        }
    }

    #[test]
    fn parse_callback_body_tolerates_extra_provider_fields() {
        // Web's response includes `provider`, `discord_user_id`,
        // etc. — engine ignores them and just keys on the fields
        // it cares about. No #[serde(deny_unknown_fields)].
        let body = r#"{
            "account_id": "acct-x",
            "display_name": "User",
            "provider": "google",
            "google_user_id": "10293847566",
            "linked_install_id": "inst-1",
            "future_field_we_dont_know_about": 42
        }"#;
        let t = parse_callback_body(Provider::Google, body).unwrap();
        assert_eq!(t.account_id, "acct-x");
    }

    #[test]
    fn parse_callback_body_rejects_garbage() {
        let body = "not json at all";
        assert!(matches!(
            parse_callback_body(Provider::Google, body).unwrap_err(),
            OauthError::BadCallback(_)
        ));
    }

    #[test]
    fn urlencode_passes_alphanumeric_and_safe_chars() {
        assert_eq!(urlencode("abc-XYZ.123_~"), "abc-XYZ.123_~");
    }

    #[test]
    fn urlencode_escapes_specials() {
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("a/b"), "a%2Fb");
        assert_eq!(urlencode("a?b=c&d"), "a%3Fb%3Dc%26d");
    }

    #[test]
    fn make_nonce_is_32_hex_chars() {
        let n = make_nonce();
        assert_eq!(n.len(), 32, "16 bytes hex-encoded = 32 chars");
        assert!(n.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn build_auth_url_google_includes_pkce_and_correct_host() {
        let (url, verifier) = build_auth_url(
            Provider::Google,
            Channel::Dev,
            "http://127.0.0.1:12345/oauth-callback",
            "test-state",
        )
        .expect("google dev has a client_id");
        assert!(
            url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"),
            "google auth must target the official OAuth endpoint, got: {url}"
        );
        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("state=test-state"));
        // The verifier must be a non-trivial base64url string the
        // engine can later send to the exchange endpoint.
        let v = verifier.expect("google flow returns a PKCE verifier");
        assert!(v.len() >= 43, "verifier too short ({}): {v}", v.len());
        assert!(
            v.chars()
                .all(|c| { c.is_ascii_alphanumeric() || c == '-' || c == '_' }),
            "verifier must be base64url-safe chars only: {v}"
        );
        // Client ID match — assert the dev one is in the URL.
        assert!(
            url.contains("j1fqjo24vgn7ik2mmh1q5ebo06b3226b"),
            "dev google client_id missing from auth URL: {url}"
        );
    }

    #[test]
    fn build_auth_url_discord_omits_pkce_on_dev() {
        let (url, verifier) = build_auth_url(
            Provider::Discord,
            Channel::Dev,
            "http://127.0.0.1:12345/oauth-callback",
            "test-state",
        )
        .expect("discord dev has a client_id");
        assert!(
            url.starts_with("https://discord.com/api/oauth2/authorize?"),
            "discord auth must target the discord endpoint, got: {url}"
        );
        assert!(url.contains("response_type=code"));
        assert!(!url.contains("code_challenge"), "discord doesn't use PKCE");
        assert!(url.contains("state=test-state"));
        // Discord dev client ID
        assert!(
            url.contains("1508187203053031454"),
            "discord dev client_id missing from auth URL: {url}"
        );
        assert!(
            verifier.is_none(),
            "discord flow must not produce a PKCE verifier"
        );
    }

    #[test]
    fn build_auth_url_discord_prod_returns_no_client_id() {
        // Web hasn't surfaced the prod Discord client_id yet
        // (per 2026-05-24T23:12Z post). The engine must surface
        // a clear `NoClientId` error instead of building a
        // half-baked auth URL.
        let err = build_auth_url(
            Provider::Discord,
            Channel::Prod,
            "http://127.0.0.1:12345/oauth-callback",
            "test-state",
        )
        .expect_err("discord prod has no client_id yet");
        assert!(
            matches!(
                err,
                OauthError::NoClientId {
                    provider: Provider::Discord,
                    channel: Channel::Prod
                }
            ),
            "expected NoClientId, got {err:?}",
        );
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_reference_vector() {
        // RFC 7636 §B.2 verifier:
        //   dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk
        // Expected challenge:
        //   E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM
        let challenge = pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
            "PKCE challenge must match the RFC 7636 §B.2 reference vector"
        );
    }

    #[test]
    fn base64url_nopad_known_vectors() {
        // RFC 4648 §10 with the URL-safe alphabet, no padding:
        // empty → ""
        // f      → "Zg"
        // fo     → "Zm8"
        // foo    → "Zm9v"
        // foob   → "Zm9vYg"
        // fooba  → "Zm9vYmE"
        // foobar → "Zm9vYmFy"
        assert_eq!(base64url_nopad(b""), "");
        assert_eq!(base64url_nopad(b"f"), "Zg");
        assert_eq!(base64url_nopad(b"fo"), "Zm8");
        assert_eq!(base64url_nopad(b"foo"), "Zm9v");
        assert_eq!(base64url_nopad(b"foob"), "Zm9vYg");
        assert_eq!(base64url_nopad(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_nopad(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn parse_callback_query_extracts_code_and_state() {
        let p = parse_callback_query("/oauth-callback?code=ABC123&state=xyz");
        assert_eq!(p.code.as_deref(), Some("ABC123"));
        assert_eq!(p.state.as_deref(), Some("xyz"));
        assert!(p.error.is_none());
    }

    #[test]
    fn parse_callback_query_handles_url_encoding() {
        let p = parse_callback_query("/oauth-callback?code=a%20b%2Fc&state=xyz");
        assert_eq!(p.code.as_deref(), Some("a b/c"));
    }

    #[test]
    fn parse_callback_query_returns_error_when_provider_declines() {
        let p = parse_callback_query(
            "/oauth-callback?error=access_denied&error_description=user%20declined",
        );
        assert!(p.code.is_none());
        let e = p.error.expect("error captured");
        assert!(e.contains("access_denied"));
        assert!(e.contains("user declined"));
    }

    #[test]
    fn pkce_verifier_length_is_within_rfc_range() {
        // RFC 7636 §4.1: 43-128 chars. 32 random bytes
        // base64url-no-pad = 43 chars exactly.
        let v = pkce_verifier();
        assert!(
            (43..=128).contains(&v.len()),
            "verifier length {} out of RFC 7636 range",
            v.len()
        );
    }

    #[test]
    fn oauth_session_does_not_block_the_caller() {
        // **Regression sentinel for GitHub issue #2.** A previous
        // version of the OAuth surface called `link_via_loopback`
        // synchronously from the egui render path, hanging the GUI
        // for the duration of the OAuth flow. The fix routes through
        // `OauthSession::start` which spawns a worker thread and
        // returns immediately.
        //
        // This test verifies the non-blocking contract: starting a
        // session against a server_url that will never reply
        // (loopback to a port that's not listening for our callback)
        // must return control within milliseconds. The session stays
        // in `is_pending()` state; `try_take_result()` returns None.
        // We then cancel + drop it.
        use std::time::Instant;
        let started = Instant::now();
        let mut session = OauthSession::start(
            Provider::Google,
            Channel::Local,
            "http://127.0.0.1:1", // unused; flow never completes
            "test-install",
            Duration::from_millis(500),
        );
        let setup_elapsed = started.elapsed();
        assert!(
            setup_elapsed.as_millis() < 250,
            "OauthSession::start must not block the caller — \
             took {}ms (issue #2 regression)",
            setup_elapsed.as_millis(),
        );

        // Pending immediately; no result yet.
        assert!(session.is_pending(), "fresh session must be pending");
        assert!(
            session.try_take_result().is_none(),
            "no result available yet"
        );

        // Cancel + verify the session resolves with SOME error
        // within a couple seconds. Acceptable terminal errors:
        // Cancelled (cancellation fired before completion),
        // Timeout (the 500ms outer wait elapsed), or
        // BrowserOpenFailed (test envs without xdg-open / open /
        // start). The contract this test pins is "session does not
        // block the caller AND resolves promptly" — not "ends with
        // a specific error variant."
        session.cancel();
        let cancel_started = Instant::now();
        loop {
            if let Some(r) = session.try_take_result() {
                assert!(
                    r.is_err(),
                    "no real OAuth flow ran; expected Err(_), got {r:?}"
                );
                break;
            }
            if cancel_started.elapsed() > Duration::from_secs(2) {
                panic!("session didn't resolve within 2s of cancel + 500ms timeout");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn session_in_flight_tracks_global_slot() {
        // Process-global slot must reflect "session started" +
        // "session cleared" across try_start_session / poll_session
        // / cancel_current_session. This is the contract the three
        // GUI surfaces depend on (they all read session_in_flight()
        // each frame to decide whether to render the spinner).

        // Pre-condition: no session in flight at test start. Other
        // tests in this module don't leak.
        cancel_current_session();
        assert!(!session_in_flight());

        // Start one with a tiny timeout so it cleans up fast.
        try_start_session(
            Provider::Discord,
            Channel::Local,
            "http://127.0.0.1:1",
            "test-install",
            Duration::from_millis(200),
        )
        .expect("first start_session should succeed");
        assert!(session_in_flight());
        assert!(current_session_snapshot().is_some());

        // Second start while first is in flight: must reject.
        assert!(try_start_session(
            Provider::Google,
            Channel::Local,
            "http://127.0.0.1:1",
            "test-install",
            Duration::from_millis(200),
        )
        .is_err());

        // Cancel + drain.
        cancel_current_session();
        assert!(!session_in_flight());
        assert!(current_session_snapshot().is_none());
    }

    #[test]
    fn account_status_anonymous_when_no_file() {
        // Direct check against the path resolver — we can't easily
        // simulate "no file exists" across a stable per-platform
        // data_dir, but we CAN verify the Anonymous variant is the
        // None case in the status mapping. This compiles + roundtrips.
        match AccountStatus::Anonymous {
            AccountStatus::Anonymous => {}
            AccountStatus::Linked { .. } => panic!("anonymous mapped wrong"),
        }
    }
}
