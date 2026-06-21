# superdeduper-hmac-signer — AGENTS guide

## Purpose
Leaf crate that implements HMAC-SHA256 signing of canonical-JSON request bodies for the superdeduper leaderboard / bench / action endpoints (client-spec section 6, threat-model section 5.2). It exposes three pure functions: `sign`, `sign_canonical`, and `canonical_body`.

It was extracted from `src/leaderboard/hmac_signer.rs` in Phase 0 (2026-05-31) so future `bench-real` / `bench-stub` crates (and today's `superdeduper-bench-real`) can depend on signing primitives without pulling in the engine binary. The engine module `src/leaderboard/hmac_signer.rs` is now a thin re-export shim around this crate; existing callsites (engine + GUI + leaderboard internals) compile unchanged.

Signing is "speed bump, not a wall" per threat-model section 5.2 — server-side sanity checks, K-anon, and bench challenges are the authoritative defences; this crate's job is to make casual cheating marginally inconvenient.

## Files

### `Cargo.toml`
- Package `superdeduper-hmac-signer` 0.1.0, edition 2021, `publish = false`, MIT.
- Deps: `hmac = "0.12"`, `sha2 = "0.10"`, `serde_json = "1"`. Deliberately dependency-light (no engine types).
- No features.

### `src/lib.rs`
Single module containing the entire public API plus a `#[cfg(test)]` test module.

- **Public API**:
  - `pub type Key = [u8; 32]` — 32-byte HMAC key, mirrors engine's `InstallKey` alias without importing it.
  - `pub fn sign(install_key: &Key, body: &[u8]) -> String` — HMAC-SHA256 over raw bytes, returns 64-char lowercase hex for the `X-Sd-Signature` header.
  - `pub fn sign_canonical(install_key: &Key, canonical: &str) -> String` — convenience wrapper for GET-endpoint canonical strings (e.g. `${install_id}|${submission_id}` for `/api/v1/ranks`); no body, no newline, no whitespace.
  - `pub fn canonical_body(value: &serde_json::Value) -> Vec<u8>` — recursively sorts object keys via `BTreeMap`, re-serialises to JSON bytes. The bytes you sign are the bytes you POST.
- **Private helpers**: `canonicalize` (recursive sort), `hex_encode` (lowercase hex via `format!("{:02x}", ...)`).
- **Tests**: determinism, body-sensitivity, key-sensitivity, length-64 sanity, an RFC-4231-inspired vector (length-only check, no precomputed expected hex), key-order independence of canonical bytes + signatures.

- **Who calls this**:
  - `src/leaderboard/hmac_signer.rs` — re-export shim (`pub use superdeduper_hmac_signer::{canonical_body, sign, sign_canonical}`).
  - Via that shim: `src/leaderboard/submission_store.rs`, `src/leaderboard/account_privacy.rs`, `src/leaderboard/oauth.rs`, `src/gui/widgets/settings_modal.rs`.
  - Direct (crate-to-crate): `crates/superdeduper-bench-real/src/bench_run.rs` (`canonical_body` + `sign` at start_body and submission body).
- **Key types / invariants**: see below.
- **Feature gates**: none in this crate. In the workspace root `Cargo.toml`, `superdeduper-hmac-signer` is an OPTIONAL dep on the engine and is enabled only by the `telemetry` feature.

## Invariants / Gotchas
- **Bytes-on-the-wire == bytes-that-were-signed.** `canonical_body` MUST be called to produce both the POST body and the HMAC input. If a caller re-serialises the `serde_json::Value` separately (e.g. via `serde_json::to_vec` directly), key order is `serde_json::Map` insertion order and the server-side HMAC recompute will mismatch.
- **Key length is fixed at 32 bytes** by the `Key = [u8; 32]` type alias. `Hmac::new_from_slice` itself accepts any length, but the public surface forces 32. If a future RFC test vector with a different key length is added, it must be padded to 32 bytes (see `sign_matches_known_test_vector` test for the pattern, lib.rs:136-153).
- **Lowercase hex.** `hex_encode` emits `"{:02x}"`; servers comparing case-sensitively must agree on lowercase. Constant-time compare is server-side, not here.
- **Recursive canonicalisation copies the entire JSON tree.** Arrays preserve element order (JSON arrays are ordered), only object keys are sorted. Numbers / strings / bools / nulls pass through unchanged — no number normalisation (e.g., `1.0` vs `1` survive as-is per `serde_json::Value` parsing).
- **No async, no I/O, no allocation tricks.** Safe to call from any thread / sync context.

## Dependencies
- INCOMING:
  - `crates/superdeduper-bench-real` (direct)
  - `src/leaderboard/hmac_signer.rs` shim, used by `submission_store.rs`, `account_privacy.rs`, `oauth.rs`, `gui/widgets/settings_modal.rs`
- OUTGOING:
  - `hmac` 0.12, `sha2` 0.10, `serde_json` 1 — all crates.io.

## Refactor Hints
- The crate is cohesive (one concern: canonicalise + HMAC) and zero-coupled to engine types — good leaf-crate hygiene. Do not add engine-side types like `InstallKey` here; the byte-array alias is intentional.
- `hex_encode` uses `format!("{:02x}", byte)` in a loop, which allocates per byte. Replace with `write!` into the pre-allocated `String` or pull in `hex`/`faster-hex` if this ever becomes hot. Currently called once per request — not hot.
- The RFC-4231 test (`sign_matches_known_test_vector`, lib.rs:136-153) only checks length and is somewhat misleadingly named — it does NOT verify against the RFC's expected output (it pads the key to 32 bytes so the expected hex differs). Consider either renaming to `sign_with_padded_rfc_key_has_correct_length` or precomputing the actual expected hex for the padded-key case to make the regression guard real.
- `canonicalize` clones `String` keys on every level; for very large payloads this is wasteful. Could use `std::mem::take` on a mutable input, or borrow via `&str`. Not currently a perf concern.
- Workspace root `Cargo.toml` lines 96-100 and 197 gate this crate behind the `telemetry` feature; if telemetry is ever made the default-on path, the optional-dep wrapping can be dropped.

## Wire Surfaces
- Output: 64-character lowercase hex string for the `X-Sd-Signature` HTTP header.
- Input canonicalisation: JSON objects emitted with keys in lexicographic (BTreeMap) order, `serde_json` default compact serialisation (no whitespace).
- GET-endpoint canonical-string format is OWNED BY CALLERS (e.g. `${install_id}|${submission_id}` for `/api/v1/ranks`); this crate just signs the bytes it's given.
- No env vars, no CLI flags, no on-disk format.

## Non-source artifacts
None.
