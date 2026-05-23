//! First-run registration per client-spec §5.
//!
//! CLI path: hashcash PoW (22-bit difficulty, ~1s); POST
//! `/api/v1/register` with the challenge response.
//!
//! GUI path: open `https://superdeduper.io/setup?cb=<loopback>`
//! in the system browser; user solves Cloudflare Turnstile;
//! superdeduper.io POSTs the token to our loopback HTTP server.
//! Same loopback pattern reused for G3 OAuth.
//!
//! Both paths terminate with a one-line confirmation + persistence
//! of the install.json registered=true.
//!
//! TODO(g1): implement against client-spec §5.

#[derive(Debug)]
pub enum RegisterError {
    Network(String),
    PoWTimeout,
    CaptchaFailed,
    AlreadyRegistered,
    ServerRejected(String),
}

pub fn register_cli() -> Result<(), RegisterError> {
    todo!("g1: hashcash + POST /api/v1/register")
}

pub fn register_gui_via_loopback(_port_request: u16) -> Result<(), RegisterError> {
    todo!("g1: loopback server + browser launch + token capture")
}
