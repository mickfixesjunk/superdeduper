# superdeduper-bench-real — AGENTS guide

## Purpose

`superdeduper-bench-real` is the default implementation of the
`BenchExecutor` + `SubmissionExecutor` traits declared by
`superdeduper-bench-iface`. It holds the real `--bench-me` flow + the
canonical-bench submission HTTP path that used to live under the engine
binary's `src/leaderboard/`. The Phase-0 trait extraction relocates the
implementation here so engine call sites can stay decoupled from the
heavy bench/Merkle/ChaCha20 surface and so a future fake/mock implementor
(`BenchFake` etc.) can be swapped in without rebuilding the engine.

`BenchReal` (in `lib.rs`) is the concrete trait impl. It delegates
`run_bench` to `bench_run::run` (the orchestration loop:
POST `/bench/start` -> tar download -> size-grouped exact dedupe ->
challenge answer -> HMAC submit) and `submit_recorded` to
`submission_http::submit_inner`. The crate also exports byte-exact
primitives consumed by the engine binary's leaderboard scaffolding via
re-exports (`leaderboard::bench`, `leaderboard::bench_corpus`,
`leaderboard::bench_client`, `leaderboard::d7_probe`,
`leaderboard::submission` shim, etc.) and exposes the cache-bypass read
primitive `read_uncached` for the engine's `pipeline/hash.rs` cold-scan
path.

Wire formats here are FROZEN and cross-stack locked against the web
verifier (`mickfixesjunk/superdeduper-web` #160 + cross-impl goldens in
research/`tcorpus-goldenvec`). Any byte-level change in `bench.rs`,
`bench_client.rs`, or `d7_probe.rs` will diverge the server verifier and
break every submission.

## Files

### `lib.rs`

Crate root + `BenchReal` impl. Defines `BenchReal { _phase_2_state: () }`
and implements both iface traits over it. `run_bench` bridges the
trait's `Fn() -> bool + Send + Sync` cancel closure to the inner
`bench_run::run` API (which takes `&AtomicBool`) by spawning a scoped
propagator thread that polls the closure every ~100ms and writes into a
local `AtomicBool`. `submit_recorded` delegates to
`submission_http::submit_inner`.

Public API:
- `pub struct BenchReal` — Default-constructible trait impl carrier (no
  state today).
- `BenchReal::new()` — constructor.
- `impl BenchExecutor for BenchReal` — `run_bench` + `debug_dedup_diff`.
- `impl SubmissionExecutor for BenchReal` — `submit_recorded`.
- `pub use bench_run::read_uncached;` — re-exported for engine's
  `pipeline/hash.rs` + `pipeline/io_threads_probe.rs` cold-read use
  (and so `--cold-enforced` on `sd scan` shares the same FILE_FLAG_NO_BUFFERING
  / O_DIRECT primitive bench-me uses).
- `pub mod bench;` + `pub mod bench_client;` + `pub mod bench_corpus;` +
  `pub mod bench_run;` + `pub mod d7_probe;` + `pub mod submission_http;`

Who calls this: external — the engine binary instantiates `BenchReal`
behind the iface trait objects.

Key invariants:
- Cancel propagator must run on a scoped thread and must exit when
  `done_flag` flips (otherwise the bench `std::thread::scope` blocks
  forever).
- 100ms cancel-poll cadence is the contract pinned by
  `cancel_propagator_bridges_callback_to_flag` (do not lengthen without
  updating the test's 2-second budget).

### `bench.rs`

T-BENCH-ME canonical-bench cryptographic primitives. FROZEN: every
function here is byte-locked to research's golden vectors AND the web
verifier's reference (`merkle_matches_web_cross_impl_vector`,
`matches_research_golden_vector`, `bc_matches_research_golden_vector`).

Public API:
- `const CHUNK_SIZE: u64 = 1 << 20` — 1 MiB Merkle-leaf chunk.
- `corpus_keys(seed) -> (K_content, K_control)` — BLAKE3 subkey derive.
- `content_bytes_at(k_content, content_id, offset, buf)` — ChaCha20
  keystream, O(1) random access via `StreamCipherSeek`.
