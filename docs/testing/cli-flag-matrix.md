# CLI flag-matrix stress-test (issue #151)

Seed enumeration of every CLI flag + subcommand exposed by
`src/cli.rs`, structured so testrunner (Linux) and sdd-testwin
(Windows) can check off rows as the matrix is exercised. Issues
landed against rows update the **Status** column in-place.

> **Owners** per #151 body:
> - **testdesign** owns full row enumeration (this doc is the
>   engine-side starter; testdesign curates as the matrix evolves).
> - **testrunner** (Linux) + **sdd-testwin** (Windows) own
>   execution.
> - **engine** (this repo) owns fixes for any RED row.

## Status legend

| Symbol | Meaning |
|--------|---------|
| 🟢 | GREEN -- verified in a recent matrix run |
| 🔴 | RED -- known issue, tracked via F-CLI-N or a GitHub issue |
| 🟡 | PENDING -- fix committed, re-verification queued |
| ⚫ | UNVERIFIED -- never run in the matrix yet |

## Findings to-date

Cross-reference with #151's body. The findings listed here mirror
the umbrella tracker; this section is the engine-side mirror so a
reader can see the full pulse from one doc.

| ID | Row / context | Status | Notes |
|----|---------------|--------|-------|
| F-CLI-1 | `ImageHashAlgoArg` default | 🟢 GREEN | Fixed v0.2.27 (clap default matches `#[default]`). |
| F-CLI-2 | global `--quiet` silences scan group-listing + Summary | 🟡 PENDING | Fix in `fix/F-CLI-2-quiet-suppression`; ships v0.2.29. Re-run row. |
| F-CLI-3 | submit-path | 🟢 GREEN | After #149 + #150. |
| F-CLI-4 | `dedupe --allow-system-paths` + `\\?\` verbatim | 🟢 GREEN | Fixed v0.2.27; sdd-testwin re-verified 3/3 legs. |
| F-CLI-5 | `is_dev_drive` reads false on Win11 26200 ReFS Dev Drive | 🔴 RED | Engine detection rework pending (classic FSCTL returns VolumeFlags=0x0 -- flag not exposed there). Candidate impl note appended below. |

## Top-level (`Cli`) flags

Every subcommand inherits these via `global = true`.

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `-v` / `--verbose` | count (0..3) | 0 | `-v` info, `-vv` debug, `-vvv` trace | ⚫ |
| `-q` / `--quiet` | bool | false | `conflicts_with = "verbose"` | 🟡 (F-CLI-2 re-verify) |
| `--channel <NAME>` | `Option<String>` | env / config | `prod` / `dev` / `local` | ⚫ |

## Subcommands

### `scan` (`ScanArgs`)

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `<PATHS>...` | positional `Vec<PathBuf>` | required unless `--list-exclusion-packs` | Multiple roots OK | ⚫ |
| `--reference <PATH>` | `Vec<PathBuf>` | empty | Repeat for N refs | ⚫ |
| `--min-size <BYTES>` | size string | `4K` | Suffixes K/M/G/T | ⚫ |
| `--tier1-bytes <BYTES>` | size string | `4K` | Experimental knob | ⚫ |
| `--max-size <BYTES>` | `Option<String>` | none | Suffixes K/M/G/T | ⚫ |
| `--include <GLOB>` | `Vec<String>` | empty | Repeat | ⚫ |
| `--exclude <GLOB>` | `Vec<String>` | empty | Repeat | ⚫ |
| `--format <FORMAT>` | enum | `text` | `text` / `json` / `csv` / `report` | ⚫ |
| `--no-cache` | bool | false | Disable persistent cache for this run | ⚫ |
| `--no-format-aware` | bool | false | Disable Tier-0 fingerprints | ⚫ |
| `--threads <N>` | `Option<usize>` | logical CPUs | | ⚫ |
| `--io-threads <N>` | `Option<usize>` | threads x 3 | Sweep `1` -> `64` for saturation | ⚫ |
| `-o` / `--output <FILE>` | `Option<PathBuf>` | stdout | | ⚫ |
| `--follow-links` | bool | false | Follow reparse points / symlinks | ⚫ |
| `--allow-system-paths` | bool | false | Permit system-critical paths | ⚫ |
| `--exclusions <STATE>` | enum | `on` | `on` / `off` | ⚫ |
| `--exclusion-pack-disable <ID>` | `Vec<String>` | empty | Repeat | ⚫ |
| `--exclusion-pack <ID>` | `Vec<String>` | empty | Repeat | ⚫ |
| `--list-exclusion-packs` | bool | false | Print packs + exit (bypasses scan) | ⚫ |
| `--placeholders-only` | bool | false | Skip stages 2-4 | ⚫ |
| `--force-hash` | bool | false | Diagnostic: hash every file via Tier 3 | ⚫ |
| `--allow-recall-on-read` | bool | false | Permit cloud-placeholder hydration | ⚫ |
| `--hash-algo <ALGO>` | enum | `river5` | `river5` (default) / `blake3`. Aliases: `ddh128`, `river128`. | ⚫ |
| `--mode <MODE>` | enum | `exact` | `exact` / `image` / `audio` | ⚫ |
| `--image-similarity-threshold <BITS\|auto>` | int or `auto` | `5` | E3 auto-scale via `auto` | ⚫ |
| `--image-hash-algorithm <ALGO>` | enum | `dhash` | `dhash` / `phash` / `ahash` | F-CLI-1 GREEN |
| `--audio-similarity-threshold <BITS>` | f64 | `5.0` | Average per-chunk Hamming | ⚫ |

### `dedupe` (`DedupeArgs`)

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `<RESULTS_FILE>` | positional `PathBuf` | required | Output of `scan` | ⚫ |
| `--strategy <STRATEGY>` | enum | `smart` | `oldest`/`newest`/`shortest-path`/`longest-path`/`in-reference`/`first`/`smart`. (`interactive` hidden per F-CLI-6.) | ⚫ |
| `--action <ACTION>` | enum | `recycle` | `remove` / `recycle` (alias `trash`, #159) / `hardlink` / `reflink` / `safe-rename` | ⚫ |
| `--mode <MODE>` | enum | `exact` | `exact` / `image` / `audio` | ⚫ |
| `--dry-run` | bool | false | Print what would happen, do nothing | ⚫ |
| `--allow-system-paths` | bool | false | Permit destructive ops under system paths | F-CLI-4 GREEN |
| `--allow-destructive-on-deduped` | bool | false | Permit ops against IO_REPARSE_TAG_DEDUP | ⚫ |
| `--integration-test-mode` | bool | false | Emit NDJSON receipts | ⚫ |
| `--receipt-file <PATH>` | `Option<PathBuf>` | stdout | `requires = "integration_test_mode"` | ⚫ |

### `cache` (`CacheCommand`)

| Subcommand | Args | Notes | Status |
|------------|------|-------|--------|
| `info` | -- | Show cache statistics | ⚫ |
| `clear` | -- | Wipe the cache | ⚫ |
| `vacuum` | -- | VACUUM the cache DB | ⚫ |

### `drive-info`

| Flag | Notes | Status |
|------|-------|--------|
| (no args) | Windows-only; bus type, seek-penalty IOCTL, final disk_class | F-CLI-5 RED (Win11 ReFS Dev Drive) |

### `diagnose` (`DiagnoseArgs`)

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `<PATH>` | positional `Option<PathBuf>` | system temp | Probe target | ⚫ |
| `--format <FORMAT>` | enum | `text` | `text` / `json` | ⚫ |
| `-o` / `--output <FILE>` | `Option<PathBuf>` | stdout | | ⚫ |
| `--skip-io` | bool | false | Skip Tier-3 sequential-read probe | ⚫ |

### `register` (telemetry-gated, `RegisterArgs`)

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `--reset` | bool | false | Wipe `install.json` + re-register | ⚫ |
| `--server-url <URL>` | `Option<String>` | `https://api.superdeduper.io` | Override backend | ⚫ |
| `--print-captcha-url` | bool | false | Print captcha URL + exit (no PoW) | ⚫ |

