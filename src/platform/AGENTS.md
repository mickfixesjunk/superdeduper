# platform — AGENTS guide

## Purpose
`src/platform/` is the OS-abstraction boundary for superdeduper. Cross-platform
code (`dedupe`, `gui`, `leaderboard`, `main`) calls free functions defined in
`mod.rs`; that file is the single place `#[cfg(...)]` routing happens, and each
target OS has its own sibling file or submodule (`linux/`, `windows.rs`,
`macos.rs`).

Design choice (see `mod.rs` doc-comment header): free functions per platform,
not a trait. Rationale: matches existing cfg patterns in the rest of the
codebase, no runtime polymorphism need, integration tests run on the real OS
so mock impls aren't valuable. Decision is reversible — converting to a trait
would be mechanical.

Today the module owns three OS-level capabilities (reflink/clone-on-write copy,
trash/recycle, default-browser URL launch) plus a fourth "per-OS terminology"
helper set (`trash_bin_noun`, `trash_action_verb`, `move_to_trash_phrase`)
introduced for issue #159 so every user-facing label says "Trash" on
Linux/macOS and "Recycle Bin" on Windows without each call site re-writing the
cfg ladder. Linux additionally has a `mount_info` submodule that parses
`/proc/self/mounts` to surface per-root filesystem warnings (ZFS pool-dedup,
LUKS, network mounts, reflink-capable FS).

## Files

### `mod.rs`
Top-level routing layer and public API. All cross-platform call sites depend on
the symbols defined here.

- Public API:
  - `pub enum PlatformError` — `Unsupported(&'static str)` / `Io(io::Error)` /
    `Other(String)`. Implements `Display` + `Error` + `From<io::Error>`.
  - `pub type PlatformResult<T> = Result<T, PlatformError>`
  - `pub struct TrashOutcome { original_path, container, info_file, data_file }`
    — optional fields populated by Linux; Windows/macOS return defaults today.
  - `pub fn clone_file(src, dst) -> PlatformResult<()>` — cfg-routed to
    `linux::reflink::clone_file` / `windows::clone_file` /
    `macos::clone_file`; `Unsupported` on other targets.
  - `pub fn trash_file(path) -> PlatformResult<TrashOutcome>` — same routing
    pattern.
  - `pub fn open_url(url) -> PlatformResult<()>` — same routing pattern.
  - `pub const fn trash_bin_noun() / trash_action_verb() / move_to_trash_phrase()`
    — per-platform vocab strings for UI labels.
  - `pub mod linux` — only re-exported on `target_os = "linux"`.
- Callers: `crate::dedupe` (clone_file, trash_file, TrashOutcome), GUI widgets
  (`bench_modal`, `alpha_warning`, `scan_history_panel`, `settings_modal`,
  `app`, `live`), `crate::leaderboard::{captcha, oauth}` (open_url), `main.rs`
  (open_url, mount_info via `linux::mount_info`).
- Tests: `trash_vocab_tests` pins the per-OS vocab strings; includes a
  cross-platform consistency test so noun/verb/phrase can't drift into mixed
  families.
- Feature gates: `#[cfg(target_os = "linux")]`, `#[cfg(windows)]`,
  `#[cfg(target_os = "macos")]`, plus a `not(any(...))` fallback that yields
  `Unsupported`.

### `windows.rs`
Routing shim that delegates Windows impls to `crate::winapi_wrappers`. This
file deliberately holds zero Win32 code today — the actual FSCTL /
IFileOperation wrappers live in `winapi_wrappers::{recycle, replace_with_reflink}`.

- Public API:
  - `pub fn clone_file(src, dst)` → `winapi_wrappers::replace_with_reflink(dst, src)`
    (note arg order swap — `replace_with_reflink` takes `(target, keeper)`).
  - `pub fn trash_file(path)` → `winapi_wrappers::recycle(path)`.
  - `pub fn open_url(url)` — `ShellExecuteW("open", url, ...)`. Bypasses
    `cmd /c start` to avoid the `&` mangling that broke the G1 captcha flow.
- Compiled only on `#[cfg(windows)]` via `mod.rs`.

### `macos.rs`
Stub impls. `clone_file` and `trash_file` return `PlatformError::Unsupported`
with an "L3 roadmap" message; `open_url` spawns `/usr/bin/open`. The file lets
the platform module compile cleanly on macOS hosts during L0/L1 phases.

### `linux/mod.rs`
Linux impl entry point. Declares submodules `mount_info`, `reflink`, `trash`
(all `pub mod`) and implements `open_url` inline (spawns `xdg-open`,
non-blocking; failure surfaces as `PlatformError::Other` with a hint to
install `xdg-utils`).

