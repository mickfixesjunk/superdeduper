//! First-run registration per client-spec §5.
//!
//! CLI path: hashcash PoW (22-bit difficulty, ~1s on modern CPU);
//! `POST /api/v1/register` with the challenge response.
//!
//! GUI path: open `https://superdeduper.io/setup?cb=<loopback>` in
//! the system browser; user solves Cloudflare Turnstile; the
//! superdeduper.io page POSTs the token to our loopback HTTP
//! server. Same loopback pattern reused for G3 OAuth.
//!
//! Both paths terminate with a one-line confirmation + persistence
//! of the install.json with `registered = true`.
//!
//! PoW challenge: client uses its own `install_id` as the challenge
//! string. Server re-computes `sha256(install_id || nonce)` and
//! verifies the leading-zero-bit count. Each install_id is registered
//! exactly once, so replay isn't a concern.

use sha2::{Digest, Sha256};

use super::hmac_signer;
use super::install::{self, InstallState};

#[derive(Debug)]
pub enum RegisterError {
    /// Already registered — caller's `--retry` flow should print
    /// "Already registered" rather than fail.
    AlreadyRegistered,
    /// install.json's `install_key_hex` failed to decode.
    MalformedKey,
    /// PoW found no nonce within iteration cap (very unlikely at
    /// difficulty <= 24).
    PoWTimeout,
    /// HTTP / TCP failure. Not specific to leaderboard semantics.
    Network(String),
    /// Backend returned a 4xx with a parsed reason.
    ServerRejected { status: u16, reason: String },
    /// Backend returned 429.
    RateLimited,
    /// install.json save() failed after a successful register.
    /// Caller's reality is "the server thinks we're registered but
    /// we didn't persist that" — should be re-tryable, but log it
    /// prominently.
    SaveFailedAfterServerAck(String),
}

/// Default PoW difficulty per client-spec §5.3 — 22 leading zero
/// bits is ~1s of single-core CPU on modern hardware (2^22 = 4M
/// SHA-256 iterations in the worst case, average 2^21 = 2M).
pub const DEFAULT_POW_DIFFICULTY: u8 = 22;

// =====================================================================
// Background-thread register session — matches the OauthSession
// pattern in oauth.rs so the GUI can fire register without blocking
// the egui render loop. Same constraints (single in-flight session
// at a time, cancel-friendly poll-tick).
//
// The synchronous `register_cli` + `register_gui_via_loopback` paths
// live further down in this file; see their doc comments for the
// "server's submit_url hint is IGNORED" rationale.
// =====================================================================

/// Process-wide slot for an in-flight register session.
static CURRENT_REGISTER_SESSION: parking_lot::Mutex<Option<RegisterSession>> =
    parking_lot::Mutex::new(None);

/// One background-threaded register flow.
pub struct RegisterSession {
    started_at: std::time::Instant,
    rx: std::sync::mpsc::Receiver<Result<String, RegisterError>>,
    join: Option<std::thread::JoinHandle<()>>,
    cached: Option<Result<String, RegisterError>>,
}

