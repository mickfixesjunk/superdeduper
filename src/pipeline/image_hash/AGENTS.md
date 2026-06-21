# image_hash - AGENTS guide

## Purpose
Perceptual image-hashing module for the superdeduper Tier-4 similarity stage
(GH #25 / T1.2). Wraps `image_hasher` to produce 64-bit perceptual fingerprints
(aHash / dHash / DoubleGradient) and computes Hamming distance + auto-scaling
threshold (`tau_for_n`).

The submodule `tier4` runs AFTER the byte-identical T0-T3 pipeline: filters
inventory to known image extensions, hashes them in parallel via rayon, and
groups files via union-find + an E2 cohesion-cap (cluster diameter must not
exceed `2 * threshold`).

The entire module is gated behind the `similar-images` Cargo feature so the
default dedup pipeline does not pull in image-codec transitive deps.

## Files

### `mod.rs`
Top-level module. Defines the `Algorithm` enum, the `ImageFingerprint` alias
(`u64`), the `tau_for_n` corpus-size scaling formula (E3), `hash_file`,
`hash_image`, `hamming_distance`, and the `HashError` enum. Houses re-export
of the `tier4` submodule.

- Public API:
  - `enum Algorithm { AverageHash, DifferenceHash (default), DoubleGradient }`
  - `Algorithm::as_slug(self) -> &'static str` - stable CLI/JSON slugs:
    "ahash", "dhash", "phash"
  - `type ImageFingerprint = u64`
  - `fn tau_for_n(default_tau: u32, n: u64) -> u32` - corpus-scaled
    Hamming threshold per spec E3
  - `fn hash_file(&Path, Algorithm) -> Result<ImageFingerprint, HashError>`
  - `fn hash_image(&DynamicImage, Algorithm) -> ImageFingerprint`
  - `fn hamming_distance(u64, u64) -> u32`
  - `enum HashError { Io(io::Error), Decode(image::ImageError) }`
    (implements `Display`, `Error`)
  - `pub mod tier4`

- Who calls this:
  - `src/cli.rs` - `From<ImageHashAlgoArg> for Algorithm`, `tau_for_n`
    via `ImageSimilarityThresholdArg::resolve`
  - `src/main.rs` - main scan path (lines ~2322-2327)
  - `src/gui/live.rs` - GUI scan path (lines ~2512-2526)

- Key invariants:
  - "phash" slug is permanently bound to `DoubleGradient` (NOT true DCT pHash)
    for back-compat with persisted scan JSON + cache rows.
  - `bytes_to_u64` reads the FIRST 8 bytes big-endian; algorithm choice must
    yield >= 64 bits of hash output (currently true for all variants).
  - `tau_for_n` floors at 3 always.

- Feature gates: `#![cfg(feature = "similar-images")]` (whole module)

### `tier4.rs`
Tier-4 perceptual-image similarity grouping submodule. Brute-force O(n^2)
union-find on Hamming distance with a cohesion-cap (diameter <= 2 * threshold)
to defeat #78's transitive-linkage mega-clusters. Hashing is rayon-parallel;
decode/IO failures are silently dropped (debug-traced).

- Public API:
  - `const IMAGE_EXTENSIONS: &[&str]` - lowercase allowlist
    (jpg/jpeg/png/webp/gif/bmp/tiff/tif/ico)
  - `const DEFAULT_THRESHOLD: u32 = 5`
  - `fn is_image_file(&Path) -> bool` - case-insensitive
  - `fn find_similar_groups(&[FileEntry], Algorithm, threshold: u32)
    -> Vec<DuplicateGroup>`

- Private:
  - `struct Hashed<'a> { file: &FileEntry, fingerprint: ImageFingerprint }`
  - `fn cluster_filter_and_build_groups(&[Hashed], threshold)
    -> (Vec<DuplicateGroup>, u64)` - extracted for #141 testability
  - `fn cluster_diameter(&[usize], &[Hashed]) -> u32`

- Who calls this:
  - `src/main.rs` (`tier4::find_similar_groups`, `is_image_file`)
  - `src/gui/live.rs` (`find_similar_groups`, `is_image_file`,
    `DEFAULT_THRESHOLD`)

- Key invariants:
  - Group identity: `content_hash` is the synthetic string
    `format!("perceptual-{canonical_fp:016x}")` where `canonical_fp` is the
    fingerprint of the path-sorted-first member - stability requires the
    same canonical member to sort first across runs.
  - `files` and `file_sizes` arrays are index-aligned (per #147) and
    path-sorted; `size` field is the MAX size in the cluster (smart-keep
    default per spec 3.9).
  - `crate::pipeline::assert_unique_paths(&g)` is called for every emitted
    group - duplicate paths in a group are a hard invariant violation.
  - HEIC is intentionally NOT in `IMAGE_EXTENSIONS` (spec 3.4 deferral).

- Feature gates: `#![cfg(feature = "similar-images")]`

## Invariants / Gotchas
- "phash" slug ALWAYS means `DoubleGradient`, not true DCT pHash. A future
  DCT variant must get a new slug.
- Cohesion-cap uses strict `>`, not `>=`. Boundary case `diameter == 2*tau`
  is KEPT (see `cluster_diameter_handles_singleton_pair_and_chain` test:
  "diameter 10 must NOT exceed cap=10 at tau=5").
- `bytes_to_u64` is big-endian. Changing to LE silently re-orders every
  persisted `perceptual-{hex}` content_hash slug across the cache.
- Hash failures are silent-skipped (only debug-traced). A corrupt JPEG will
  not surface as a similarity candidate but will not fail the scan.
- `tau_for_n` floor of 3 is independent of the user-supplied default; a
  default below 3 still returns >= 3.
- The `hash_size(8, 8)` call is the *output* hash size, not the downsample
  dimension. `image_hasher` does its own internal downsample.
- Singleton clusters (k=1) are dropped before group emission; only clusters
  with >= 2 members produce a `DuplicateGroup`.

## Dependencies
- INCOMING:
  - `src/cli.rs` (Algorithm conversion, tau_for_n)
  - `src/main.rs` (scan-mode == image branch)
  - `src/gui/live.rs` (live GUI scan)
  - `src/pipeline/mod.rs` (re-exports the module)
- OUTGOING:
  - `image_hasher` (Hasher / HashAlg)
  - `image` (DynamicImage / ImageReader / ImageError)
  - `rayon` (par_iter over inventory)
  - `tracing` (debug/info on skip + cohesion-reject)
  - `crate::inventory::FileEntry`
  - `crate::pipeline::{DuplicateGroup, SimilarityKind, assert_unique_paths}`
  - `tempfile` (tests only)

## Refactor Hints
- `hash_image` is `pub` but the module doc-comment for it claims "pub-in-crate
  for future Tier-4 integration." `tier4.rs` does NOT call `hash_image`
  directly (it calls `hash_file`); current sole non-test caller is the
  `tests` module. If full module-API tightening is desired, downgrading
  `hash_image` to `pub(crate)` would not break anything in-tree
  (grep: `git grep "image_hash::hash_image"` returns only the same file).
- `HashError` doc-comment claims it will be "promoted to the crate `Error`
  when Tier-4 integration lands"; Tier-4 has landed but `find_similar_groups`
  silently drops `HashError` rather than propagating, so the promotion is no
  longer required. Consider removing the stale TODO note from the doc.
- BK-tree v3 perf follow-up is mentioned in 4 places (module doc, V1 list,
  Step 2 comment, header comment). Worth tracking under a single issue ref.
- The inner recursive `fn find` inside `cluster_filter_and_build_groups`
  could blow the stack on >100k-image pathological inputs - iterative
  path-compression is the easy follow-up.

## Wire Surfaces
- Algorithm slugs (CLI + JSON + cache content_hash): "ahash" / "dhash" /
  "phash" - ASSERTED STABLE by the `algorithm_slugs_are_stable` test.
- `DuplicateGroup::content_hash` format: `perceptual-{u64-fingerprint-hex}`
  (16 lowercase hex chars). Consumed by cache + presumably by GUI display.
- `DuplicateGroup::similarity_kind == SimilarityKind::PerceptualImage` for
  every group emitted from this module.
- CLI flag (handled in `src/cli.rs`): `--image-similarity-threshold N|auto`,
  `--image-hash-algorithm ahash|dhash|phash`.
- Feature flag: `similar-images` (Cargo).

## Non-source artifacts
None - directory contains only `mod.rs` and `tier4.rs`.