- `leaf_hash(path, offset, len, chunk)` — BLAKE3(0x00 || lp_path || off
  || len || chunk).
- `node_hash(left, right)` — BLAKE3(0x01 || L || R).
- `merkle_root(leaves) -> Option<[u8; 32]>` — RFC-6962 PROMOTE-LAST
  (CVE-2012-2459 guard).
- `audit_path(m, leaves)` — RFC-6962 inclusion proof, deepest-sibling-first.
- `root_from_path(leaf, path, m, n)` — verifier-side root recompute.
- `file_leaves(k_content, path, content_id, size)` — per-file leaf
  enumeration in offset order.
- `root_base64(root)` — std-base64 (WITH padding, 44 chars).
- `challenge_positions(bench_challenge_id, leaf_count, n)` — legacy
  bare-id derivation (no domain prefix).
- `const CHALLENGE_DOMAIN_V1: &[u8] = b"tcorpus-challenge-v1"`.
- `challenge_positions_from_bc(bc, leaf_count, n)` — hardened v1
  derivation bound to `BenchContext`.
- `struct BenchContext<'a>` + `BenchContext::encode()` — version-binding
  context (BC). 165-byte canonical encoding cross-locked with research.

Who calls this: `bench_corpus.rs` (server-side planning/manifest path),
`bench_client.rs` (challenge derivation), engine binary's `main.rs`
(via `bc::manifest_hash` + `bc::manifest_m`).

Key invariants:
- Endianness is little-endian throughout (locked).
- `BenchContext.encode`: `manifest_hash` is RAW 32 bytes (NOT
  length-prefixed) — research disambiguated 2026-05-29; a `u32le(32)`
  prefix here breaks the BC golden positions `[1, 5, 0]`.
- Merkle PROMOTE-LAST split MUST be `largest power of two < n`; never
  duplicate a lone node.

### `bench_client.rs`

Client-side bench primitives — challenge-response hashes + result_digest
families + canonical-bench assembly. Carries V1 (tag 0x02), V2 (tag
0x03, server_blob), V3 (tag 0x04, per-run K + per-file mutation), and
V3.1 (tag 0x05 rep_hash, full-mutated-content commitment) flavours, all
cryptographically distinct (different domain tags). V3.1 is the
post-v0.3.1 hard-cutover wire shape.

Public API (selected):
- `challenge_hash`, `challenge_hash_v2`, `challenge_hash_v3` —
  per-position content commitments (tags 0x02 / 0x03 / 0x04).
- `answer_challenge_from_dir`, `answer_challenge_from_dir_v`,
  `answer_challenge_from_dir_v3` — read disk, hash, return
  `(answers, bytes_read)`.
- `result_digest_bytes` + `result_digest`, `_v2` variants, `_v3`
  variants, `_v3_1` variants — canonical-order commitment of the dupset
  partition.
- `const RESULT_DIGEST_DOMAIN`, `_V2`, `_V3`, `_V3_1`,
  `REP_HASH_TAG_V3_1: u8 = 0x05`.
- `per_file_key_v3(K, file_hash)` — HMAC-SHA256 derivation.
- `keystream_at_v3` + `mutate_bytes_v3` — ChaCha20 XOR with absolute
  offset seek.
- `rep_hash_v3_1(rep_path_index, K, file_size, mutated_bytes)` — full
  mutated-content commitment.
- `result_digest_bytes_v3_1`, `result_digest_v3_1`,
  `compute_rep_hashes_v3_1` — V3.1 assembly path.
- `file_raw_hash(path)` — chunked BLAKE3 over a file.
- `to_canonical_bench`, `to_canonical_bench_v`, `to_canonical_bench_v3`
  — final `CanonicalBench` wire assembly.
- `pub use superdeduper_bench_iface::{CanonicalBench, ChallengeAnswer, ChallengePosition};`

Who calls this: `bench_run.rs` (challenge answer + canonical-bench
build), engine binary's `leaderboard::bench_client` re-export shim,
test-only goldens.

Key invariants:
- Canonical dupset ordering: members sorted ascending, groups sorted by
  min member. Permutation must apply in lockstep to rep_hashes.
