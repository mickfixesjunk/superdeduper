# winapi_wrappers - AGENTS guide

## Purpose
This directory is the sole home for `unsafe` Win32 / NT FFI in the `superdeduper` crate. Every `unsafe` block in the project lives here, each annotated with a `// SAFETY:` comment naming the contract it upholds. Callers outside this module see only safe Rust types and `crate::Result`.

The module gates on `#[cfg(windows)]` and re-exports `windows_impl::*`; on non-Windows targets it re-exports a stub module so the rest of the crate (CLI, config, grouping, output, walker traits) still compiles for cross-platform development. The stubs all return `Error::Unsupported`.

Functional surface area: volume resolution and raw-volume handle open, storage device classification (HDD vs SSD via bus type + seek-penalty IOCTL), file extent maps (`FSCTL_GET_RETRIEVAL_POINTERS`), USN journal enumeration and delta read, reparse-tag fetch, Recycle Bin via `IFileOperation` COM, hardlink replacement, and ReFS block-clone (reflink) replacement.

## Files

### `mod.rs`
Tiny dispatcher. `#[cfg(windows)] mod windows_impl; pub use windows_impl::*;` and the symmetric `#[cfg(not(windows))]` branch for `stub`. Crate-wide `#![allow(dead_code)]` so the stub side doesn't error on items only called from Windows-specific paths elsewhere.

- Public API: none of its own; transparent re-export.
- Feature gates: `#[cfg(windows)]` / `#[cfg(not(windows))]` at module level.

### `stub.rs`
Non-Windows shim. Type aliases and structs mirror the Windows side; every function returns `Err(Error::Unsupported(...))` except `fetch_reparse_tag` which returns `None` (because `classify()` already treats `None` as `NotPlaceholder`).

- Public API:
  - `type FileRef = u64;`
  - `type StartLcn = u64;`
  - `struct StorageDeviceInfo { ... }`
  - `struct ExtentRun { vcn, lcn, length_clusters }`
  - `struct UsnJournalState { journal_id, first_usn, next_usn }`
  - `fn bus_type_name(_b: u8) -> &'static str`
  - `fn volume_for_path(_p: &Path) -> Result<String>`
  - `fn query_storage_device(_volume: &str) -> Result<StorageDeviceInfo>`
  - `fn get_retrieval_pointers(_path: &Path) -> Result<Vec<ExtentRun>>`
  - `fn query_usn_journal_state(_volume_guid: &str) -> Result<UsnJournalState>`
  - `fn recycle(_p: &Path) -> Result<()>`
  - `fn replace_with_hardlink(_t: &Path, _k: &Path) -> Result<()>`
  - `fn replace_with_reflink(_t: &Path, _k: &Path) -> Result<()>`
  - `fn fetch_reparse_tag(_path: &Path) -> Option<u32>`
- Who calls this: same callers as the Windows side, automatically on non-Windows targets (e.g. CI Linux compile).
- Feature gates: file is reached only under `#[cfg(not(windows))]`.

### `windows_impl.rs`
The substantive module. Wraps `windows` crate FFI into safe Rust returning `crate::Result`. Approx. 1200 lines including unit tests.

- Public API (selected, all `pub`):
  - Types: `FileRef`, `StartLcn`, `StorageDeviceInfo`, `ExtentRun`, `UsnRecord`, `UsnJournalState`, `OwnedHandle`, `UsnEnum`.
  - Volume / device: `volume_for_path`, `open_volume_handle`, `open_volume_handle_for_query`, `query_storage_device`, `query_bus_type`, `classify_seek_penalty`.
  - Bus-type constants: `pub mod bus_type` (SCSI, ATAPI, ATA, USB, RAID, ISCSI, SAS, SATA, SD, MMC, NVME, SCM, UFS) and `bus_type_name`.
  - File metadata: `open_file_direct`, `get_retrieval_pointers`, `fetch_reparse_tag`.
  - USN: `UsnEnum::open` / `next_batch`, `query_usn_journal_state`, `read_usn_journal_delta`.
  - Mutating ops: `recycle` (IFileOperation), `replace_with_hardlink`, `replace_with_reflink`.
  - Helper: `pathbuf_from_wide` (pub(crate), marked `#[allow(dead_code)]`).