### `config` (telemetry-gated, `ConfigCommand`)

Subcommands per `src/cli.rs:1034`. Enumerate when sd-testwin walks
the row.

| Subcommand | Notes | Status |
|------------|-------|--------|
| (see `ConfigCommand` enum at cli.rs:1034) | Print / update local share preference, channel, etc. | ⚫ |

### `achievements` (telemetry-gated, `AchievementsCommand`)

| Subcommand | Flags | Notes | Status |
|------------|-------|-------|--------|
| `list` | `--format <FORMAT>`, `--all` | Lists granted (or all when `--all`) | ⚫ |
| `refetch` | `--quiet` | Force fresh `/api/v1/profile/{install_id}` | ⚫ |
| `verify` | (see enum) | Bumps `verify-veteran` counter | ⚫ |
| `show` / `diff` / `anchor` | -- | v0.1.9 future surface | ⚫ |

### `account` (telemetry-gated, `AccountCommand`)

| Subcommand | Flags | Notes | Status |
|------------|-------|-------|--------|
| `link <PROVIDER>` | `--timeout-secs <SECS>` (default 300) | `google` or `discord` | ⚫ |
| `unlink` | -- | Delete OAuth token + revoke server-side | ⚫ |
| `status` | `--format <FORMAT>` | Anonymous vs Linked | ⚫ |
| `nickname get` | `--format <FORMAT>` | Read display_name | ⚫ |
| `nickname set <NAME>` | `--yes` | Backfill: rewrites past leaderboard rows | ⚫ |

### `submit-pending` (telemetry-gated, `SubmitPendingArgs`)

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `--channel <NAME>` | `Option<String>` | all | Only drain rows for this channel | ⚫ |
| (see SubmitPendingArgs at cli.rs:136) | | | Idempotent on already-submitted rows | ⚫ |