- V3 challenge / rep_hash inputs MUST be the mutated bytes (raw bytes
  produce a different hash; the mutation IS the forge defence).
- V3.1 wire shape (post-cutover) emits `result_digest_v3_1` +
  `rep_hashes` + `k_echo`; the legacy V3 `result_digest` is NO LONGER
  emitted (server reads only V3.1).
- Filenames follow the engine convention `f{:010}.bin`
  (`answer_challenge_from_dir_v3` + `compute_rep_hashes_v3_1` hardcode
  this).
- All four protocol versions must remain cryptographically distinct via
  domain tag + prefix changes (golden tests in this module enforce).

### `bench_corpus.rs`

Server-side corpus planning + materialization (the seed-holder path).
PLAN layer (`plan_corpus`) is pure + fast; MATERIALIZE layer
(`compute_leaves`, `write_corpus`, `build_manifest`) generates ChaCha20
content + leaves + the signed-shape manifest. Cross-locked against
web's TypeScript `bench-corpus-gen.ts` port via the `sample_tier`
fixture.

Public API (selected):
- Size constants: `SMALL_SIZE`, `MEDIUM_SIZE`, `LARGE_SIZE`,
  `GENERATOR_ID`.
- `struct SizeClassSpec`, `struct TierSpec`, `struct FilePlan`,
  `struct SizeClassCounts`, `struct CorpusPlan`, `struct CorpusManifest`,
  `struct ServedManifest`, `struct LeafLoc`, `struct SampleProof`,
  `struct BenchProof`.
- Tier constructors: `sample_tier`, `quick_tier`, `full_tier`.
- `plan_corpus(spec)`, `compute_leaves(k_content, plan)`,
  `build_manifest(...)`, `write_corpus(...)`, `served_manifest(...)`,
  `scan_corpus_dir(dir)`, `parse_corpus_path_index(name)`,
  `client_found_dupsets(groups)`.
- `dedup_efficiency(measured, ceiling)`.
- `const BENCH_SAMPLE_N: usize = 32`.
- `const MANIFEST_DOMAIN_V1: &[u8] = b"tcorpus-manifest-v1"`,
  `manifest_m(...)`, `manifest_hash(m)`.
- `build_bench_proof(plan, k_content, bc, sample_n)` and
  `build_bench_proof_from_dir(dir, bc, sample_n)`.

Who calls this: `bench_run.rs` (`parse_corpus_path_index`,
`full_content_dedup` candidate enumeration); engine `main.rs`
(`served_manifest`, `build_manifest`, `compute_leaves`, `manifest_m`,
`manifest_hash`); tests-only for `build_bench_proof*`, `leaf_locations`,
`BenchProof`, `SampleProof`, `LeafLoc`, `BENCH_SAMPLE_N`.

Key invariants:
- `f{path_index:010}.bin` filename format is hardcoded throughout.
- `CorpusPlan::reclaimable_ceiling_bytes` assumes `files[i].path_index
  == i` (debug_assert at line 311). Refactoring `plan_corpus` to emit
  non-contiguous indices would silently break.
- `build_manifest` asserts `leaves.len() == plan.leaf_count` — panics
  rather than emit a lying manifest.
- `served_manifest` MUST NOT serialize root or groundtruth (work-proof
  invariant; pinned by `served_manifest_excludes_root_and_groundtruth`).

### `bench_run.rs`

The orchestration loop the CLI `--bench-me` flag AND the GUI "Run
Canonical Bench" button both drive. ONE implementation = no
CLI/GUI drift. Owns the `/bench/start` HTTP, tar download/extraction,
single-subdirectory tar flatten (for v3 corpora), page-eviction
post-untar, `/bench/dedup-ready` anchor, size-grouped exact dedupe
(parallel rayon io_pool sized cpu*3), challenge-answer + result_digest
+ optional V3.1 rep_hashes, and HMAC submit via a caller-supplied
closure.

Public API:
- `fn run(install_id, install_key, server_url, corpus_version, tier,
  workroot, fresh, cancel, progress, lane, submit_fn, hardware_detect)
  -> anyhow::Result<BenchOutcome>`.
