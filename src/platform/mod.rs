//! Platform abstraction layer.
//!
//! Cross-platform code calls into this module via free functions; the
//! cfg-routing happens once here, and per-OS impls live in their
//! sibling files. Adding a new platform is a matter of dropping a
//! new sibling + adding a cfg branch on each public fn — call sites
//! don't change.
//!
//! Design endorsed (A) free-function-per-platform over a trait-based
//! abstraction (channel: design-superdeduper.md 2026-05-24T08:19:41Z).
//! Reasons: matches existing cfg patterns in the codebase, no runtime
//! polymorphism need, integration tests run on the real OS so mock
//! impls aren't valuable. Refactor to traits is mechanical if a need
//! ever surfaces.
//!
//! ## Module structure
//!
//! ```text
//! src/platform/
//!   mod.rs       -- this file; public free-fn API + cfg routes
//!   linux.rs     -- Linux impls (or linux/mod.rs if it grows)
//!   windows.rs   -- Windows impls (or windows/mod.rs)
//!   macos.rs     -- macOS impls (L3 territory; stub for now)
//! ```
//!
//! ## Cross-platform rules
//!
//! * Public functions in this file MUST cfg-route to a per-OS impl.
//!   No platform-specific symbols leak out.
//! * Per-OS files MAY use any platform-specific crates / syscalls.
//! * Tests on the public API run on every supported OS; per-OS files'
//!   own tests run only on their target.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "macos")]
mod macos;

#[cfg(windows)]
mod windows;

use std::path::Path;

/// Errors the platform layer can surface. Kept narrow so callers
/// only need to handle the kinds that actually differ — most are
/// passed through as `Other` with a context string.
#[derive(Debug)]
pub enum PlatformError {
    /// The platform doesn't support this operation. (e.g. reflink on
    /// a non-CoW filesystem; trash on a platform that doesn't have
    /// a trash convention.)
    Unsupported(&'static str),
    /// Underlying IO error.
    Io(std::io::Error),
    /// Catch-all for OS-specific errors that don't map cleanly to
    /// `Io`. Carries a human-readable message.
    Other(String),
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformError::Unsupported(reason) => {
                write!(f, "unsupported on this platform: {reason}")
            }
            PlatformError::Io(e) => write!(f, "{e}"),
            PlatformError::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for PlatformError {}

impl From<std::io::Error> for PlatformError {
    fn from(e: std::io::Error) -> Self {
        PlatformError::Io(e)
    }
}

pub type PlatformResult<T> = std::result::Result<T, PlatformError>;

// ============================================================
// Reflink — clone-on-write file copy.
//
// Windows: ReFS block-clone via FSCTL_DUPLICATE_EXTENTS_TO_FILE.
// Linux:   FICLONE ioctl (Btrfs, XFS, Bcachefs, ZFS-via-block-clone).
// macOS:   APFS clonefile() (L3 territory).
//
// `clone_file(src, dst)` creates `dst` as a clone of `src`. Both
// files share storage on disk until one is modified. Returns
// Unsupported when the underlying filesystem doesn't support
// reflinks (e.g. NTFS without ReFS, ext4 without FICLONE).
// ============================================================

#[cfg(target_os = "linux")]
pub fn clone_file(src: &Path, dst: &Path) -> PlatformResult<()> {
    linux::reflink::clone_file(src, dst)
}

#[cfg(windows)]
pub fn clone_file(src: &Path, dst: &Path) -> PlatformResult<()> {
    windows::clone_file(src, dst)
}

#[cfg(target_os = "macos")]
pub fn clone_file(src: &Path, dst: &Path) -> PlatformResult<()> {
    macos::clone_file(src, dst)
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn clone_file(_src: &Path, _dst: &Path) -> PlatformResult<()> {
    Err(PlatformError::Unsupported(
        "reflink not supported on this platform",
    ))
}

// ============================================================
// Trash — move a file to the user's trash / Recycle Bin.
//
// Windows: IFileOperation with FOF_ALLOWUNDO.
// Linux:   XDG Trash spec — move to ~/.local/share/Trash/files + write
//          .trashinfo metadata in ~/.local/share/Trash/info.
// macOS:   NSFileManager trashItemAtURL (L3 territory).
//
// `trash_file(path)` moves `path` to the OS trash. Recoverable
// until the user empties the trash. Returns Unsupported when no
// trash mechanism is available (rare; covers no-XDG-trash-dir
// situations + headless server installs).
// ============================================================

/// Cross-platform "what just got trashed" metadata. Populated when
/// the per-platform trash backend has detail to surface; consumed by
/// the dedupe pipeline to fill the `recycle_bin_entry` field on
/// action receipts (per GH #33). v1 only Linux populates this; the
/// Windows IFileOperation result wiring is a follow-up.
#[derive(Debug, Clone, Default)]
pub struct TrashOutcome {
    pub original_path: Option<std::path::PathBuf>,
    pub container: Option<std::path::PathBuf>,
    pub info_file: Option<std::path::PathBuf>,
    pub data_file: Option<std::path::PathBuf>,
}

#[cfg(target_os = "linux")]
pub fn trash_file(path: &Path) -> PlatformResult<TrashOutcome> {
    let e = linux::trash::trash_file(path)?;
    Ok(TrashOutcome {
        original_path: Some(e.original_path),
        container: Some(e.container),
        info_file: Some(e.info_file),
        data_file: Some(e.data_file),
    })
}

#[cfg(windows)]
pub fn trash_file(path: &Path) -> PlatformResult<TrashOutcome> {
    windows::trash_file(path)?;
    // TODO #33 v2 — extract IFileOperation result (the $I + $R
    // filenames the shell minted) and populate this struct. Today
    // the windows::trash_file shim doesn't return that; needs a
    // wider plumbing job in winapi_wrappers::recycle.
    Ok(TrashOutcome::default())
}

#[cfg(target_os = "macos")]
pub fn trash_file(path: &Path) -> PlatformResult<TrashOutcome> {
    macos::trash_file(path)?;
    Ok(TrashOutcome::default())
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn trash_file(_path: &Path) -> PlatformResult<TrashOutcome> {
    Err(PlatformError::Unsupported(
        "trash not supported on this platform",
    ))
}

// ============================================================
// Open URL in default browser. Used by the G1 captcha
// registration flow + the badge-wall "view profile" link.
// ============================================================

#[cfg(target_os = "linux")]
pub fn open_url(url: &str) -> PlatformResult<()> {
    linux::open_url(url)
}

#[cfg(windows)]
pub fn open_url(url: &str) -> PlatformResult<()> {
    windows::open_url(url)
}

#[cfg(target_os = "macos")]
pub fn open_url(url: &str) -> PlatformResult<()> {
    macos::open_url(url)
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub fn open_url(_url: &str) -> PlatformResult<()> {
    Err(PlatformError::Unsupported(
        "open_url not supported on this platform",
    ))
}
