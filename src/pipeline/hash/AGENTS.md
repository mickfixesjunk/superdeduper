# hash — AGENTS guide

## Purpose
This directory hosts the content-hash plumbing for the scan pipeline.
`algo.rs` defines the algorithm enum (`HashAlgo`) and the streaming
dispatch wrapper (`ContentHasher`) that the rest of the engine uses
instead of touching `blake3::Hasher` or `river5::StackHasher`
directly. `format.rs` is the Tier 0 (format-aware) fingerprint
dispatcher: for known container extensions it calls into a per-format
parser in the `format/` subdirectory and folds the parser output
through a `ContentHasher` keyed by the same `HashAlgo` the scan was
configured with.

The parent module `src/pipeline/hash.rs` is what orchestrates the
tier pipeline; this subdirectory only provides the algorithm wrapper
and the Tier 0 entry point. Per-format parsers (jpeg, png, mp3, mp4,
mkv, pdf, zip) live in `format/` and are outside this audit's scope.

## Files

### `algo.rs`
Algorithm enum and streaming hasher dispatch. `HashAlgo::River5` is
the default (was `Ddh128`, briefly `River128`); serde aliases keep
old persisted `ScanSettings` and old cache rows loadable. The
`ContentHasher` enum wraps `blake3::Hasher` and
`river5::StackHasher` (the latter chosen specifically to avoid the
per-file heap allocation that `river5::Hasher` paid; see file header
for the ~3x wall-clock motivation).

- Public API:
  - `enum HashAlgo { Blake3, River5 }` — Copy + serde + Default = River5
  - `HashAlgo::output_len(self) -> usize` — 32 or 16
  - `HashAlgo::tag(self) -> &'static str` — `"blake3"` / `"river5"`
  - `HashAlgo::from_tag(s: &str) -> Option<Self>` — accepts legacy `"ddh128"`
  - `enum ContentHasher { Blake3(blake3::Hasher), River5(river5::StackHasher) }`
  - `ContentHasher::new(algo) / update(&mut self, &[u8]) / finalize(self) -> Vec<u8>`
  - `fn hash_oneshot(algo, data) -> Vec<u8>`
- Who calls this: `src/pipeline/hash.rs` (orchestrator), `src/cache.rs`
  (schema column + serde of `HashAlgo`), `src/config.rs`
  (`ScanSettings::hash_algo`), `src/cli.rs`, the GUI settings modal,
  bench/leaderboard crates, image_hash / audio_hash sibling
  pipelines, and a number of integration tests.
- Key types or invariants: output width is fixed per algo (32 / 16);
  streaming hash MUST equal one-shot hash for the same bytes (tested);
  serde aliases `Ddh128` / `River128` are load-bearing for old
  checkpoints — do not drop them without a schema-reset story.
- Feature gates: none.

### `format.rs`
Tier 0 fingerprint dispatcher. `Format::from_path` maps known
extensions onto a `Format` enum; `fingerprint()` opens the file, runs
the matching per-format parser, prepends the format discriminant
(`fmt as u8`) into the hasher, then folds the parser bytes through a
`ContentHasher` of the caller's chosen `HashAlgo`. Any I/O or parse
error returns `None` and the caller is expected to fall back to the
byte-sampling tiers — Tier 0 is documented as never inventing
matches.

- Public API:
  - `enum Format { Zip, Jpeg, Png, Mp3, Mp4, Mkv, Pdf }`
  - `Format::from_path(&Path) -> Option<Format>`
  - `pub fn fingerprint(path, size, algo) -> Option<Vec<u8>>`
  - `pub mod jpeg / mkv / mp3 / mp4 / pdf / png / zip` (re-exports the format/ subdir)
- Crate-private helpers (`pub(crate)`):
  - `read_n`, `read_u16_be`, `read_u16_le`, `read_u32_be`, `read_u32_le`, `seek_to`
- Who calls this: `src/pipeline/hash.rs:932`
  (`format::fingerprint(...)` in the Tier 0 stage). The format
  submodules in `format/` consume the `read_*` / `seek_to`
  helpers.
- Key types or invariants:
  - Fingerprint bytes are prefixed by `fmt as u8` BEFORE the
    parser output is hashed. Re-ordering enum variants in
    `Format` would silently change all Tier 0 fingerprints and
    invalidate every persisted cache row — a hash-algo schema
    bump alone would not catch it.
  - Extension match is lowercased; only the extension table in
    `from_path` decides Tier 0 eligibility.
  - Parsing failure must return `None`, never a partial / fake
    fingerprint.