- `struct Cancelled` (anyhow downcast target for clean abort).
- `pub use superdeduper_bench_iface::{BenchOutcome, DebugDedupDiff,
  DebugDedupDiffReport};`
- `pub fn read_uncached(path) -> std::io::Result<(Vec<u8>, bool)>` —
  cfg-gated per platform (Linux O_DIRECT, macOS F_NOCACHE, Windows
  FILE_FLAG_NO_BUFFERING, other = buffered).
- `pub fn cold_bypass_reliable() -> bool` — Linux-only (cfg-gated); WSL
  fail-closed guard.
- `pub fn debug_dedup_diff(dir) -> anyhow::Result<DebugDedupDiffReport>`
  — three-way dedupe diff for telemetry only.

Feature gates / cfg:
- `#[cfg(target_os = "linux")] fn evict_file_pages`, `read_uncached`,
  `cold_bypass_reliable`.
- `#[cfg(target_os = "macos")] fn evict_file_pages`, `read_uncached`.
- `#[cfg(target_os = "windows")] fn evict_file_pages`, `read_uncached`.
- `#[cfg(not(any(...)))]` fallback variants.

Who calls this: `BenchReal::run_bench` (`lib.rs`),
`BenchReal::debug_dedup_diff`, engine `pipeline/hash.rs` and
`pipeline/io_threads_probe.rs` (via the `read_uncached` re-export from
`lib.rs`).

Key invariants:
- The dedupe pass MUST mirror the production hasher's IO policy
  (`io_threads = cpu_threads * 3`) for measurement fidelity.
- `cold_enforced` is the AND across every worker — any buffered fallback
  taints the whole run (125x cold/warm spread => the leverage point
  testdesign flagged).
- WSL must report `cold_bypass_reliable() == false` so honest runs land
  on the casual board (testrunner finding).
- Sub-sector files honestly report `cold = false` (cannot be cold-read
  through O_DIRECT/NO_BUFFERING).
- V3 path failure for `compute_rep_hashes_v3_1` is FATAL post-v0.3.1
  cutover (no fallback wire shape).
- The cancel poll inside `full_content_dedup`'s rayon worker uses a
  Relaxed load per file; on cancel the `Cancelled` error short-circuits
  via rayon's first-Err semantics and `run()` downcasts it to a clean
  abort.

### `submission_http.rs`

Pure HTTP path for `/api/v1/submit` (build payload + HMAC sign + POST +
classify response). No InstallState dep; the engine wraps this with its
InstallState-aware variants.

Public API:
- `now_iso8601() -> String` — UTC `YYYY-MM-DDTHH:MM:SSZ` timestamp,
  inlined civil-from-days math so this crate carries no engine `time`
  dep.
- `build_payload(inputs, install_id) -> Value` — canonical submit body
  builder.
- `submit_inner(server_url, install_id, install_key, inputs) ->
  SubmitOutcome` — entry point for the live bench flow.
- `submit_recorded_payload_inner(server_url, current_install_id,
  install_key, payload, built_with_install_id) -> SubmitOutcome` —
  resubmit a stored payload from `scan_history`.

Who calls this: `BenchReal::submit_recorded` (`lib.rs`), engine
`src/leaderboard/submission.rs` (wraps both inner fns).

Key invariants:
- `effective_lane`: a bench submission with `cold_enforced=false` is
  FORCED to `lane="casual"` regardless of caller intent (Phase B.5
  option (b), Mick GO 2026-05-31).
- `build_payload` lifts bench fields (`protocol_version`,
  `corpus_version`, `tier`, `bench_run_id`, `bench_proof`,
  `cold_enforced`) to the top level only when `inputs.bench.is_some()`;
  non-bench scans must NOT carry them.
- 409 response with `submission_id` returns `DuplicateNoChange`, not
  `Rejected`.
- `submit_recorded_payload_inner` refreshes `timestamp` in the recorded
  payload pre-sign so the server's clock-skew sanity check passes on
  resubmit.

### `d7_probe.rs`