### `linux/reflink.rs`
FICLONE ioctl implementation per `linux-roadmap.md` §5.1 L0.

- Public API: `pub fn clone_file(src, dst) -> PlatformResult<()>`.
- Algorithm: open src RO → create `dst`'s sibling `.{basename}.superdeduper-clone-tmp`
  with `create_new(true)` → invoke `ioctl(dst_fd, FICLONE=0x4020_9409, src_fd)`
  → drop dst file (flush) → `fs::rename(tmp, dst)` atomically.
- Hand-encoded `ioctl` extern + `FICLONE = 0x4020_9409` constant (avoids
  pulling libc/nix).
- Error classification: `EOPNOTSUPP (95)`, `ENOTTY (25)`, `EXDEV (18)` and
  `ErrorKind::Unsupported` → `PlatformError::Unsupported`; everything else
  passes through as `PlatformError::Io`.
- Tests cover the tmp-path shape, tmpfs-Unsupported-or-Ok contract, and the
  errno→variant mapping table.

### `linux/trash.rs`
XDG Trash spec implementation (freedesktop.org trashspec-1.0). Pure Rust —
no `gio trash` shell-out (decision documented at the top of the file:
no external binary dependency, faster, typed errors).

- Public API:
  - `pub struct TrashEntry { original_path, container, info_file, data_file }`
  - `pub fn trash_file(path) -> PlatformResult<TrashEntry>`