- Private helpers: `open_volume_handle_with_access`, `open_file_for_metadata`, `parse_retrieval_pointers`, `parse_usn_records`, `block_clone`, `create_hard_link`, `strip_verbatim_prefix`.
- Who calls this: `crate::inventory::warm` (USN delta + journal state), `crate::inventory::walk` / `inventory::mft` (UsnEnum, retrieval pointers, reparse tag), CLI `drive-info` subcommand (storage device query + bus-type display), dedupe action layer (`recycle`, `replace_with_hardlink`, `replace_with_reflink`), IOCP read pipeline (`open_file_direct`).
- Key invariants: see below.
- Feature gates: entire file under `#[cfg(windows)]` via `mod.rs`.

## Invariants / Gotchas

- Every `unsafe` block must carry a `// SAFETY:` comment naming the FFI contract. Adding a new unsafe block without one breaks the module convention.
- `OwnedHandle` is a single-owner RAII handle; do NOT clone or duplicate without using `DuplicateHandle`. Drop calls `CloseHandle` exactly once and guards against `INVALID_HANDLE_VALUE`.
- `open_volume_handle_for_query` uses `desired_access = 0` so non-admin / WSL-interop callers can run `drive-info`. Do NOT raise this to `GENERIC_READ` - it will fail with ERROR_ACCESS_DENIED for unelevated users. `GENERIC_READ` is required for `FSCTL_ENUM_USN_DATA` and `FSCTL_READ_USN_JOURNAL` (those use `open_volume_handle`).
- `open_volume_handle_with_access` trims the trailing backslash from the GUID path because Win32 needs `\\?\Volume{...}` for device-open, not `\\?\Volume{...}\`.
- `read_usn_journal_delta` MUST pass a non-zero `journal_id` even though MSDN documents 0 as "bypass identifier verification" - real Windows builds return `ERROR_INVALID_PARAMETER` on 0. Caller must query state first via `query_usn_journal_state`.
- USN journal validity check (in `inventory::warm`): saved snapshot is valid iff `(stored journal_id == current journal_id) && (stored cursor >= first_usn)`. Anything else means rebuild from full MFT walk.
- `recycle()` MUST strip the `\\?\` verbatim prefix before calling `SHCreateItemFromParsingName` - the shell API rejects it with E_INVALIDARG. The walker adds the prefix in `inventory::walk::to_verbatim` for legacy-API long-path support. Regression-guarded by `strip_verbatim_prefix_drops_extended_path_marker` test.
- COM init in `recycle()`: only call `CoUninitialize` if `CoInitializeEx` returned `S_OK` or `S_FALSE`. `RPC_E_CHANGED_MODE` means another component already initialized the thread MTA and we did NOT take a refcount.
- `create_hard_link` (and by extension `replace_with_hardlink`) MUST surface FFI failure - swallowing the error would cause the caller to delete the `.tmp` snapshot of the original, losing the file. The HRESULT is masked to its Win32 code via `& 0xFFFF` so the dedupe layer can match `ERROR_NOT_SAME_DEVICE = 17`.
- `replace_with_reflink` / `replace_with_hardlink` use a rename-aside, swap, delete-on-success / restore-on-failure dance for atomicity. Preserve the order.
- `block_clone` cluster alignment: requests rounded up to 4 KiB, never exceeding `size`. ReFS standard cluster is 4 KiB, may be 64 KiB - the constant `CHUNK = 64 KiB` is the safe upper bound; alignment-up to 4095 satisfies both.
- `parse_usn_records` only honors `USN_RECORD_V2`; V3 (object IDs) and V4 (ReFS range-tracking) are silently skipped by record_len advance. Cold-path `FSCTL_ENUM_USN_DATA` reason field is always 0 - only warm-path `FSCTL_READ_USN_JOURNAL` populates it; consumers should not trust `reason` on cold records.
- `classify_seek_penalty` trusts bus type over the IOCTL for NVMe/SCM/SD/MMC/UFS (some NVMe controllers lie). ATA bus is HDD by construction. Ambiguous + IOCTL-failed defaults to HDD (conservative). Do not rearrange without re-examining the comment block.
- Stubs MUST mirror the Windows public API by shape (names, types, signatures) so cross-platform compile stays green. Adding a pub item to `windows_impl.rs` requires a matching stub.

## Dependencies

- INCOMING:
  - `crate::inventory::walk`, `crate::inventory::mft`, `crate::inventory::warm` (USN + reparse + extents)
  - `crate::dedupe` action layer (`recycle`, `replace_with_hardlink`, `replace_with_reflink`)
  - CLI `drive-info` subcommand (storage device classification)
  - IOCP read pipeline (`open_file_direct`)
- OUTGOING:
  - `windows` crate (Win32 FFI bindings)
  - `std::os::windows::ffi`, `std::os::windows::io`, `std::os::windows::fs`
  - `tracing` (info-level event on `query_storage_device`)
  - `crate::Error`, `crate::Result`

## Refactor Hints

- `pathbuf_from_wide` is `pub(crate)` and marked `#[allow(dead_code)]` (line 731). Grep the repo for `pathbuf_from_wide` before deleting; the `#[allow]` suggests it's a deliberate utility kept around. **info** candidate for removal if grep confirms no callers.
- `StorageDeviceInfo.sector_size` / `physical_sector_size` are hardcoded to 4096 with comment `// populated by a later commit` (lines 282-283). This is a placeholder - the IOCTL `IOCTL_STORAGE_QUERY_PROPERTY` with `StorageAccessAlignmentProperty` should fill these. Tracking-comment style; not load-bearing today since callers don't yet inspect them, but anyone reading the struct shape will be misled.
- `bus_type` submodule exposes named constants but `bus_type_name` decodes some values inline (0x04 "1394", 0x05 "SSA", 0x06 "Fibre", 0x0E "Virtual", 0x0F "FileBackedVirtual", 0x10 "Storage Spaces"). Cohesion improvement: lift these to named consts in `pub mod bus_type` for consistency.
- The `let _ = &mut request;` on line 1035 is a no-op keep-alive hint; modern Rust extends temporaries through `DeviceIoControl` automatically. Likely dead; verify with a single test cycle before removing.
- `OwnedHandle.as_handle` is `pub` but every internal caller uses the `.0` tuple-field directly (lines 227, 252, 392, 420, 538, 660, 708). Either route callers through the accessor or drop the accessor. **info**.
- Three near-identical `CreateFileW` helpers (`open_volume_handle_with_access`, `open_file_for_metadata`, `open_file_direct`, plus the inline open in `fetch_reparse_tag`) share the same wide-string prep + null-handle check pattern. A `create_file_w(path, access, share, flags) -> Result<OwnedHandle>` would consolidate ~40 LOC.
- `fetch_reparse_tag` is the only function that opens a handle via raw `CreateFileW` instead of going through `OwnedHandle` - it manually `CloseHandle`s. Migrate to the common helper for symmetry.
- `query_storage_device` returns `Ok` even when the seek-penalty IOCTL failed (Err -> None); fine. But the bus-type query failing falls back silently to 0; tracing event still logs "Unknown" via `bus_type_name(0)`. Consider plumbing the bus error into `classification_reason` for `drive-info` diagnostics.