D7 calibration probe — Phase C anti-cheat hardware-claim axis. PURE
offset-derivation (`derive_probe_offsets`) + a generic
`execute_probes` driver that takes a `read_at_offset` closure (so the
module stays platform-agnostic; production wires `read_uncached` in).

Public API:
- `const PROBE_COUNT: usize = 32`, `const PROBE_LENGTH: u64 = 4096`,
  `const CALIBRATION_SEED_LEN: usize = 32`.
- `struct FileEntry { path_index, size }`, `struct ProbeTarget`,
  `struct ProbeResult`.
- `derive_probe_offsets(calibration_seed, file_layout) ->
  Vec<ProbeTarget>` — deterministic, byte-exact across engine + web.
- `execute_probes(targets, paths, read_at_offset) -> Vec<ProbeResult>`
  — sequential probe driver.

Who calls this: external — re-exported via `engine::leaderboard::d7_probe`
for forward-compat. No engine call site currently reaches `d7_probe`
(see `src/leaderboard/mod.rs` line 89). 10 golden-vector tests cross-lock
the offset derivation against the web TS verifier.

Key invariants:
- Endianness LE (locked by infosec 2026-05-31 06:46 PST per spec
  L3679-3680 typo arbitration).
- `derive_probe_offsets` empty-layout returns empty Vec rather than
  panicking (caller rejects upstream).
- Probes are SEQUENTIAL — parallel I/O queue effects confuse the
  per-probe latency signal (spec L3690).
- Latency on failed reads is recorded as 0 (server flags but does not
  hard-reject — some real disks legitimately fail mid-scan).
- All 10 golden vectors (`LOCKED_V1_OFFSETS` ... `LOCKED_V10_...`) are
  byte-pinned; regeneration requires the documented protocol in
  `docs/testing/d7-goldens.md`.

## Invariants / Gotchas

- **Byte-exact wire shapes everywhere**: `bench.rs`, `bench_client.rs`,
  `d7_probe.rs` all carry golden vectors locked against research and/or
  the web verifier. ANY change to a hash preimage, framing, or
  endianness here breaks the server verifier and rejects every
  submission silently. Always add a golden test before changing.
- **Protocol-version routing**: `bench_run::run` switches V1/V2/V3 paths
  off the server's response (`protocol_version` + presence of `k_b64`
  vs `server_challenge_blob`). The engine speaks all three; web flips
  the slice without forcing a re-cut.
- **`cold_enforced` is load-bearing**: it gates the lane override
  (warm => casual). A buffered fallback taints the whole run.
- **`f{:010}.bin` filename convention**: hardcoded in
  `answer_challenge_from_dir*`, `compute_rep_hashes_v3_1`,
  `scan_corpus_dir`, `parse_corpus_path_index`,
  `full_content_dedup`. Changing the format requires updating all of
  these and the web TS port.
- **Cancel propagation**: cancel must reach into the rayon worker
  (per-file Relaxed load) AND into the tar download (`CancelReader`).
  The trait surface uses a closure, bridged to an `AtomicBool` via the
  scoped propagator in `lib.rs` — the `cancel_propagator_*` tests pin
  this contract.
- **V3.1 hard-cutover**: post-v0.3.1, the V3 path emits ONLY
  `result_digest_v3_1` + `rep_hashes` + `k_echo`. The legacy V3
  `result_digest` field is no longer on the wire; `compute_rep_hashes_v3_1`
  failure is FATAL.