- Feature gates: none.

## Invariants / Gotchas
- `Format` discriminant ordering is part of the on-disk hash
  contract. Adding a variant at the end is safe; reordering or
  inserting is not (it silently changes every fingerprint for the
  formats whose discriminant shifted).
- `HashAlgo::tag()` strings (`"blake3"`, `"river5"`) are persisted
  in the cache (`hash_algo TEXT NOT NULL`, see `src/cache.rs:335`).
  Renaming the tag requires a cache schema bump; the
  `from_tag("ddh128")` alias is the precedent for how a rename was
  handled last time.
- Serde aliases `Ddh128` / `River128` on the enum variant are the
  in-memory counterpart of the `"ddh128"` tag alias — needed when
  deserializing old `ScanSettings` checkpoints.
- `ContentHasher::Blake3` carries `blake3::Hasher` (~2 KiB) and
  `River5` carries `river5::StackHasher` (~1 KiB); the
  `#[allow(clippy::large_enum_variant)]` is deliberate. Switching
  the river5 variant back to `river5::Hasher` would re-introduce the
  per-file heap allocation the comment in `algo.rs:75-84` warns
  about.
- `format::fingerprint` opens the file itself; callers should not
  pre-open and pass a handle. `size` is already known to the caller
  and is passed in for parsers that need a total length without an
  extra fstat.

## Dependencies
- INCOMING:
  - `src/pipeline/hash.rs` (sibling orchestrator)
  - `src/cache.rs` (persists `HashAlgo`)
  - `src/config.rs` / `src/cli.rs` (config + CLI flag)
  - `src/gui/widgets/settings_modal.rs`, `src/gui/state.rs`,
    `src/gui/live.rs`, `src/gui/results_store.rs`,
    `src/gui/events.rs`, GUI widgets
  - `src/pipeline/image_hash/*`, `src/pipeline/audio_hash/*`
  - `src/bin/hash_repro.rs`, `src/diagnose.rs`
  - `crates/superdeduper-bench-real/`,
    `crates/superdeduper-bench-iface/`
  - Tests under `tests/` and `src/pipeline/hash/format/*`
- OUTGOING:
  - `blake3`, `river5` (third-party crates)
  - `serde`
  - `std::fs`, `std::io`, `std::path`
  - `super::{HashAlgo, ContentHasher}` from `format.rs` to `algo.rs`
  - `src/pipeline/hash/format/*` per-format parsers

## Refactor Hints
- Cohesion is good; `algo.rs` is the public surface and `format.rs`
  is a thin dispatcher.
- `read_n`, `read_u32_le`, `read_u16_le` carry `#[allow(dead_code)]`
  and have no callers in `src/pipeline/hash/format/*`
  (verified: `grep -rn 'read_u32_le\|read_u16_le\|read_n\b'
  src/pipeline/hash/format/` returns nothing). The header comment
  defends them as "kept available for additional formats (MP4, MKV,
  PDF) on the roadmap" — MP4 / MKV / PDF parsers now exist
  (`format/mp4.rs`, `format/mkv.rs`, `format/pdf.rs`) and still
  don't use them, so the roadmap defense is stale. Either remove or
  update the comment.
- `read_u32_be` is marked `#[allow(dead_code)]` but IS used by
  `format/jpeg.rs:16` — the allowance is now redundant and the
  jpeg.rs side carries a defensive comment about it.
- `ContentHasher::update` returns `()`; the doc on `hash_oneshot`
  claims it is equivalent to
  `ContentHasher::new(algo).update(data).finalize()` which would
  require `update` to return `&mut Self` (builder style). The chain
  doesn't actually compile; this is doc-drift, not a refactor
  opportunity per se.
- Consider documenting the "discriminant byte is hashed first"
  invariant on `Format` itself (currently only implicit in
  `format.rs:75`).

## Wire Surfaces
- Cache schema column `hash_algo TEXT NOT NULL` (values: `"blake3"`,
  `"river5"`; legacy `"ddh128"` accepted on read). Owned by
  `src/cache.rs`, value supplied by `HashAlgo::tag` here.
- Persisted `ScanSettings::hash_algo` (serde, with `Ddh128` /
  `River128` aliases).
- Tier 0 fingerprint byte layout (1 byte `Format` discriminant +
  per-format parser output) is an on-disk wire surface in the sense
  that fingerprints are persisted into the cache; reordering enum
  variants is a silent breaker.
