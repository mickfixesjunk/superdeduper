//! #73 — Single source-of-truth helper for user-facing path
//! display. Strips the Windows `\\?\` verbatim-path prefix so
//! UI surfaces show `C:\Foo\bar` instead of `\\?\C:\Foo\bar`;
//! rewrites the UNC verbatim form (`\\?\UNC\server\share`) back
//! to `\\server\share`.
//!
//! ## Background
//!
//! The engine uses verbatim paths internally to bypass the Win32
//! MAX_PATH limit. Surfacing them as-is to the user is jarring —
//! every "open in Explorer" / "select-keeper" UX shows the
//! `\\?\` prefix that nobody recognises. Several independent GUI
//! call sites each had their own identical `format_path` /
//! `display_path` impl; this module consolidates. Current GUI
//! surfaces routed through `for_user_display`:
//!
//! * groups-table dupe row + keeper row (#118)
//! * preview pane header
//! * scan-finished Log
//! * roots panel
//! * resume modal
//! * settings-drift modal
//!
//! ## What's NOT in scope
//!
//! `src/output.rs`'s `display_path` (JSON / CSV emit) intentionally
//! does NOT strip the verbatim prefix. The output goes to programmatic
//! consumers (web's history ingest, dedupe action_receipt records)
//! where the canonical Win32 path form may be load-bearing. If we
//! ever decide JSON/CSV should also be verbatim-stripped, that's a
//! wire-format change + a coordinated web-side update; out of
//! scope for this consolidation.
//!
//! `gui::app` Log-panel message lines (many `path.display()` /
//! `path.to_string_lossy()` callsites in archive/copy/move/manifest
//! status messages) still render verbatim. They're cosmetic + don't
//! show on the dupe view that prompted #118; tracked for a v0.2.14
//! Log-panel sweep rather than bundled here.

use std::path::Path;

/// User-facing path display. Strips Windows verbatim-path prefix
/// (`\\?\`) so UI rows + tooltips show `C:\Foo\bar`. UNC verbatim
/// form (`\\?\UNC\server\share`) is rewritten back to
/// `\\server\share`. Paths without the prefix pass through
/// unchanged (Linux/macOS paths, regular Windows paths).
pub fn for_user_display(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return format!(r"\\{unc}");
        }
        return rest.to_string();
    }
    s.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_verbatim_drive_prefix() {
        let p = Path::new(r"\\?\C:\Windows\System32\notepad.exe");
        assert_eq!(for_user_display(p), r"C:\Windows\System32\notepad.exe");
    }

    #[test]
    fn drops_verbatim_drive_prefix_lowercase_drive() {
        // Some Windows APIs return lowercase drive letters in the
        // verbatim form. Stripping the prefix must not change the
        // case of what's underneath.
        let p = Path::new(r"\\?\c:\foo\bar.txt");
        assert_eq!(for_user_display(p), r"c:\foo\bar.txt");
    }

    #[test]
    fn rewrites_verbatim_unc_form() {
        let p = Path::new(r"\\?\UNC\fileserver\public\report.pdf");
        assert_eq!(for_user_display(p), r"\\fileserver\public\report.pdf");
    }

    #[test]
    fn passes_through_normal_windows_path() {
        let p = Path::new(r"C:\Users\Mick\Documents\thing.txt");
        assert_eq!(for_user_display(p), r"C:\Users\Mick\Documents\thing.txt");
    }

    #[test]
    fn passes_through_normal_unc_path() {
        let p = Path::new(r"\\fileserver\share\thing");
        assert_eq!(for_user_display(p), r"\\fileserver\share\thing");
    }

    #[test]
    fn passes_through_unix_paths() {
        let p = Path::new("/home/neomatrix/file.bin");
        assert_eq!(for_user_display(p), "/home/neomatrix/file.bin");
    }

    #[test]
    fn handles_root_verbatim_path() {
        let p = Path::new(r"\\?\D:\");
        assert_eq!(for_user_display(p), r"D:\");
    }

    #[test]
    fn empty_path_passes_through() {
        let p = Path::new("");
        assert_eq!(for_user_display(p), "");
    }
}
