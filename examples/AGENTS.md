# examples/

## Purpose

Cargo `examples/` target directory. Holds standalone binaries that exercise
library APIs outside the main `superdeduper` CLI — typically for
microbenchmarking or repro of specific code paths. Built only when
explicitly requested via `cargo run --example <name>` or `cargo build
--examples`; never compiled by default `cargo build`.

## Files

- `hash_microbench.rs` — in-memory ns/byte microbench comparing the two
  `HashAlgo` backends (River5 / BLAKE3) on a 100 MiB xorshift64-filled
  buffer, 7 trials each, warm-up pass discarded. Removes filesystem,
  page-cache, and tier-dispatch variables — pure hash-kernel throughput.
  Referenced by `src/pipeline/hash.rs` and `src/pipeline/AGENTS.md` as the
  canonical "kernel-only" measurement vs end-to-end pipeline timing.

## Invariants

- Depends on the public re-export path
  `superdeduper::pipeline::hash::algo::{hash_oneshot, HashAlgo}`. If either
  symbol is renamed or the `hash` module loses its `pub mod algo`
  re-export, this example breaks the workspace build under
  `--examples`/`--all-targets`.
- `HashAlgo` must expose at least the `River5` and `Blake3` variants.
- `fill_pseudo` is NOT cryptographic — its only contract is "deterministic
  + cheap + defeats trivial all-zero short-circuits in the hashers."
- Output goes to `stderr` (so `cargo run --example hash_microbench
  2>bench.log` captures everything; stdout stays clean).

## Dependencies

- `superdeduper` crate (the workspace lib itself), specifically
  `pipeline::hash::algo`.
- `std::time::Instant`, `std::hint::black_box`.
- No external dev-deps; no `criterion`, no `rand`.

## Refactor Hints

- The "median" label on the summary line is misleading — the value is the
  arithmetic MEAN over all trials (`total_ns / (trials * len)`), not the
  median. Rename to `mean` or actually compute the median.
- `trials = 7` and `SIZE = 100 MiB` are hard-coded. A future refactor
  could read these from env vars (`SDD_BENCH_TRIALS`, `SDD_BENCH_SIZE_MB`)
  without changing the call surface.
- `worst_ns_per_byte` is computed but only printed in the summary line; if
  it ever becomes unused after a refactor, drop it rather than silencing
  a warning.
- The audio-hash AGENTS.md (`src/pipeline/audio_hash/AGENTS.md:156`)
  laments that no audio-side bench example exists; adding
  `audio_hash_microbench.rs` here would be the natural home.
- No `Cargo.toml` `[[example]]` stanza is required as long as the file
  stays at `examples/<name>.rs` with a `fn main`.

## Wire Surfaces

- None — pure in-process bench. No network, no FS reads, no on-disk
  artifacts produced. stderr lines are the only output; format is
  human-readable and NOT consumed by any harness (testdesign matrices
  use the production binary, not this example).
- Cross-references that point AT this file:
  - `src/pipeline/AGENTS.md:125` and `:232`
  - `src/pipeline/audio_hash/AGENTS.md:156`
  - inline `//!` doctring + `Run:` hint at top of the file itself