impl RegisterSession {
    /// Spawn a worker that runs `register_cli` on a fresh
    /// `InstallState`. Holds the install state inside the thread;
    /// on success the new install_id is sent back through the
    /// mpsc + the file is already saved. Per Mick's
    /// 2026-05-25T01:20Z preference, this also backs up the
    /// existing install.{channel}.json to `.bak.<ts>` before
    /// generating the fresh state.
    pub fn start(channel: crate::channel::Channel) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<Result<String, RegisterError>>();
        let join = std::thread::spawn(move || {
            // Backup old file if present.
            let backup_result = crate::leaderboard::install::back_up_for(channel);
            if let Err(e) = &backup_result {
                crate::log_warn!("register: backup failed: {e} (continuing anyway)");
            }
            // Generate fresh install state for this channel.
            let server_url = crate::channel::server_url_for(channel).to_string();
            let mut state = crate::leaderboard::install::new_unregistered(server_url);
            match register_cli(&mut state) {
                Ok(()) => {
                    // register_cli already saves via the active
                    // channel. Force a save through the per-channel
                    // helper so a non-active-channel register
                    // (Settings tab on dev while main is prod, etc.)
                    // also lands at the right path.
                    if let Err(e) = crate::leaderboard::install::save_for(channel, &state) {
                        let _ =
                            tx.send(Err(RegisterError::SaveFailedAfterServerAck(format!("{e}"))));
                        return;
                    }
                    let _ = tx.send(Ok(state.install_id));
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
        RegisterSession {
            started_at: std::time::Instant::now(),
            rx,
            join: Some(join),
            cached: None,
        }
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    pub fn is_pending(&mut self) -> bool {
        if self.cached.is_some() {
            return false;
        }
        match self.rx.try_recv() {
            Ok(r) => {
                self.cached = Some(r);
                false
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.cached = Some(Err(RegisterError::Network(
                    "register worker thread died".into(),
                )));
                false
            }
        }
    }

    pub fn try_take_result(&mut self) -> Option<Result<String, RegisterError>> {
        if self.cached.is_none() {
            match self.rx.try_recv() {
                Ok(r) => self.cached = Some(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.cached = Some(Err(RegisterError::Network(
                        "register worker thread died".into(),
                    )));
                }
            }
        }
        self.cached.take()
    }
}

impl Drop for RegisterSession {
    fn drop(&mut self) {
        // No cancel signal yet — register_cli's PoW completes in
        // ~1s typically, so just detach the JoinHandle and let
        // the thread exit on its own.
        let _ = self.join.take();
    }
}

/// Zero-data error returned when a register or OAuth session is
/// already in flight and the caller tried to start another. Carries
/// no payload; the only signal is "busy, try again later." Defined
/// here so [`try_start_register_session`] doesn't return
/// `Result<_, ()>` (clippy's `result_unit_err` lint).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionAlreadyRunning;

impl std::fmt::Display for SessionAlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a leaderboard session is already in flight")
    }
}

impl std::error::Error for SessionAlreadyRunning {}

/// Attempt to start a register session. `Err(SessionAlreadyRunning)`
/// if one is already in flight.
pub fn try_start_register_session(
    channel: crate::channel::Channel,
) -> Result<(), SessionAlreadyRunning> {
    let mut slot = CURRENT_REGISTER_SESSION.lock();
    if slot.is_some() {
        return Err(SessionAlreadyRunning);
    }
    *slot = Some(RegisterSession::start(channel));
    Ok(())
}

/// True while a register flow is in flight.
pub fn register_session_in_flight() -> bool {
    CURRENT_REGISTER_SESSION.lock().is_some()
}

/// Elapsed time since the current register session started.
/// `None` when no session is running.
pub fn register_session_elapsed() -> Option<std::time::Duration> {
    CURRENT_REGISTER_SESSION
        .lock()
        .as_ref()
        .map(|s| s.elapsed())
}

/// Drain the current register session if it has completed.
/// Returns `Some(_)` exactly once per session.
///
/// **Auto-retry OAuth side effect** (Mick 2026-05-25T01:35Z):
/// when register succeeds AND
/// [`crate::leaderboard::oauth::take_pending_retry_provider`]
/// returns a stashed provider, kicks off an OAuth flow with that
/// provider automatically. Closes the chain that started when an
/// earlier OAuth flow failed with `InstallNotRegistered`.
pub fn poll_register_session() -> Option<Result<String, RegisterError>> {
    let result = {
        let mut slot = CURRENT_REGISTER_SESSION.lock();
        let session = slot.as_mut()?;
        if let Some(result) = session.try_take_result() {
            *slot = None;
            result
        } else {
            return None;
        }
    };

    // Auto-retry OAuth if a retry provider is stashed.
    if let Ok(install_id) = &result {
        if let Some(provider) = crate::leaderboard::oauth::take_pending_retry_provider() {
            let active = crate::channel::active_channel();
            let server_url = crate::channel::server_url_for(active);
            crate::leaderboard::oauth::log_oauth_event(&format!(
                "auto_register_chain: register OK; auto-retrying OAuth with {} on {}",
                provider, active
            ));
            if crate::leaderboard::oauth::try_start_session(
                provider,
                active,
                server_url,
                install_id,
                crate::leaderboard::oauth::DEFAULT_OAUTH_TIMEOUT,
            )
            .is_err()
            {
                crate::leaderboard::oauth::log_oauth_event(
                    "auto_register_chain: OAuth session already in flight; can't auto-retry",
                );
            }
        }
    }

    Some(result)
}

/// CLI registration flow. Mutates `state` to set `registered = true`
/// + persists, on a successful round-trip.
///
/// Note on the register response body: the server's `/register`
/// response MAY include a `submit_url` hint (see web's
/// `register.ts:140`). Engine deliberately IGNORES it — per
/// dev-channel-spec.md post-#7 add-on (design 20:09Z, option 2),
/// client-side URL resolution via [`crate::channel::server_url_for`]
/// is the single source of truth. This guarantees a stale hardcoded
/// URL on the server can never leak a dev install onto prod
/// telemetry. Same rule applies to [`register_gui_via_loopback`].
pub fn register_cli(state: &mut InstallState) -> Result<(), RegisterError> {
    if state.registered {
        return Err(RegisterError::AlreadyRegistered);
    }
    // D0 (web ad6f7fe, 2026-05-29) — server-bound PoW: client must first call
    // POST /api/v1/pow/challenge to obtain a server-issued challenge plus
    // an iat+signature that the server later verifies via
    // HMAC(POW_SIGNING_KEY, install_id||iat||challenge). Previously we used
    // state.install_id as the challenge directly; that's now rejected by
    // dev. iat+signature get echoed in the /register body so the server can
    // close the loop.
    let pow = fetch_pow_challenge(&state.server_url, &state.install_id)?;
    crate::log_info!(
        "register_cli: fetched D0 pow_challenge (difficulty={}, ttl={}s)",
        pow.difficulty,
        pow.ttl_secs
    );
    let nonce =
        compute_pow(&pow.challenge, pow.difficulty).ok_or(RegisterError::PoWTimeout)?;

    // Register is the bootstrap: the server doesn't have our
    // install_key_hex yet, so it can't verify the X-Sd-Signature
    // on THIS request. The body carries `install_key_hex` so the
    // server can store the key and verify all subsequent
    // (X-Sd-Signature-checked) requests against it. Per web's
    // backend spec; missing this field caused register to fail
    // server-side until a follow-up fixed it.
    let body = serde_json::json!({
        "install_id": state.install_id,
        "client_version": state.client_version_at_register,
        "install_key_hex": state.install_key_hex,
        "registration_proof": {
            "kind": "pow",
            "challenge": pow.challenge,
            "nonce": nonce,
            "difficulty": pow.difficulty,
            // D0 fields — server-signed handshake from /pow/challenge.
            "iat": pow.iat,
            "signature": pow.signature,
        }
    });
    submit_registration(state, &body)
}

/// D0 (web ad6f7fe) — server-bound PoW challenge fetched from
/// `POST /api/v1/pow/challenge`. The server signs `(install_id || iat ||
/// challenge)` with a server-private key; the engine echoes `iat` and
/// `signature` back in the `/register` body's `registration_proof` so the
/// server can verify both the PoW solution AND that the challenge was
/// genuinely issued by it (binds the client to a specific server-issued
/// challenge, prevents pre-computed solutions).
struct PowChallenge {
    challenge: String,
    iat: i64,
    signature: String,
    difficulty: u8,
    ttl_secs: u64,
}

fn fetch_pow_challenge(server_url: &str, install_id: &str) -> Result<PowChallenge, RegisterError> {
    let url = format!("{}/api/v1/pow/challenge", server_url.trim_end_matches('/'));
    let body = serde_json::json!({ "install_id": install_id });
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .timeout(std::time::Duration::from_secs(15))
        .send_json(&body)
        .map_err(|e| match e {
            ureq::Error::Status(code, r) => RegisterError::ServerRejected {
                status: code,
                reason: r.into_string().unwrap_or_default(),
            },
            ureq::Error::Transport(t) => RegisterError::Network(format!("transport: {t}")),
        })?;
    let v: serde_json::Value = resp
        .into_json()
        .map_err(|e| RegisterError::Network(format!("pow_challenge body parse: {e}")))?;
    let challenge = v
        .get("challenge")
        .and_then(|x| x.as_str())
        .ok_or_else(|| RegisterError::Network("pow_challenge: missing 'challenge'".into()))?
        .to_string();
    let iat = v
        .get("iat")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| RegisterError::Network("pow_challenge: missing 'iat'".into()))?;
    let signature = v
        .get("signature")
        .and_then(|x| x.as_str())
        .ok_or_else(|| RegisterError::Network("pow_challenge: missing 'signature'".into()))?
        .to_string();
    let difficulty = v
        .get("difficulty")
        .and_then(|x| x.as_u64())
        .map(|n| n.min(64) as u8)
        .unwrap_or(DEFAULT_POW_DIFFICULTY);
    let ttl_secs = v.get("ttl").and_then(|x| x.as_u64()).unwrap_or(300);
    Ok(PowChallenge {
        challenge,
        iat,
        signature,
        difficulty,
        ttl_secs,
    })
}

/// GUI registration via loopback HTTP server. Opens the system
/// browser to `{server_url}/setup?cb=http://127.0.0.1:{port}/...`,
/// captures the Turnstile token POSTed back to our loopback, and
/// completes registration with `proof = captcha` instead of PoW.
///
/// Caller's responsibility: spawn this off the UI thread — the
/// captcha-await blocks for up to `CAPTCHA_TIMEOUT`.
pub fn register_gui_via_loopback(state: &mut InstallState) -> Result<(), RegisterError> {
    if state.registered {
        return Err(RegisterError::AlreadyRegistered);
    }
    let token = super::captcha::await_captcha_token(
        &state.server_url,
        &state.install_id,
        CAPTCHA_TIMEOUT,
    )
    .map_err(|e| match e {
        super::captcha::CaptchaError::Timeout => {
            RegisterError::Network("captcha not completed within 5 minutes".into())
        }
        super::captcha::CaptchaError::BrowserOpenFailed(msg) => {
            RegisterError::Network(format!(
                "could not launch browser ({msg}) — use `superdeduper register` from a terminal instead"
            ))
        }
        super::captcha::CaptchaError::BindFailed(msg) => {
            RegisterError::Network(format!("could not bind loopback port: {msg}"))
        }
        super::captcha::CaptchaError::ServerDied => {
            RegisterError::Network("captcha listener died unexpectedly".into())
        }
    })?;

    // See `register_cli` for why install_key_hex appears in the
    // body — bootstrap: server uses it to populate the key store
    // so future X-Sd-Signature checks have something to verify
    // against.
    let body = serde_json::json!({
        "install_id": state.install_id,
        "client_version": state.client_version_at_register,
        "install_key_hex": state.install_key_hex,
        "registration_proof": {
            "kind": "captcha",
            "provider": "turnstile",
            "token": token,
        }
    });
    submit_registration(state, &body)
}

/// #72 — shared body-sign-and-POST core for both
/// [`register_cli`] (PoW proof) and [`register_gui_via_loopback`]
/// (captcha proof). Was 30 lines of character-for-character
/// identical match arms duplicated across both call sites; a
/// future server-error-category change would land in only one
/// path otherwise.
///
/// F11 folded in: response-body read failure now surfaces in the
/// error reason instead of degrading to an empty-string
/// `ServerRejected { status: 500, reason: "" }`. Easy diagnosis
/// when a 5xx returns truncated.
fn submit_registration(
    state: &mut InstallState,
    body: &serde_json::Value,
) -> Result<(), RegisterError> {
    let key = state.install_key().ok_or(RegisterError::MalformedKey)?;
    let canonical = hmac_signer::canonical_body(body);
    let signature = hmac_signer::sign(&key, &canonical);

    let url = format!("{}/api/v1/register", state.server_url.trim_end_matches('/'));
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&canonical);