- **`/bench/dedup-ready` 409 retry**: not `already_stamped` — must
  honour `retry_after_ms` (web PR #11), bounded by MAX_SLEEP_MS=5s and
  MAX_RETRIES=3. Treating 409 as success leaves the no-anchor bypass
  open.
- **Self-verifying manifests**: `build_manifest` panics on plan/leaf
  count mismatch; `build_bench_proof` asserts every sample's audit path
  reconstructs the committed root before emitting.

## Dependencies

INCOMING:
- `superdeduper` (engine binary): `src/leaderboard/{mod.rs, bench.rs,
  bench_client.rs, bench_corpus.rs, bench_run.rs, d7_probe.rs,
  submission.rs}` are now re-export shims pointing here.
- `superdeduper::pipeline::hash` + `superdeduper::pipeline::io_threads_probe`
  call `superdeduper_bench_real::read_uncached` directly for the
  cold-enforced scan path.
- `superdeduper::main` calls `bench_corpus::served_manifest`,
  `build_manifest`, `compute_leaves`, `manifest_m`, `manifest_hash`.

OUTGOING:
- `superdeduper-bench-iface` — canonical wire types
  (`SubmissionInputs`, `BenchOutcome`, `BenchContext`,
  `BenchServices`, `BenchExecutor`, `SubmissionExecutor`,
  `CanonicalBench`, `ChallengePosition`, `ChallengeAnswer`,
  `InstallKey`, `RankEntry`, `SubmitOutcome`, `HardwareFingerprint`,
  `RunShape`, `ResultSummary`, `DebugDedupDiff`,
  `DebugDedupDiffReport`).
- `superdeduper-hmac-signer` — `canonical_body`, `sign`.
- `superdeduper-log` — `log_info!`, `log_warn!`.
- External crates: `blake3`, `chacha20`, `base64`, `serde`,
  `serde_json`, `sha2`, `hmac`, `anyhow`, `ureq` (json+tls), `uuid`
  (v4), `tar`, `rayon`. Unix-only: `libc`. Windows-only: `windows` 0.58
  (Win32_Foundation, Win32_Storage_FileSystem, Win32_System_IO).

## Refactor Hints

- **Phase 2 placeholder is stale**: `BenchReal { _phase_2_state: () }`
  (lib.rs line 66) was a Phase-1 scaffold. Phase 2/3 has landed; the
  zero-sized phantom field can probably go. The doc above it (lines
  60-63) talks about "future Phase 2 wiring" but Phase 3 has shipped
  real bodies already.
- **Orphaned doc block in `bench_run.rs`**: lines 482-527 are a single
  fused doc-comment block that ends up attached to
  `signal_dedup_ready` (line 528). The first 12 lines describe
  `read_uncached`, lines 494-495 describe `cold_bypass_reliable`, and
  the rest describes `signal_dedup_ready`. As written, `read_uncached`
  and `cold_bypass_reliable` (defined later in the file) carry NO doc
  comments because their docs are physically attached to the wrong
  function. Split this block back to the three intended targets.
- **`d7_probe` module-level claim is wrong** (lines 11-13): it says
  probe execution lives "in `bench_run.rs` as D7-B" and wire format
  lives "in `bench_client.rs` as D7-C". Neither is true — `execute_probes`
  is in `d7_probe.rs` itself (line 147), and there is no D7 wire code
  in `bench_client.rs`. `src/leaderboard/mod.rs` confirms no engine
  caller currently reaches `d7_probe`. Either wire D7-B/D7-C up or
  correct the docstring.
- **Disabled circular-dep test**: `bench_client.rs` line 910 has a
  `#[cfg(any())]` gated test that reaches
  `super::super::submission::SubmissionInputs` (which would create
  a `bench-real -> engine -> bench-real` cycle). Comment says it'll
  move engine-side "once Phase 2-B's submission move + call-site
  rewrites complete" — Phase 2-B has shipped (`submission_http.rs` is
  here); the test still hasn't relocated.
- **`use std::time::Duration` unused** at `lib.rs` line 93 — `Duration`
  is imported inside `run_bench` but never used in that body (it's used
  inside `spawn_cancel_propagator` which has its own import). Likely
  dead-import to clean up.
- **Several big `pub` items only used by tests**:
  `bench_corpus::build_bench_proof`, `build_bench_proof_from_dir`,
  `leaf_locations`, `BenchProof`, `SampleProof`, `LeafLoc`, `BENCH_SAMPLE_N`,
  `CorpusManifest`. Engine binary only consumes `served_manifest`,
  `build_manifest`, `compute_leaves`, `manifest_m`, `manifest_hash`,
  `parse_corpus_path_index` from this module (per
  `grep -rn '::<item>' src/`). The seed-derived `build_bench_proof`
  family is left over from the pre-server-direct-verify model. Confirm
  with `cargo +nightly rustc -- -W unused` per platform before
  removing — these are part of the cross-impl "Merkle reference"
  surface even if no engine call site touches them.
