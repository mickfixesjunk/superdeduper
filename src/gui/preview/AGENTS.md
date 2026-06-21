# preview - AGENTS guide

## Purpose

This directory implements the in-app file preview panel used in the
duplicate-decision UI (issue #27 v1). Given a single file path, it
classifies the file (text / hex / system-handler / unavailable) and
renders an appropriate viewer inside an `egui::Ui` region driven by
the GUI's right-hand panel.

The module ships the scaffold plus the two fallback viewers (text +
hex), and on Windows it also performs a registry lookup that
resolves a file extension to its registered `IPreviewHandler` CLSID.
The actual COM-hosted `IPreviewHandler` rendering (CoCreateInstance,
IInitializeWithFile, DoPreview into an offscreen surface, GDI/WIC
bitmap capture, upload to an egui texture) is explicitly deferred to
a Windows-side iteration session - today the system-handler branch
only surfaces a "preview handler registered at {CLSID}" indicator
and falls through to the hex viewer.

The whole module is gated behind `#![cfg(feature = "gui")]`; the
`gui` feature is defined in the workspace `Cargo.toml`.

## Files

### `mod.rs`
Public API surface for the preview panel. Defines the `PreviewMode`
and `PreviewAction` enums plus the `PreviewState` host struct, and
provides the top-level `show` entry point used by `gui::app`. Also
contains a `show_side_by_side` stub (allowed-dead-code; v2) and a
`show_or_close` convenience wrapper.

- Public API:
  - `pub enum PreviewMode { Text, Hex, SystemHandler { clsid }, Unavailable { reason } }` - viewer selection
  - `pub enum PreviewAction { Close, ForceHex, ForceText }` - host-handled UI actions
  - `pub struct PreviewState { pub mode_override: Option<PreviewMode> }` - sticky mode override
  - `pub fn show(ui, path, state) -> Option<PreviewAction>` - main entry; used by `gui::app`
  - `pub fn show_side_by_side(...)` - v2 stub, `#[allow(dead_code)]`
  - `pub fn show_or_close(ui, current, state) -> Option<Option<PathBuf>>` - convenience wrapper, no callers
  - `pub mod classify;` / `pub mod fallback_text;` / `pub mod fallback_hex;` / (windows) `pub mod registry_lookup;`
- Who calls this: `crate::gui::app` (PreviewState field on App, `show` call at line ~4196).
- Feature gates: `#![cfg(feature = "gui")]` at top of file; `#[cfg(windows)]` on the `registry_lookup` submodule import and on the SystemHandler classification branch.

### `classify.rs`
Picks the `PreviewMode` for a given path. Strategy: stat the file
(to surface Unavailable on missing / non-regular), then an extension
allowlist (~70 known text extensions) maps directly to Text, then a
4-KiB content sniff falls back to text iff valid UTF-8 and >=90%
"printable" (tab/CR/LF or 0x20-0x7E). On Windows, if the sniff
fails, attempts `registry_lookup::handler_clsid` and returns
`SystemHandler { clsid }` if found; otherwise `Hex`.

- Public API:
  - `pub fn classify_path(path: &Path) -> PreviewMode`
- Internal: `read_sniff`, `looks_like_text`, `TEXT_EXTENSIONS`.
- Who calls this: `super::show` (mod.rs line 128).
- Feature gates: inherits `gui` from parent; `#[cfg(windows)]` block around the registry lookup.

### `fallback_text.rs`
Built-in read-only text viewer. Reads up to 64 KiB + 1 byte (the
+1 is used as a truncation sentinel), renders the prefix lossily
through `String::from_utf8_lossy` into a read-only monospace
`egui::TextEdit::multiline` inside a vertical `ScrollArea`. On
truncation, shows a "Showing first 64KiB of ..." banner. No syntax
highlighting (deliberate: pulling syntect would balloon the binary).

- Public API: `pub fn show(ui: &mut Ui, path: &Path)`
- Who calls this: `super::show` (mod.rs line 170).
- Constants: `MAX_BYTES = 64 KiB`.

### `fallback_hex.rs`
Built-in hex viewer. Reads first 4 KiB (head) and, for files larger
than 8 KiB, also the last 4 KiB (tail). Renders 16-byte rows with
8-byte midline gap and ASCII gutter, padded to a 49-char canonical
hex column width so trailing partial rows still align with the
ASCII column. A "[...]" separator marks the elision between head
and tail.

- Public API: `pub fn show(ui: &mut Ui, path: &Path)`
- Internal: `read_head_and_tail`, `render_block`, `ROW_BYTES`, `HEAD_BYTES`, `TAIL_BYTES`.
- Who calls this: `super::show` (mod.rs lines 171 and 186, where the SystemHandler arm falls through to hex).

### `registry_lookup.rs`
Windows-only. Resolves an extension to its registered
`IPreviewHandler` CLSID by reading
`HKEY_CLASSES_ROOT\.<ext>\shellex\{8895b1c6-b41f-4c1c-a562-0d564250836f}`
and, failing that, following one level of class-name redirection
via the default value of `HKCR\.<ext>`. Uses raw `RegOpenKeyExW` /
`RegQueryValueExW` from the `windows` crate; two-pass query (size
probe then read), wide-string decode, trim. Returns the braced GUID
string or `None`.

- Public API: `pub fn handler_clsid(extension: &str) -> Option<String>`
- Internal: `read_default`, constant `IPREVIEW_HANDLER_SHELLEX_GUID`.
- Who calls this: `super::classify::classify_path` (only inside `#[cfg(windows)]`).
- Feature gates: `#![cfg(windows)]` at the file level (and inherits `gui` from parent module).

## Invariants / Gotchas

- The entire module compiles only with `feature = "gui"`; the
  `registry_lookup` submodule additionally requires `cfg(windows)`.
- `PreviewMode::SystemHandler` MUST NOT be constructed outside
  Windows. `classify_path` enforces this by gating the branch with
  `#[cfg(windows)]`. The match arm in `mod.rs::show` for
  `SystemHandler` exists unconditionally and falls back to the hex
  viewer; that is intentional (non-Windows builds simply never see
  the variant).
- The 4-KiB sniff in `classify::looks_like_text` checks "printable
  ASCII" by counting bytes in `0x20..=0x7E` (plus tab/CR/LF). The
  inline comment claims "be lenient on UTF-8 multi-byte chars" but
  the loop does NOT treat multi-byte UTF-8 bytes (>= 0x80) as
  printable - it relies on the >= 90% ratio to absorb them. Pure
  non-ASCII UTF-8 text (e.g. mostly-CJK) can therefore classify as
  hex. Preserve that behavior or update the comment.
- `fallback_hex::render_block` depends on the exact `FULL_WIDTH = 49`
  canonical hex column. Changing `ROW_BYTES` (16) requires
  recomputing FULL_WIDTH; the comment on line 151 explains the
  formula (16*2 + 15 + 1 = 48 with one extra midpoint space = 49).
- The hex viewer reads head and tail in a single open + seek; small
  files (<= 8 KiB) go through `std::fs::read` instead. The `tail`
  field of the tuple being `None` IS the small-file sentinel.
- `display_path` in mod.rs intentionally delegates to
  `crate::path_display::for_user_display` so the `\\?\` strip rules
  stay consistent across UI surfaces (per issue #73).

## Dependencies

- INCOMING: `crate::gui::app` (constructs `PreviewState`, calls
  `preview::show`, handles `PreviewAction`).
- OUTGOING:
  - `crate::gui::theme` (color constants TEXT_HI / TEXT_LO / COOL / WARN / HOT)
  - `crate::path_display::for_user_display`
  - `egui` (RichText, Ui, ScrollArea, TextEdit, Layout)
  - `humansize` (file-size formatting)
  - `windows` crate (registry APIs, windows-only)
  - `std::fs` / `std::io`

## Refactor Hints

- `show_or_close` (mod.rs line 253) is `pub` with zero callers in the
  repo (`grep -rn "show_or_close" .` returns only the definition).
  Either delete or add `#[allow(dead_code)]` like `show_side_by_side`.
- `fallback_text.rs` lines 44-49 build the truncation banner with
  `MAX_BYTES + 1` as the "of" size, which prints "Showing first 64
  KiB of 64 KiB" or similar - it's not the file's actual size.
  Replace with `std::fs::metadata(path).map(|m| m.len())` for a
  truthful number (the hex viewer already does this).
- `classify::looks_like_text` comment about UTF-8 leniency disagrees
  with the implementation - either widen the predicate to count
  bytes >= 0x80 inside a UTF-8-valid buffer or trim the comment.
- The `TEXT_EXTENSIONS` list contains `"dockerfile"`, `"gitignore"`,
  `"readme"`, `"license"`, `"editorconfig"` - these are typically
  full filenames without extensions, not extensions, so they will
  never match through `path.extension()`. Either move them to a
  filename-based fast-path or drop them.
- `registry_lookup::read_default`: the second `RegQueryValueExW`
  ignores `data_type` after reading and trusts the bytes are
  wide-string. A `REG_SZ` / `REG_EXPAND_SZ` check would harden it
  but is low priority because preview-handler CLSIDs are always
  REG_SZ in practice.

## Wire Surfaces

- No HTTP endpoints, no on-disk format, no CLI flags.
- Reads Windows registry keys under `HKEY_CLASSES_ROOT` (read-only,
  KEY_READ): `<.ext>\shellex\{8895b1c6-b41f-4c1c-a562-0d564250836f}`
  and `<.ext>` default value. The GUID 8895b1c6-... is the
  IPreviewHandler shellex category and is a Windows-platform
  constant.
- No environment variables read.