- Algorithm (matches XDG spec §3 invariant — write info file BEFORE rename
  so a crash leaves an unreferenced info file rather than an unrecoverable
  data file):
  1. Canonicalize path.
  2. Resolve trash root from `$XDG_DATA_HOME/Trash` or
     `$HOME/.local/share/Trash`.
  3. `mkdir -p` `files/` and `info/` under it.
  4. `pick_unique_name` collision-suffixes with `.2`, `.3`, ... (not the
     spec's " 2" — file deliberately deviates to avoid spaces).
  5. Build trashinfo text (URL-escape Path field per RFC 2396, write
     DeletionDate in `YYYY-MM-DDTHH:MM:SS` local-time-no-TZ).
  6. Write `.trashinfo` (`sync_all`), then `fs::rename` data.
- L0 only writes to the home-trash; cross-volume admin-trash (`<mount>/.Trash-$UID`)
  is a follow-up — cross-FS rename(2) returns EXDEV which currently surfaces
  as `Io`.
- Tests use `crate::test_serial::home_env_guard` (see issue A-home-env-serial
  #146) to serialize HOME-mutating tests against every other module that
  mutates HOME.

### `linux/mount_info.rs`
Issue #15 L2 — per-path mount introspection. Parses `/proc/self/mounts`,
longest-prefix-matches an input path, and tags the result with derived flags.

- File-level `#![cfg(target_os = "linux")]` gate.
- Public API:
  - `pub struct MountInfo { source, mountpoint, fs_type, options,
    is_dm_mapped, is_network, supports_reflink, may_have_pool_dedup }`
  - `MountInfo::summary_line() -> String`
  - `MountInfo::warnings() -> Vec<String>`
  - `pub fn for_path(&Path) -> Option<MountInfo>`
  - `pub fn parse_mounts_file(&str) -> io::Result<Vec<MountInfo>>`
- Callers: `main.rs` (CLI scan-start banner, line ~2034) and
  `gui/live.rs` (live message stream into the GUI Roots panel, line ~172).
- Flag tables (centralized here):
  - network: `cifs | smb3 | smbfs | nfs | nfs4 | afpfs | afs | ceph | 9p |
    fuse.sshfs | fuse.gvfs | fuse.gvfsd-fuse | fuse.rclone | fuse.s3fs`
  - reflink-capable: `btrfs | xfs | bcachefs | zfs | ocfs2`
  - pool-dedup-capable: `zfs | btrfs`
  - `is_dm_mapped`: source starts with `/dev/mapper/`
- `decode_octal` handles the `\040 \011 \134` escapes that the kernel emits
  for whitespace + backslash in mountpoint paths.

## Invariants / Gotchas

- **Trash write order**: `linux::trash::trash_file` writes `<base>.trashinfo`
  BEFORE renaming the data file into `files/`. Inversion is a load-bearing
  user-data-loss bug — invariant lifted directly from XDG spec §3 and
  documented inline.
- **Reflink rename atomicity**: `linux::reflink::clone_file` creates its tmp
  in the same directory as `dst` because (a) FICLONE can't cross filesystems
  and (b) `fs::rename` is atomic only within one filesystem. Moving the tmp
  to `std::env::temp_dir()` would break both invariants silently.
- **`windows::clone_file` arg order**: maps `(src, dst)` → `replace_with_reflink(dst, src)`.
  The wrapper inside `winapi_wrappers` takes `(target, keeper)` (target = file
  being replaced). Refactorers swapping arg names should preserve this swap.
- **Vocab const-fns**: `trash_bin_noun`, `trash_action_verb`,
  `move_to_trash_phrase` are `const fn`; the `trash_vocab_is_internally_consistent_across_helpers`
  test runs on every OS and asserts the three results are either all
  "Trash-family" or all "Recycle-family" — a cfg typo dropping the Linux arm
  of one helper but not the others would be caught here.
- **HOME-mutating tests**: `linux::trash::tests` MUST use
  `crate::test_serial::home_env_guard` (see top-of-tests comment) because
  several modules outside `platform/` also mutate HOME (`scan_history`,
  `dedupe`). A bare per-module `Mutex` is insufficient — burned per #146.
- **`TrashOutcome` populated only on Linux today**: Windows + macOS
  `trash_file` return `TrashOutcome::default()`. Receipt consumers in
  `dedupe.rs` must tolerate all-None fields.

## Dependencies
- INCOMING: `crate::dedupe` (clone_file, trash_file, TrashOutcome,
  PlatformError), `crate::gui::{app, live, widgets::*}` (vocab + open_url +
  `linux::mount_info::for_path`), `crate::leaderboard::{captcha, oauth}`
  (open_url), `crate::main` (open_url + `linux::mount_info::for_path`).
- OUTGOING:
  - `crate::winapi_wrappers::{recycle, replace_with_reflink}` (Windows
    delegation).
  - `crate::time::{now_unix_i64, unix_to_ymdhms}` (trashinfo DeletionDate).
  - `crate::test_serial::home_env_guard` (test serialization).
  - External `windows` crate (`windows::Win32::UI::Shell::ShellExecuteW` etc.).
  - Raw `extern "C" { fn ioctl }` for FICLONE (no libc dependency).
  - System binaries: `xdg-open` (Linux), `open` (macOS) — spawned with
    null'd stdio.

## Refactor Hints

- **`linux::trash::TrashEntry` and `platform::TrashOutcome` are two near-identical
  structs.** mod.rs's `linux` branch translates field-by-field. Could collapse
  to one struct (e.g. re-export `linux::trash::TrashEntry as TrashOutcome` on
  Linux) but the indirection earns its keep on Windows/macOS where the fields
  are all None — keep as-is unless Windows starts populating them (the TODO
  on `mod.rs:217` says it eventually will).
- **`parse_mounts_file` is documented as "Public so unit tests can run against
  a fixture file" but no test actually calls it** (tests call
  `parse_mounts_body` instead). Either tighten visibility to `pub(crate)` /
  private or add a test that exercises the file path. See p3 finding below.
- **Vocab helpers are `const fn`**, so callers in `format!` strings can't fold
  them at compile-time (`format!` is runtime). If a refactor moves them into
  `concat!`-style sites, the `const fn` shape pays off; otherwise plain `fn`
  is fine.
- **Windows `clone_file` doc comment mentions FSCTL_DUPLICATE_EXTENTS_TO_FILE,
  but the impl is one delegate-call away** — actual ioctl wrangling lives in
  `winapi_wrappers::windows_impl::replace_with_reflink:940`. Refactorers
  chasing the FSCTL syscall must follow that hop.
- **`linux/mod.rs` module-tree comment in `mod.rs` lines 19-24** lists
  `linux.rs` first then "(or linux/mod.rs if it grows)" — directory form is
  now the reality; could simplify the comment.

## Wire Surfaces

- `linux::trash` writes the XDG `.trashinfo` format at
  `$XDG_DATA_HOME/Trash/info/<base>.trashinfo` (default
  `$HOME/.local/share/Trash/info/...`). Format: 2-line key=value following
  `[Trash Info]` header — `Path=<RFC 2396 url-escaped>` +
  `DeletionDate=YYYY-MM-DDTHH:MM:SS` (no TZ). Collision suffix is `.<n>` (not
  the spec's " <n>").
- `linux::reflink` writes a tmp sidecar `.<basename>.superdeduper-clone-tmp`
  in the destination directory; rename-over-`dst` is the atomic publish step.
- `linux::mount_info` reads `/proc/self/mounts`. Best-effort — returns `None`
  in chroots / containers where the file isn't present.
- Environment variables read:
  - `XDG_DATA_HOME` (Linux trash root) — non-absolute values are ignored per
    XDG spec.
  - `HOME` (Linux trash root fallback).
- Spawned binaries: `xdg-open <url>` (Linux), `open <url>` (macOS).

## Non-source artifacts
None — all files in this directory are `.rs` source.
