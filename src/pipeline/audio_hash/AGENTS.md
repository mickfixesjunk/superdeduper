# audio_hash — AGENTS guide

## Purpose
Tier-4 acoustic-audio fingerprinting + similarity grouping for the superdeduper
pipeline (GH #26 / T1.3). Sits AFTER the byte-identical T0-T3 pipeline and adds
a perceptual-audio cluster pass: decode each audio file via `symphonia`,
resample to mono 11025 Hz, feed to `rusty-chromaprint`, then brute-force union-
find groups by per-chunk average Hamming distance.

Entire module is gated behind `#[cfg(feature = "similar-audio")]` — the always-
on dedup pipeline does not pull the audio-codec dep tree (symphonia + transitive
deps).

Callers: CLI `main.rs` (audio scan mode) and GUI `gui/live.rs` (live scan
worker). Both invoke `tier4::find_similar_groups` after building the inventory.

## Files

### `mod.rs`
Module root. Owns the audio-extension allowlist, fingerprint type aliases, the
single-file decode-and-fingerprint pipeline (`hash_file`), the per-chunk and
per-sequence Hamming helpers, the `HashError` enum, and a thread-local
profiling sub-module (`profile`) wired into `hash_file`'s hot loop and gated at
runtime by the `SUPERDEDUPER_AUDIO_PROFILE` env var.

- Public API:
  - `pub const AUDIO_EXTENSIONS: &[&str]` — lowercase ext allowlist (mp3, m4a,
    aac, flac, wav, ogg). OPUS + WMA deliberately omitted.
  - `pub const DEFAULT_THRESHOLD: u32 = 5` — placeholder per-chunk Hamming
    threshold; **not referenced anywhere outside this file** (the live
    threshold is `tier4::DEFAULT_THRESHOLD: f64 = 5.0`).
  - `pub type AudioFingerprint = Vec<u32>`
  - `pub fn is_audio_file(&Path) -> bool`
  - `pub fn hamming_distance_chunk(u32, u32) -> u32`
  - `pub fn average_hamming_distance(&AudioFingerprint, &AudioFingerprint) -> f64`
  - `pub fn hash_file(&Path) -> Result<AudioFingerprint, HashError>`
  - `pub enum HashError { Io, Decode, Chromaprint }` with `Display + Error`.
  - `pub mod profile` — thread-local micros accumulators
    (`T_DECODE`, `T_MIXDOWN`, `T_RESAMPLE`, `T_PCM`, `T_CHROMA`) plus
    `enabled()`, `add()`, `snapshot()`, `reset()`.
  - `pub mod tier4` re-export.
- Who calls this: `tier4.rs` (in-tree). External code reaches the type via
  `tier4` only; no callers reference `audio_hash::hash_file`,
  `audio_hash::AUDIO_EXTENSIONS`, etc. directly today.
- Feature gates: file-level `#![cfg(feature = "similar-audio")]`.

### `tier4.rs`
Tier-4 grouping driver. Filters the inventory to audio extensions, hashes each
file under `catch_unwind` (shields against the symphonia-codec-aac 0.5.5 ICS
panic), classifies decode failures into a stable wire-shape kind set, runs
brute-force O(n^2) union-find on average chunk-Hamming distance, and emits
`DuplicateGroup`s tagged `SimilarityKind::PerceptualAudio` with content_hash
`perceptual-audio-{first-chunk-u32:016x}`.

- Public API:
  - `pub const DEFAULT_THRESHOLD: f64 = 5.0` — canonical threshold; both CLI
    and GUI use this.
  - `pub struct AudioTier4Result { groups, short_skipped_count, decode_warnings }`
  - `pub struct AudioDecodeWarning { path, kind, detail }` (serde-serializable;
    stable kind wire set: `corrupt_header`, `mid_stream_corrupt`, `truncated`,
    `empty_file`, `decoder_panic`, `unknown`).
  - `pub fn find_similar_groups(&[FileEntry], threshold: f64) -> AudioTier4Result`
- Who calls this:
  - `src/main.rs:2362` (CLI audio scan)
  - `src/gui/live.rs:2589` (live GUI scan worker)
- Key types / invariants:
  - `DuplicateGroup.files` and `DuplicateGroup.file_sizes` are kept index-
    aligned (#147 fix) by sorting `(path, size)` tuple pairs together.
  - `decode_warning_paths` only carries members of THIS group that hit a
    decode warning during the hashing phase.
  - `content_hash` format is the literal string `perceptual-audio-{16-hex}`
    where the 16-hex is the first chunk of the lex-min member's fingerprint.
  - `assert_unique_paths` is called per group before push.
- Feature gates: file-level `#![cfg(feature = "similar-audio")]`.

## Invariants / Gotchas
- **Feature-gate**: every item in this directory compiles only with
  `--features similar-audio`. Touching consumer code must mirror the gate.
- **Thread-local profile counters**: `profile::T_*` accumulate per-thread. A
  `snapshot()` reads only the current thread — under `rayon` you must
  aggregate per worker, not call `snapshot()` once. Comment in mod.rs sells
  this as "correct under rayon par_iter" — semantically true, but only via
  per-thread snapshots.
- **`SUPERDEDUPER_AUDIO_PROFILE` is read once and cached** in a `OnceLock`
  for process lifetime; flipping the env var mid-run has no effect.
- **Tail flush drops up to ~93 ms**: comment is accurate (1024 samples /
  11025 Hz). Don't tune `RESAMPLE_CHUNK` without re-verifying.
- **Mono mixdown**: averaged (`sum / n_channels`), not summed — preserves
  headroom for the `f32 -> i16` clamp+round at the chromaprint feed.
- **Hash failures are NOT silently dropped (anymore)**: per #119,
  `find_similar_groups` records `AudioDecodeWarning` records. Only `Ok(Ok(_))`
  with an empty fingerprint counts as a "short skip". Older doc-comment
  language about "silently skipped" inside `find_similar_groups` is
  superseded by the surrounding code.
- **`AudioDecodeWarning.kind` is a stable wire string** — additions require a
  serde-default-compatible bump; the docstring says consumers must fall back
  to `"unknown"`.
- **Group identity stability**: the synthetic `content_hash` is derived from
  the lex-min member's first chunk u32. It is stable across runs ONLY if the
  member set is identical and the canonical sort key (`path.as_os_str()`)
  is stable on the host filesystem.
- **Brute-force O(n^2)**: tier4 is intentionally non-BK-tree; spec §4.4
  defers that. Don't try to "fix" the quadratic loop without coordinating
  with the perf follow-up.

## Dependencies
- INCOMING:
  - `src/main.rs` (CLI audio scan mode)
  - `src/gui/live.rs` (live GUI scan worker)
  - `src/pipeline/mod.rs` (re-exports `audio_hash`)
- OUTGOING:
  - `crate::inventory::FileEntry`
  - `crate::pipeline::{DuplicateGroup, SimilarityKind, assert_unique_paths}`
  - `symphonia` (decode), `rubato` (resample), `rusty-chromaprint`
    (fingerprint), `tracing` (warn/debug), `serde` (warning wire shape),
    `tempfile` (tests only).

## Refactor Hints
- **Dead-or-near-dead `pub const DEFAULT_THRESHOLD: u32 = 5` at
  `mod.rs:123`**: not referenced by any module in the tree (verified by
  `grep -rn audio_hash::DEFAULT_THRESHOLD --include="*.rs"` returning empty;
  callers use `tier4::DEFAULT_THRESHOLD: f64 = 5.0`). Candidates: delete, or
  make `tier4::DEFAULT_THRESHOLD` a re-export. Two consts named the same
  with different types are an invitation to a future bug.
- **`hash_file`'s closure depth**: profile-timing macros + match arms are
  legible but five `if profile_on { Some(Instant::now()) }` blocks could
  become a small RAII guard or macro without changing semantics.
- **`fn find` is recursive path-compression**: a `while` form would avoid
  any stack-depth risk on very deep parent chains (not realistic for the
  expected n<500, but trivial to harden).
- **`mod.rs` profile module's `add()`** is `pub` for use only inside this
  crate's `hash_file`; could move to `pub(super)`.
- **`AUDIO_EXTENSIONS`, `hamming_distance_chunk`, `is_audio_file`,
  `AudioFingerprint`, `HashError`** are all `pub` but have no external
  callers in the current tree (only `tier4` consumes them). All are
  reasonably part of the module's documented public surface, but if this
  module is ever moved behind an internal API the `pub` -> `pub(crate)`
  reduction is mechanical.
- **Top-of-mod "V1 explicitly does NOT include: Tier-4 pipeline
  integration" is stale.** Tier-4 *is* shipped in `tier4.rs` next to it.

## Wire Surfaces
- `AudioDecodeWarning` is `serde::{Serialize, Deserialize}` and lands in
  scan-result JSON; its `kind` field has the stable string set documented
  above. Any addition is a downstream-compatible bump only.
- `DuplicateGroup.content_hash` for audio groups: literal prefix
  `perceptual-audio-` followed by 16 lowercase hex chars (first chunk u32).
- Environment variable read: `SUPERDEDUPER_AUDIO_PROFILE` — presence (any
  value) enables profile timing; absence disables. Read once, cached.
- No HTTP endpoints, no on-disk format owned in this directory.

---

## Audit findings cross-reference
- `mod.rs:60` — references `examples/audio_profile.rs`; that file does not
  exist (only `examples/hash_microbench.rs` is present). Either add the
  example or fix the doc.
- `mod.rs:11-15` — "V1 explicitly does NOT include: Tier-4 pipeline
  integration (next sub-deliverable)" — Tier-4 is now this directory's
  sibling file; comment is stale.
- `mod.rs:122-123` — `DEFAULT_THRESHOLD: u32 = 5` shadows the real-use
  `tier4::DEFAULT_THRESHOLD: f64 = 5.0`. No external callers. Likely
  dead-code residue from the v1 placeholder.
- `tier4.rs:17-23` — "Reports groups via the existing `DuplicateGroup`
  type with `similarity_kind = PerceptualImage` (TODO: add
  `SimilarityKind::PerceptualAudio` variant once design signs off ...)" —
  superseded by the inline comment at lines 311-316 and by the code at
  line 317 (`similarity_kind: SimilarityKind::PerceptualAudio`). The
  module-level doc is stale.
- `tier4.rs:97-101` — "Hash failures (decode error, DRM, unsupported
  codec) are silently skipped" — superseded by the #119 work in the same
  function; failures now produce `AudioDecodeWarning` records.
