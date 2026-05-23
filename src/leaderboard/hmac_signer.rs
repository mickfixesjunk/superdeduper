//! HMAC-SHA256 signing of canonical-JSON request bodies per
//! client-spec §6 + threat-model §5.2.
//!
//! Caller produces canonical JSON (sorted keys, no whitespace),
//! we HMAC it with the install_key and return the hex header
//! value for `X-Sd-Signature`.
//!
//! TODO(g1): implement against RustCrypto `hmac` + `sha2`.

pub fn sign(_install_key: &[u8; 32], _canonical_body: &[u8]) -> String {
    todo!("g1: hmac-sha256 over canonical-body, hex-encoded")
}