### `bench-me` (telemetry-gated, `BenchMeArgs`)

| Flag | Type / arity | Default | Notes | Status |
|------|--------------|---------|-------|--------|
| `--corpus-version <VERSION>` | string | `corpus-v2-quick` | A-cli-default-corpus-v2 hotfix; v3 queued | ⚫ |
| `--tier <TIER>` | string | `quick` | `/bench/start` tier label | ⚫ |
| `--fresh` | bool | false | Force fresh corpus download | ⚫ |
| `--workdir <DIR>` | `Option<PathBuf>` | system temp | Use real disk (avoid tmpfs RAM-backing) | ⚫ |
| `--lane <LANE>` | `Option<enum>` | persisted | `ranked` / `casual` | ⚫ |
| `--no-deep-link` | bool | false | Suppress post-bench browser open | ⚫ |

### `scan-history` (`ScanHistoryCommand`)

| Subcommand | Flags | Notes | Status |
|------------|-------|-------|--------|
| `list` | `--format <FORMAT>` | Table or JSON | ⚫ |
| `delete <SCAN_ID>` | -- | Idempotent | ⚫ |
| `resubmit` (telemetry-gated) | `<SCAN_ID>` or `--pending` (mutually exclusive) | Resubmit one row or drain pending | ⚫ |
| `prune <DAYS>` | -- | 0 = noop sentinel | ⚫ |

### `debug` (`DebugCommand`)

| Subcommand | Flags | Notes | Status |
|------------|-------|-------|--------|
| `snapshot <PATH>` | `--format <FORMAT>` (json only), `--out <FILE>` | Containment-test snapshot schema | ⚫ |
| `make-bench-corpus` (telemetry-gated) | `--tier`, `--out <DIR>`, `--seed <HEX>`, `--print-merkle-root` | Dev/server only | ⚫ |

## Row-execution conventions

When executing a row:

1. Run the canonical command shape against a known fixture corpus
   (e.g., `tests/fixtures/cli-flag-matrix/<row-id>/`).
2. Compare actual output (stdout / stderr / exit code / on-disk
   side effects) against the expected snapshot.
3. On mismatch: flip the **Status** column to 🔴 with a tracking
   ID (`F-CLI-N` continuing the series, or a new GitHub issue
   number). Open the issue with a minimal repro + the expected
   vs actual diff.
4. On pass: flip to 🟢.
5. Issues with engine-side fix committed but verification pending
   become 🟡 with a release-version note.

Combinatorial rows (flag pairs / triples): start with
**negative-shape** combinations the issue body hints at -- e.g.,
`--no-cache` AND `--placeholders-only` together; `--mode image`
AND `--no-format-aware`; `--dry-run` AND `--integration-test-mode`
together. Document each combination once.

## F-CLI-5 candidate engine impl (Win11 ReFS Dev Drive detection)

> **NOT an action item from #151 -- engine-side candidate note
> for the F-CLI-5 row above so testdesign / sdd-testwin can
> coordinate verification when Mick's NEO box is available.**

Per #151 body: the classic
`FSCTL_QUERY_DEVELOPER_VOLUME_STATE` IOCTL returns VolumeFlags=0x0
on Win11 build 26200 ReFS Dev Drives because the "is dev drive"
flag is no longer exposed on that path. Candidate engine
detection paths:

1. **Query `FileFsAttributeInformation` for `FILE_SUPPORTS_DEV_VOLUME`**
   on a handle to the volume root. If present in the attribute
   mask, treat as Dev Drive.
2. **`NtQueryVolumeInformationFile`** with
   `FileFsControlInformationEx` (Win11+) -- may surface the
   developer-volume flag under a different bit.
3. **WMI fallback:**
   `Win32_Volume.DriveType = 3` + a heuristic that the volume is
   ReFS AND the user's developer mode is enabled. Less precise.

(1) is the cleanest replacement -- it stays in the same
`winapi_wrappers::query_storage_device` family the rest of
disk_class detection lives in. The fix would land as a new arm
in `disk_class` resolution: after the classic FSCTL returns
VolumeFlags=0x0, fall through to (1) before deciding "not a Dev
Drive."

Verification needs Mick's NEO box (Win11 26200 ReFS Dev Drive)
plus a non-Dev Drive Win11 baseline so the new arm doesn't
false-positive elsewhere.

## Maintenance

When adding a new CLI flag or subcommand to `src/cli.rs`:

1. Add a row to the relevant table in this doc.
2. Initialize **Status** to ⚫ UNVERIFIED.
3. testdesign queues a row-execution slot in the next matrix
   sweep.

When a row's behavior changes (fix landed, semantic change):

1. Update the **Notes** column.
2. Bump **Status** to 🟡 PENDING until the next verification
   sweep flips it to 🟢 or 🔴.

This doc lives at `docs/testing/cli-flag-matrix.md`; ownership
is engine-side for the row enumeration (this commit), testdesign-
side for the test-spec layer (to be authored).