## Wire Surfaces

- FSCTL surfaces consumed (kernel ABI, not us):
  - `FSCTL_ENUM_USN_DATA` (MFT_ENUM_DATA_V0)
  - `FSCTL_QUERY_USN_JOURNAL` (USN_JOURNAL_DATA_V0)
  - `FSCTL_READ_USN_JOURNAL` (READ_USN_JOURNAL_DATA_V0)
  - `FSCTL_GET_RETRIEVAL_POINTERS` (STARTING_VCN_INPUT_BUFFER + RETRIEVAL_POINTERS_BUFFER)
  - `FSCTL_GET_REPARSE_POINT`
  - `FSCTL_DUPLICATE_EXTENTS_TO_FILE` (DUPLICATE_EXTENTS_DATA)
  - `IOCTL_STORAGE_GET_DEVICE_NUMBER`, `IOCTL_STORAGE_QUERY_PROPERTY` (StorageDeviceSeekPenaltyProperty, StorageAdapterProperty)
- COM surfaces: `IFileOperation` (FileOperation CLSID), `IShellItem` via `SHCreateItemFromParsingName`.
- On-disk format: none directly. The `journal_id` / cursor pair this module returns is persisted by `inventory::warm` in its own snapshot format.
- CLI flags / env: none owned here. `drive-info` subcommand consumes `query_storage_device` output.

## Non-source artifacts
None in this directory.
