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

/// CLI registration flow. Mutates `state` to set `registered = true`
/// + persists, on a successful round-trip.
pub fn register_cli(state: &mut InstallState) -> Result<(), RegisterError> {
    if state.registered {
        return Err(RegisterError::AlreadyRegistered);
    }
    let key = state.install_key().ok_or(RegisterError::MalformedKey)?;
    let nonce = compute_pow(&state.install_id, DEFAULT_POW_DIFFICULTY)
        .ok_or(RegisterError::PoWTimeout)?;

    let body = serde_json::json!({
        "install_id": state.install_id,
        "client_version": state.client_version_at_register,
        "registration_proof": {
            "kind": "pow",
            "challenge": state.install_id,
            "nonce": nonce,
            "difficulty": DEFAULT_POW_DIFFICULTY,
        }
    });
    let canonical = hmac_signer::canonical_body(&body);
    let signature = hmac_signer::sign(&key, &canonical);

    let url = format!(
        "{}/api/v1/register",
        state.server_url.trim_end_matches('/')
    );
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
        .set("X-Sd-Signature", &signature)
        .timeout(std::time::Duration::from_secs(15))
        .send_bytes(&canonical);

    match resp {
        Ok(_) => {
            state.registered = true;
            install::save(state).map_err(|e| {
                RegisterError::SaveFailedAfterServerAck(format!("{e}"))
            })?;
            Ok(())
        }
        Err(ureq::Error::Status(429, _)) => Err(RegisterError::RateLimited),
        Err(ureq::Error::Status(code, resp)) => {
            let body_text = resp.into_string().unwrap_or_default();
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

/// GUI registration via loopback HTTP server. Stubbed; lands in
/// follow-up commit alongside G3 OAuth (same code shape — browser
/// open + 127.0.0.1:0 listener + token capture).
pub fn register_gui_via_loopback(_state: &mut InstallState) -> Result<(), RegisterError> {
    // TODO(g1-followup): same loopback pattern as G3 OAuth.
    //
    // Flow:
    // 1. Bind 127.0.0.1:0; remember the port.
    // 2. Open browser to `{server_url}/setup?cb=http://127.0.0.1:{port}/captcha-callback`.
    // 3. User solves Turnstile on superdeduper.io.
    // 4. superdeduper.io POSTs `{ "captcha_token": "..." }` to our loopback.
    // 5. We POST `/api/v1/register` with `registration_proof: {kind: "captcha", provider: "turnstile", token}`.
    // 6. Close loopback.
    Err(RegisterError::Network(
        "GUI registration not implemented yet — use CLI `sd register`".into(),
    ))
}

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
    for i in 0..full_bytes {
        if digest[i] != 0 {
            return false;
        }
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
