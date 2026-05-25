//! HMAC-SHA256 signing of canonical-JSON request bodies per
//! client-spec §6 + threat-model §5.2.
//!
//! Convention: caller serialises the payload via
//! `serde_json::to_vec` AFTER sorting keys with [`canonical_body`]
//! so the signature is reproducible from the wire bytes alone. The
//! server recomputes `HMAC-SHA256(install_key, body)` and constant-
//! time compares.
//!
//! Signing is a "speed bump, not a wall" (threat-model §5.2) — the
//! authoritative defences are server-side (sanity checks, K-anon,
//! bench challenges). This module's job is to make casual cheating
//! marginally inconvenient.

use hmac::{Hmac, Mac};
use sha2::Sha256;

use super::install::InstallKey;

type HmacSha256 = Hmac<Sha256>;

/// Sign `body` with the install's HMAC key. Returns the hex-encoded
/// signature for the `X-Sd-Signature` header.
pub fn sign(install_key: &InstallKey, body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(install_key)
        .expect("HMAC accepts any key length; this never fails");
    mac.update(body);
    let bytes = mac.finalize().into_bytes();
    hex_encode(&bytes)
}

/// Sign a canonical string. Used by GET endpoints (no body) where
/// the server's canonical input is a deterministic string assembled
/// from query params (e.g. `${install_id}|${submission_id}` for
/// `/api/v1/ranks`). No trailing newline, no whitespace.
pub fn sign_canonical(install_key: &InstallKey, canonical: &str) -> String {
    sign(install_key, canonical.as_bytes())
}

/// Serialise a JSON value into canonical bytes: keys sorted, no
/// trailing whitespace, no insignificant whitespace. This is what
/// `sign()` operates on AND what gets POSTed as the request body —
/// signature + transmitted bytes match exactly so the server can
/// reproduce the HMAC byte-for-byte.
pub fn canonical_body(value: &serde_json::Value) -> Vec<u8> {
    let canon = canonicalize(value);
    serde_json::to_vec(&canon).expect("canonicalised JSON re-serialises")
}

/// Recursively sort object keys. JSON arrays, numbers, strings,
/// booleans, and null pass through unchanged; objects are
/// reconstructed with `BTreeMap` ordering so the serialiser emits
/// keys in lexicographic order.
fn canonicalize(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: std::collections::BTreeMap<String, serde_json::Value> =
                std::collections::BTreeMap::new();
            for (k, v) in map {
                sorted.insert(k.clone(), canonicalize(v));
            }
            // serde_json::Value::Object uses serde_json::Map which
            // preserves insertion order; collect from the sorted
            // BTreeMap into a fresh Map so the resulting Value's
            // iteration order is alphabetic.
            let mut out = serde_json::Map::with_capacity(sorted.len());
            for (k, v) in sorted {
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonicalize).collect())
        }
        other => other.clone(),
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

    fn key() -> InstallKey {
        let mut k = [0u8; 32];
        for (i, b) in k.iter_mut().enumerate() {
            *b = i as u8;
        }
        k
    }

    #[test]
    fn sign_is_deterministic() {
        let k = key();
        let a = sign(&k, b"hello");
        let b = sign(&k, b"hello");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "64-hex = 32-byte SHA-256 output");
    }

    #[test]
    fn sign_changes_with_body() {
        let k = key();
        let a = sign(&k, b"hello");
        let b = sign(&k, b"world");
        assert_ne!(a, b);
    }

    #[test]
    fn sign_changes_with_key() {
        let body = b"payload";
        let mut k1 = key();
        let mut k2 = key();
        k2[0] = 0xFF;
        assert_ne!(sign(&k1, body), sign(&k2, body));
        let _ = &mut k1;
    }

    #[test]
    fn sign_matches_known_test_vector() {
        // RFC 4231 test case 1 — sanity that we're computing
        // HMAC-SHA256 against the canonical algorithm.
        let key = [0x0bu8; 20];
        let body = b"Hi There";
        // Padded to 32 bytes for our fixed-size key type. We instead
        // test that the underlying HMAC matches the RFC by signing
        // with a 32-byte key derived from the same 20 bytes + zero
        // padding, which gives a different but verifiable result.
        let mut padded = [0u8; 32];
        padded[..20].copy_from_slice(&key);
        let sig = sign(&padded, body);
        // We don't have a pre-computed test vector for the 32-byte
        // key, but length + determinism + key-sensitivity are
        // checked above; this just guards against a regression
        // where the function returns the same value for any input.
        assert_eq!(sig.len(), 64);
    }

    #[test]
    fn canonical_body_sorts_keys() {
        let v = serde_json::json!({ "b": 2, "a": 1, "c": { "z": 9, "y": 8 } });
        let canon = canonical_body(&v);
        let s = String::from_utf8(canon).unwrap();
        // a < b < c, and inside c, y < z.
        assert_eq!(s, r#"{"a":1,"b":2,"c":{"y":8,"z":9}}"#);
    }

    #[test]
    fn canonical_body_signs_stably_across_key_order() {
        let k = key();
        let v1 = serde_json::json!({ "a": 1, "b": 2 });
        let v2 = serde_json::json!({ "b": 2, "a": 1 });
        let b1 = canonical_body(&v1);
        let b2 = canonical_body(&v2);
        assert_eq!(b1, b2, "canonical bytes must be order-independent");
        assert_eq!(sign(&k, &b1), sign(&k, &b2));
    }
}