    match resp {
        Ok(_) => {
            state.registered = true;
            install::save(state)
                .map_err(|e| RegisterError::SaveFailedAfterServerAck(format!("{e}")))?;
            Ok(())
        }
        Err(ureq::Error::Status(429, _)) => Err(RegisterError::RateLimited),
        Err(ureq::Error::Status(code, resp)) => {
            // F11 — preserve mid-stream read errors in the
            // reason rather than silently emptying the string.
            let body_text = resp
                .into_string()
                .unwrap_or_else(|e| format!("<read error: {e}>"));
            let reason = serde_json::from_str::<serde_json::Value>(&body_text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .or_else(|| v.get("reason"))
                        .and_then(|r| r.as_str())
                        .map(String::from)
                })
                .unwrap_or(body_text);
            Err(RegisterError::ServerRejected {
                status: code,
                reason,
            })
        }
        Err(ureq::Error::Transport(t)) => Err(RegisterError::Network(format!("{t}"))),
    }
}

/// Wall-clock budget for the GUI captcha flow. 5 minutes is generous
/// enough for any reasonable user interaction (read page, complete
/// Turnstile widget, return to app) without being so long that an
/// abandoned listener stays alive forever.
pub const CAPTCHA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Hashcash-style PoW: find a `nonce` (decimal string) such that
/// `sha256(challenge_bytes || nonce_bytes)` has at least `bits`
/// leading zero bits. Returns the nonce string on success, `None`
/// if no solution found within the iteration cap.
pub fn compute_pow(challenge: &str, bits: u8) -> Option<String> {
    let max_iter: u64 = 1u64 << (bits as u64 + 6); // generous: 64x expected
    for nonce in 0u64..max_iter {
        let mut h = Sha256::new();
        h.update(challenge.as_bytes());
        h.update(nonce.to_string().as_bytes());
        let digest = h.finalize();
        if has_leading_zero_bits(&digest, bits) {
            return Some(nonce.to_string());
        }
    }
    None
}

