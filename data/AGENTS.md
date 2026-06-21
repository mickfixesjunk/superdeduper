# data/

## Purpose

Vendored static-data snapshots that the engine bundles at compile time via
`include_str!`. Lets GUI + CLI render bracket / catalog metadata offline
with no network round-trip. Snapshots intentionally lag the live web
catalog by ~one engine release.

## Files

- `cpu-brackets-catalog.json` — frozen snapshot of the live CPU bracket
  classifier catalog. Mirrors `api/data/cpu-brackets-catalog.yaml` in
  the web repo. Consumed via `include_str!` from
  `src/leaderboard/cpu_brackets.rs:20`.

## Invariants

- Top-level shape: `{ version: "v1", classifier_version: u32, brackets: [...] }`.
  Must deserialize cleanly into `Catalog` (`src/leaderboard/cpu_brackets.rs:23`)
  or `catalog()` panics on first access (line 66, hard `.expect`).
- `classifier_version` is currently `4` (line 3). Bumped lock-step with
  the web-side classifier (`api/src/buckets/hardware-class.ts`, commits
  `65b91d1` + `acfc3f8` per `cpu_brackets.rs:3-4`).
- Exactly five brackets: `flagship`, `high-end`, `mid-range`, `older`,
  `legacy`, with `display_order` 1..5 (a sixth wire id `"unknown"` is
  engine-synthesized — `cpu_brackets.rs:43-46` — not present in JSON).
- Each bracket carries `id`, `display_name`, `display_order`, `intent`,
  `examples`, `patterns`. `patterns` are regex `.source` strings,
  compiled case-insensitive at classify time (`cpu_brackets.rs:38-40`).
- Live endpoint of record: `GET /api/v1/cpu-brackets/catalog`.

## Dependencies

- Compile-time only: pulled in by `include_str!` in
  `src/leaderboard/cpu_brackets.rs`. No runtime fetch path.
- Referenced in `Cargo.toml:60` comment.
- Bracket id reused by diagnose output (`src/diagnose.rs:102`, #217).

## Refactor Hints

- New brackets / new CPU patterns: update web YAML first, ship web
  release, then re-export JSON snapshot here and bump engine release.
  Same staleness model as the achievements catalog
  (see `[[reference_achievements_catalog_yaml]]` memory).
- Adding a new bracket id does NOT require engine recompile of match
  logic — `BracketId` is `String` (`cpu_brackets.rs:47-48`) so GUI + CLI
  pick up new ids automatically; only display ordering / sort assumes
  contiguous `display_order`.
- Pattern format is intentionally permissive (e.g. `apple\s*m[234]\s*(max|pro)?`
  at line 57 matches `apple m3` alone — bracket overlap is resolved by
  iteration order in the classifier, not by this file).

## Wire Surfaces

- File schema mirrors `GET /api/v1/cpu-brackets/catalog` response body.
- Public reference page: `<leaderboard-url>/brackets`.
- Bracket `id` strings appear in leaderboard rows, diagnose output,
  and GUI bracket badges. Renames are breaking changes for historical
  rows.
