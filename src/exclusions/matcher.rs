//! Pure-function matchers used by [`super::ExclusionPolicy::evaluate`].
//! Kept as standalone helpers (rather than inlined into the policy)
//! so unit tests can hit edge cases — trailing dot, multi-dot
//! filenames, no extension at all, mixed case, non-ASCII chars —
//! without spinning up the full policy + presets.

use std::path::Path;

/// Extract the filename's extension as lowercase, or `None` if the
/// file has no extension. The returned string never starts with
/// `.` (matches the normalisation [`super::normalize_extension`]
/// applies to the rule side, so comparison is symmetric).
///
/// Behaviour:
/// - `foo.txt`  → `Some("txt")`
/// - `Foo.TXT`  → `Some("txt")`
/// - `Makefile` → `None`
/// - `foo.tar.gz` → `Some("gz")` (matches `Path::extension`)
/// - `.bashrc`  → `None` (dotfile; no extension per `Path::extension`)
/// - `foo.`     → `Some("")` (trailing dot is a zero-length ext)
pub fn lowercased_extension(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|os| os.to_str())
        .map(|s| s.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_lowercase_extension() {
        assert_eq!(
            lowercased_extension(Path::new("foo.txt")),
            Some("txt".to_string())
        );
    }

    #[test]
    fn lowercases_mixed_case() {
        assert_eq!(
            lowercased_extension(Path::new("FOO.TXT")),
            Some("txt".to_string())
        );
        assert_eq!(
            lowercased_extension(Path::new("Bar.MixedCase")),
            Some("mixedcase".to_string())
        );
    }

    #[test]
    fn no_extension_returns_none() {
        assert_eq!(lowercased_extension(Path::new("Makefile")), None);
        assert_eq!(lowercased_extension(Path::new("CHANGELOG")), None);
    }

    #[test]
    fn dotfile_has_no_extension() {
        // Std lib treats leading-dot files as extensionless. Match
        // that behaviour so users see consistent rules — `.bashrc`
        // is not "extension bashrc."
        assert_eq!(lowercased_extension(Path::new(".bashrc")), None);
        assert_eq!(lowercased_extension(Path::new(".gitignore")), None);
    }

    #[test]
    fn multi_dot_takes_last_segment() {
        // foo.tar.gz → "gz". Aligns with `Path::extension`.
        assert_eq!(
            lowercased_extension(Path::new("archive.tar.gz")),
            Some("gz".to_string())
        );
    }

    #[test]
    fn trailing_dot_yields_empty_extension() {
        assert_eq!(lowercased_extension(Path::new("foo.")), Some(String::new()));
    }

    #[test]
    fn directory_path_no_extension() {
        // A path ending in `/` represents a dir; should not match
        // an extension rule. Note: Path::extension on a path with
        // no filename component returns None.
        assert_eq!(lowercased_extension(Path::new("/usr/lib/")), None);
    }

    #[test]
    fn nested_path_extracts_from_filename() {
        assert_eq!(
            lowercased_extension(Path::new("/a/b/c/foo.JPEG")),
            Some("jpeg".to_string())
        );
    }
}
