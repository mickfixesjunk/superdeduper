# debug — AGENTS guide

## Purpose
Hosts the `sd debug ...` family of read-only subcommands. These are engine-internal state dumpers used by the containment-integration test harness (testdesign) and by ad-hoc auditing. Distinct from `sd diagnose`, which is the user-facing triage path for performance / inventory issues on a specific scan.

Currently only the snapshot helper lives here. The module is structured to expand as the containment-integration spec grows (e.g. `sd debug inode-resolve <path>`).

## Files

### `mod.rs`
Tiny re-export hub. Declares `pub mod snapshot;` and documents intent. No items beyond the module declaration.

### `snapshot.rs`
Implements `sd debug snapshot <path>`. Walks a path tree recursively (without following symlinks), captures stable per-entry metadata (inode, nlink, size, content hash, mtime_ns, ACL hash, reparse-point flags), and emits the JSON shape locked in `testdesign/specs/containment-fixtures/_snapshot-schema-examples.md`. The harness diffs pre- and post-action snapshots against per-action expected deltas; the only contract is that the engine emits consistent values for identical inputs.

Public API:
- `const SCHEMA_VERSION: &str` — `"superdeduper.snapshot.v1"`, emitted as the top-level `schema` field.
- `struct Snapshot` — top-level JSON shape (schema, root_path, captured_at, filesystem, mtime_precision, entries).
- `enum MtimePrecision` — `Ns | Us | Ms | S`, lowercase-serialized.
- `struct SnapshotEntry` — per-entry record matching the wire schema.
- `enum EntryType` — `File | Directory | Symlink`, lowercase-serialized.
- `struct AdsEntry` — Windows alternate-data-stream entry (always empty in V1).
- `fn capture(root: &Path) -> io::Result<Snapshot>` — walk + build a deterministic snapshot.
- `fn write_json<W: io::Write>(snapshot: &Snapshot, w: W) -> io::Result<()>` — pretty-print + newline-terminate to a writer.

Who calls this:
- `src/main.rs:533` (the `Debug { Snapshot { .. } }` CLI branch) calls `snapshot::capture` then `write_json`.
- Internal `#[cfg(test)]` suite exercises every shape invariant.
- No other in-crate callers.

Key invariants:
- Entries are sorted by `path` UTF-8 bytes ascending (capture() at snapshot.rs:131).
- Inodes formatted as `0x` + 16 hex chars (zero-padded u64).
- ACL hash is SHA-256 hex of a platform-specific canonical buffer: `posix:` + mode||uid||gid little-endian on unix; `win:` + file_attributes LE on windows.
- Reparse points: content hash + ADS enumeration are explicitly skipped to protect cloud-placeholder safety; size is still reported.
- Symlinks are never followed; only their target string is captured.
- `captured_at` is built from a hand-rolled civil-from-days routine (Hinnant) — a chronos/time crate dependency is deliberately avoided.

Feature gates: none. The platform splits are `#[cfg(unix)]`, `#[cfg(windows)]`, and a `#[cfg(not(any(unix, windows)))]` fallback for inode/nlink/attrs, ACL hash, and reparse-point detection.

## Invariants / Gotchas
- The output JSON is a wire contract with the containment harness. The schema-examples file in `testdesign/specs/containment-fixtures/` is the source of truth for field shape; bump `SCHEMA_VERSION` if any breaking shape change ships.
- Determinism comes from two places: sorted children inside `walk_recursive` (line 152) and a final sort on `entries` by path bytes in `capture` (line 131). Both are required — directory `read_dir` ordering is not stable across filesystems.
- `acl_hash` only covers `mode|uid|gid` (unix) or `file_attributes` (windows). DACL-entry changes on Windows will not move it; that is a documented V1 limitation, not a bug.
- `mtime_to_ns` returns `0` if the `modified()` call fails, masking pre-epoch errors. Tests rely on this not panicking; do not change to `?` without updating callers.
- Windows codepath relies on `OPEN_NO_RECALL | OPEN_REPARSE_POINT | BACKUP_SEMANTICS`. Removing any of these would hydrate cloud placeholders (data loss / cost) or fail on directories.
- `rfc3339_now` and `days_to_ymd` are hand-rolled copies of the routine in `action_receipt`; if you change one, change both (comment at line 261).

## Dependencies
- INCOMING: `src/main.rs` (CLI dispatch only).
- OUTGOING: `serde`, `sha2`, `std::fs`, `std::time`, `windows` crate (windows-only). No intra-crate dependencies — this module is self-contained.

## Refactor Hints
- The `rfc3339_now` / `days_to_ymd` pair is duplicated from `action_receipt`. Worth lifting into a small `crate::util::time` if a third copy ever appears.
- `detect_filesystem` returns a coarse static string per OS; the `_root: &Path` argument is unused and could be dropped, or wired to real introspection (statfs / `GetVolumeInformationW`).
- `walk_recursive` is recursive and unbounded — risk of stack overflow on pathological trees. Inventory's walker uses an explicit queue; could be aligned if this command ever runs on huge corpora.
- `capture()` does `root.canonicalize().or_else(|_| Ok(root.to_path_buf()))` to defer not-found errors to the walker. This works but is subtle — the comment at lines 122-128 explains why.
- Suspect dead code: none. All `pub` items are used by `main.rs` or by the cfg-test suite. `grep -rn "debug::snapshot\|snapshot::capture" --include=*.rs` confirms.
- The `#[cfg(not(any(unix, windows)))]` fallbacks for `platform_inode_nlink_attrs` / `platform_acl_hash` are unreachable in supported builds; harmless but worth noting if cfg cleanup ever happens.

## Wire Surfaces
- CLI: `sd debug snapshot <path>` (dispatched from `src/main.rs:533`).
- JSON schema: `superdeduper.snapshot.v1`, locked against `testdesign/specs/containment-fixtures/_snapshot-schema-examples.md`.
- No HTTP, env-var, or on-disk format owned here beyond the emitted JSON.