- **`bench_client::result_digest_bytes` / `_v2` / `_v3`** are only used
  by tests in this file. The `result_digest*` (b64) variants are the
  wire-shape callers reach. If we ever trim the V1/V2 surface as
  obsolete (post all-V3.1 cutover) the `_bytes` helpers can go with
  them.
- **`bench_corpus::dedup_efficiency`** is defined here but called
  only from tests. Look for an engine-side or web-side consumer
  before pruning.
- **`bench_client::file_raw_hash`** is `pub` but only consumed inside
  the crate (by `compute_rep_hashes_v3_1`) and from tests. Could be
  `pub(crate)`.
- **`scan_corpus_dir` returns `Vec<(u64, PathBuf, u64)>`** — a named
  struct (`CorpusFileEntry`) would be easier to read than the bare
  tuple, especially in the `build_bench_proof_from_dir` consumer.
- **Test-only `hex32` helper** is duplicated across `bench.rs` (line
  445) and `bench_client.rs` (line 635). Could live in a shared
  `tests/util.rs` if a test-only common module is worth it.

## Wire Surfaces

HTTP endpoints (driven by `bench_run::run` + `submission_http::submit_inner`):
- `POST {server_url}/api/v1/bench/start` — HMAC-authed; request:
  `{install_id, corpus_version, tier, protocol_version}`. Response:
  `{bench_run_id, download_url, protocol_version, corpus_version,
  tier, challenges, server_challenge_blob?, k_b64?}`.
- `GET {download_url}` — tar stream (S3 or web-served), wrapped in
  `CancelReader` for mid-stream abort.
- `POST {server_url}/api/v1/bench/dedup-ready` — HMAC-authed; body
  `{install_id, bench_run_id}`. Responses: 200 `{dedup_start_ts}` or
  `{existing_ts}`, or 409 `{retry_after_ms}`.
- `POST {server_url}/api/v1/submit` — HMAC-authed; canonical-bench
  body shape defined by `submission_http::build_payload`. Bench-mode
  body lifts `protocol_version`, `corpus_version`, `tier`,
  `bench_run_id`, `bench_proof`, `cold_enforced` to top level.

`bench_proof` JSON shape (V3.1, post-cutover):
```
{ "answers": [{path_index, byte_offset, byte_length, challenge_hash}*],
  "result_digest_v3_1": "<b64>",
  "rep_hashes": ["<b64>", ...],
  "k_echo": "<b64 of K>" }
```
V2 emits `result_digest` + `challenge_blob_echo`; V1 emits only
`answers` + `result_digest`.

On-disk format versions:
- `corpus-sample` (1030 files, ~34 MB) — cross-validation fixture.
- `corpus-v1-quick` (~121,701 files, ~2.53 GB) — `--bench-me` default.
- `corpus-v2-full` (~1,001,801 files, ~6.25 GB) — competitive
  Hall-of-Fame tier.
- `corpus-v3-*` — V3-mutate corpora (engine routes off
  `corpus_version` prefix to protocol_version=`v3.1-mutate`).
- Cache sentinel: `{workroot}/sd-bench-corpus-{slug}/.sd-bench-complete`
  (presence => corpus is cached; `--fresh` forces re-download).

Environment variables (test-only):
- `SD_BENCH_DISKLOOP_DIR` — override path for the heavy disk-loop
  `#[ignore]`'d test in `bench_run.rs`.
- `RECOMPUTE_GOLDENS` — referenced in `d7_probe.rs` comments for the
  golden-vector regeneration protocol.

CLI flags this dir effectively owns (via the engine's `--bench-me`):
- `--bench-me` (engine `main.rs`) drives `bench_run::run`.
- `--fresh` toggles cache reuse vs. forced re-download.
- `--keep` (referenced in `run`'s docstring): currently the corpus dir
  is the persistent cache and is intentionally NOT removed.
- `sd debug dedup-diff` (engine) drives `BenchReal::debug_dedup_diff`.
- `--cold-enforced` on `sd scan` reuses `read_uncached` (via the lib.rs
  re-export).
