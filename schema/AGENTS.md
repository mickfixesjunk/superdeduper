# schema/

## Purpose

Holds the engine-owned canonical JSON Schema for the
`POST /api/v1/submit` payload sub-objects (`hardware`, `run_shape`,
`result_summary`). The schema is auto-generated from the Rust wire
structs via `schemars` — DO NOT hand-edit. Regenerate by running:

```
SD_UPDATE_SCHEMA=1 cargo test
```

This file is consumed by the web backend (`api.superdeduper.io`) as the
source-of-truth for what shape the engine emits, and by the engine's
own `schema_regen` test as a regression gate against unintended wire
drift. The schema's existence is tracked under issue #144.

## Files

- `submit.schema.json` — draft-2019-09 JSON Schema with three
  `definitions` (`HardwareFingerprint`, `RunShape`, `ResultSummary`)
  and three top-level `properties` ($ref'd from the definitions).
  Title: `superdeduper.submit.wire-structs`.

## Invariants

- Schema is auto-generated, NOT hand-written. Any direct edit will be
  clobbered by the next `SD_UPDATE_SCHEMA=1` regen run.
- Field shape is dictated by the backend's Zod schema at
  `api.superdeduper.io/api/v1/submit/schema.json`; the engine must
  emit exactly the keys listed in each `required` array because the
  backend enforces `additionalProperties: false` (note: the JSON
  schema in this directory does NOT explicitly carry that constraint —
  see Findings).
- The three definitions mirror Rust structs that live in
  `crates/superdeduper-bench-iface/src/lib.rs` (NOT
  `src/leaderboard/payload_meta.rs` as one might infer from the
  schema's own callouts — payload_meta.rs holds helper accumulators
  and classifier functions only).
- The engine's `leaderboard` module re-exports the structs via
  `pub use` from the bench-iface crate for back-compat with existing
  call sites.
- Action-bytes keys in `result_summary.actions_taken_summary` are
  LOCKED to the strings `deleted_to_recycle_bytes`,
  `deleted_permanently_bytes`, `hardlink_replaced_bytes`. Web's
  `lifetime-audit.ts` will reject other keys. Constants live engine-
  side as `ACTION_BYTES_KEY_*` in `leaderboard::submission`.
- The `is_dev_drive` field is intentionally absent from
  HardwareFingerprint's `required` array because the Rust struct has
  `#[serde(default)]` — old engine submissions remain readable.
- Optional `Option<T>` fields on the Rust side (cache_hit_ratio,
  zero_byte_group_max, max_hardlink_count_in_scan,
  name_collision_count, share_count_in_scope, dry_run,
  groups_reviewed_count, placeholder_skip_count, placeholder_skip_bytes,
  client_found_dupsets) appear in JSON as nullable unions
  (`"type": ["integer","null"]` etc.) and are NOT in `required`.

## Dependencies

Inbound (who reads this file):

- Engine schema-regen test (writes the file on
  `SD_UPDATE_SCHEMA=1`, otherwise compares it byte-for-byte against
  schemars output to detect wire drift).
- Web backend (`web/api/v1/submit/schema.json`) — separate file but
  must stay shape-compatible; cross-track sync is manual today.
- Anyone building `/submit` payloads outside the canonical
  `leaderboard::submission` builder (currently only bench-real, which
  uses the same structs).

Outbound (what this file references):

- `#/definitions/HardwareFingerprint`, `#/definitions/RunShape`,
  `#/definitions/ResultSummary` (all internal `$ref`).

Source-of-truth Rust structs:

- `crates/superdeduper-bench-iface/src/lib.rs:289`
  `HardwareFingerprint` (schemars feature-gated under `telemetry`).
- `crates/superdeduper-bench-iface/src/lib.rs:383` `RunShape`.
- `crates/superdeduper-bench-iface/src/lib.rs:456` `ResultSummary`.

## Refactor Hints

- If you change a field on any of the three structs in
  `superdeduper-bench-iface`, you MUST regen this schema. The CI
  regen-check will otherwise fail.
- If you ADD a field, decide explicitly whether it goes in `required`
  (no `#[serde(default)]` / not `Option`) or stays optional. Optional
  is the safer back-compat choice for esoteric achievement metrics.
- The action-bytes keys are a wire contract with web. Adding a new
  action class (e.g. `moved_bytes`) requires a coordinated web schema
  bump + engine constant addition; see the `ACTION_BYTES_KEY_*` block
  in `leaderboard::submission`.
- The schema is currently `draft/2019-09`. Bumping to `draft/2020-12`
  needs schemars version coordination + a web-side validator check.
- The schemars `JsonSchema` derive is `#[cfg_attr(feature =
  "telemetry", ...)]` — regeneration only works in a build that has
  the `telemetry` feature on the bench-iface crate.

## Wire Surfaces

Top-level `/submit` payload exposes three keys this schema covers:

```
{
  "hardware":       { ...HardwareFingerprint },
  "run_shape":      { ...RunShape },
  "result_summary": { ...ResultSummary }
}
```

`HardwareFingerprint.required` (11 keys, `is_dev_drive` excluded):

```
cluster_size_kb, cpu_cores, cpu_isa_flags, cpu_model_string,
cpu_threads, disk_class, filesystem, os_edition, os_version,
ram_total_gb_bucket, volume_size_gb_bucket
```

`RunShape.required` (8 keys; everything else is nullable / optional):

```
bytes_scanned, corpus_kind, features_used_bitmap, files_scanned,
hash_algorithm, scope, walker_variant, wall_clock_seconds
```

`ResultSummary.required` (4 keys):

```
actions_taken_summary, duplicate_bytes_reclaimable,
duplicate_groups, largest_single_group_bytes
```

Locked enum sets (any change is a cross-track wire bump):

- `hash_algorithm`: `river5-aes-ni | river5-96 | blake3 | sha256 | other`
- `walker_variant`: `mft | walker | hybrid`
- `scope`: `whole-volume | subdirectory | selection | canonical-bench`
- `corpus_kind`: `user-data | system | canonical-bench`
- `disk_class`: `NVMe-Gen5 | NVMe-Gen4 | NVMe-Gen3 | SATA-SSD | HDD |
  USB-SSD | USB-HDD | network | mixed`
- `filesystem`: `NTFS | ReFS | exFAT | FAT32 | network-SMB | APFS |
  HFS+ | ext4 | btrfs | xfs | zfs | other` (APFS/HFS+/ext4/btrfs/xfs/
  zfs added 2026-06-08 at web commit 47b4419)
- `os_edition`: `Home | Pro | Enterprise | Education | Server | Other`
- `ram_total_gb_bucket`: `4 | 8 | 16 | 32 | 64 | 128 | 256 | 512 | 1024`
- `volume_size_gb_bucket`: power-of-two `1`..`32768`

Action-bytes keys (LOCKED):

```
deleted_to_recycle_bytes
deleted_permanently_bytes
hardlink_replaced_bytes
```

CPUID ISA flag pattern (backend regex): `^[a-z0-9-]{1,20}$`, sorted.
