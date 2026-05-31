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
pub mod linux;

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
// #159 -- A-trash-vocab: per-platform "Trash" vs "Recycle Bin"
// terminology. macOS calls the bin "Trash" and the verb
// "Move to Trash"; Linux follows XDG ("Trash"); Windows is
// "Recycle Bin" / "Recycle". The helpers below centralize the
// platform branch so every user-facing label, tooltip, modal
// text, and CLI help string reads the same per-OS noun + verb
// without each call site re-writing the cfg ladder. Adding a
// new platform = one change here, not a sweep.
// ============================================================

/// Per-platform name of the OS-level "soft delete" destination
/// directory the user sees in their file manager. macOS + Linux =
/// "Trash"; Windows = "Recycle Bin". Use this in any user-facing
/// label that refers to WHERE the file goes ("file is in the Trash"
/// vs "file is in the Recycle Bin").
pub const fn trash_bin_noun() -> &'static str {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        "Trash"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "Recycle Bin"
    }
}

/// Per-platform verb for the soft-delete ACTION. macOS = "Trash"
/// (matches Finder's "Move to Trash" menu); Linux = "Trash"; Windows =
/// "Recycle". Use this in any button label / action verb that names
/// what the user is about to do.
pub const fn trash_action_verb() -> &'static str {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        "Trash"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "Recycle"
    }
}

/// Per-platform full "Move to <trash>" phrase. macOS + Linux = "Move
/// to Trash" (mirrors Finder + GNOME Files); Windows = "Move to
/// Recycle Bin" (mirrors File Explorer). Convenience over
/// `format!("Move to {}", trash_bin_noun())` so the call site reads
/// closer to the displayed text.
pub const fn move_to_trash_phrase() -> &'static str {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        "Move to Trash"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "Move to Recycle Bin"
    }
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

#[cfg(test)]
mod trash_vocab_tests {
    use super::*;

    // #159 -- A-trash-vocab. These tests pin the per-platform string
    // contract so a future cfg-typo (e.g. dropping the Linux arm of
    // the any()) gets caught at test time instead of in a screenshot
    // review. Each test is target_os-gated so it asserts the value
    // for the platform it's actually running on.

    #[cfg(target_os = "macos")]
    #[test]
    fn trash_vocab_uses_trash_words_on_macos() {
        assert_eq!(trash_bin_noun(), "Trash");
        assert_eq!(trash_action_verb(), "Trash");
        assert_eq!(move_to_trash_phrase(), "Move to Trash");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn trash_vocab_uses_trash_words_on_linux() {
        // Linux follows XDG vocab -- same as macOS.
        assert_eq!(trash_bin_noun(), "Trash");
        assert_eq!(trash_action_verb(), "Trash");
        assert_eq!(move_to_trash_phrase(), "Move to Trash");
    }

    #[cfg(windows)]
    #[test]
    fn trash_vocab_uses_recycle_words_on_windows() {
        assert_eq!(trash_bin_noun(), "Recycle Bin");
        assert_eq!(trash_action_verb(), "Recycle");
        assert_eq!(move_to_trash_phrase(), "Move to Recycle Bin");
    }

    /// Cross-platform contract: bin-noun and action-verb must NEVER
    /// return mismatched vocabularies (e.g. action-verb="Trash" but
    /// bin-noun="Recycle Bin") -- they're shown together in modals
    /// and the mismatch is jarring. This test runs everywhere.
    #[test]
    fn trash_vocab_is_internally_consistent_across_helpers() {
        let noun = trash_bin_noun();
        let verb = trash_action_verb();
        let phrase = move_to_trash_phrase();
        let is_trash_family = noun == "Trash" && verb == "Trash"
            && phrase == "Move to Trash";
        let is_recycle_family = noun == "Recycle Bin" && verb == "Recycle"
            && phrase == "Move to Recycle Bin";
        assert!(
            is_trash_family || is_recycle_family,
            "vocab helpers disagreed: noun={noun:?}, verb={verb:?}, phrase={phrase:?}"
        );
    }
}