fn has_leading_zero_bits(digest: &[u8], bits: u8) -> bool {
    let full_bytes = (bits / 8) as usize;
    let remaining_bits = bits % 8;
    if digest.len() < full_bytes + 1 {
        return false;
    }
    if digest.iter().take(full_bytes).any(|&b| b != 0) {
        return false;
    }
    if remaining_bits > 0 {
        let mask = 0xFFu8 << (8 - remaining_bits);
        return (digest[full_bytes] & mask) == 0;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_leading_zero_bits_zero_bits_always_passes() {
        let d = [0xFFu8; 32];
        assert!(has_leading_zero_bits(&d, 0));
    }

    #[test]
    fn has_leading_zero_bits_one_byte_check() {
        let mut d = [0u8; 32];
        // 0x00 at index 0 → 8 leading zeros
        assert!(has_leading_zero_bits(&d, 8));
        d[0] = 0x01; // 7 leading zeros then a 1
        assert!(has_leading_zero_bits(&d, 7));
        assert!(!has_leading_zero_bits(&d, 8));
        d[0] = 0x80;
        assert!(!has_leading_zero_bits(&d, 1));
    }

    #[test]
    fn has_leading_zero_bits_partial_byte_check() {
        let mut d = [0u8; 32];
        d[0] = 0x00;
        d[1] = 0x0F; // top nibble of byte 1 is zero
                     // 8 + 4 = 12 leading zeros
        assert!(has_leading_zero_bits(&d, 12));
        assert!(!has_leading_zero_bits(&d, 13));
    }

    #[test]
    fn compute_pow_finds_nonce_at_low_difficulty() {
        // 8 bits = avg 256 iterations; trivially fast.
        let nonce = compute_pow("install-uuid-abc", 8).expect("must find nonce");
        // Verify by re-computing.
        let mut h = Sha256::new();
        h.update("install-uuid-abc".as_bytes());
        h.update(nonce.as_bytes());
        let digest = h.finalize();
        assert!(has_leading_zero_bits(&digest, 8));
    }

    #[test]
    fn compute_pow_is_deterministic() {
        let a = compute_pow("same-challenge", 8);
        let b = compute_pow("same-challenge", 8);
        assert_eq!(a, b, "PoW should find the same nonce given same challenge");
    }

    #[test]
    fn compute_pow_changes_with_challenge() {
        let a = compute_pow("challenge-a", 8);
        let b = compute_pow("challenge-b", 8);
        assert_ne!(a, b);
    }
}
