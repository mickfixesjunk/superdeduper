//! Pick a `PreviewMode` for a file based on extension + a small
//! content sniff. The sniff is cheap (first 4 KiB) so a
//! misclassified binary (e.g. a CSV mislabelled .bin) still has a
//! chance to land on the text viewer.
//!
//! On Windows, when the auto-classification would otherwise fall
//! back to hex, we ALSO check the registry for a registered
//! `IPreviewHandler` CLSID — if one exists, classify as
//! `SystemHandler` so the user sees the "system handler available"
//! indicator (full COM hosting is v2 work; today this is just a
//! signal that the file has a registered handler).
//!
//! Cross-platform: never returns `SystemHandler` outside Windows.

use std::path::Path;

use super::PreviewMode;

/// Extension allowlist that always maps to the text viewer.
/// Anything that's "obviously text" — source code, configs, logs,
/// markdown, JSON, YAML. Sniffing skips the UTF-8 cost on these.
const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "md",
    "rst",
    "log",
    "csv",
    "tsv",
    "json",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "rs",
    "py",
    "js",
    "ts",
    "jsx",
    "tsx",
    "go",
    "java",
    "c",
    "cpp",
    "cc",
    "cxx",
    "h",
    "hpp",
    "hxx",
    "cs",
    "rb",
    "php",
    "swift",
    "kt",
    "kts",
    "scala",
    "clj",
    "ex",
    "exs",
    "erl",
    "hs",
    "ml",
    "fs",
    "fsx",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "xml",
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "vue",
    "svelte",
    "lua",
    "vim",
    "dockerfile",
    "gitignore",
    "gitattributes",
    "editorconfig",
    "lock",
    "license",
    "readme",
];

pub fn classify_path(path: &Path) -> PreviewMode {
    // Fast-path 1: file doesn't exist / can't be metadata'd —
    // surface "unavailable" with the actual error string so the
    // user knows why.
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            return PreviewMode::Unavailable {
                reason: format!("cannot stat: {e}"),
            };
        }
    };
    if !meta.is_file() {
        return PreviewMode::Unavailable {
            reason: "not a regular file".to_string(),
        };
    }

    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    // Extension allowlist → text viewer.
    if TEXT_EXTENSIONS.iter().any(|allowed| *allowed == ext) {
        return PreviewMode::Text;
    }

    // Cheap content sniff: read first 4 KiB, check for valid UTF-8
    // with mostly-printable characters. If yes → text viewer.
    // Otherwise → SystemHandler (Windows registered) or Hex.
    if let Ok(sniff) = read_sniff(path) {
        if looks_like_text(&sniff) {
            return PreviewMode::Text;
        }
    }

    // On Windows, check whether the system has a registered
    // IPreviewHandler for this extension. Surface that as
    // SystemHandler so the user sees the "preview handler
    // available" indicator + (eventually) the COM-hosted preview.
    #[cfg(windows)]
    {
        if !ext.is_empty() {
            if let Some(clsid) = super::registry_lookup::handler_clsid(&ext) {
                return PreviewMode::SystemHandler { clsid };
            }
        }
    }

    PreviewMode::Hex
}

/// Read up to 4 KiB from the start of the file for the sniff.
fn read_sniff(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 4096];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(buf)
}

/// Heuristic: a buffer "looks like text" iff it's valid UTF-8 and
/// at least 90% of its bytes are in the printable-ASCII range
/// (plus tab, CR, LF). 4 KiB cap so the sniff is bounded.
fn looks_like_text(buf: &[u8]) -> bool {
    if buf.is_empty() {
        // Empty file — let the text viewer handle it (renders "<empty>").
        return true;
    }
    if std::str::from_utf8(buf).is_err() {
        return false;
    }
    let printable = buf
        .iter()
        .filter(|&&b| b == b'\t' || b == b'\r' || b == b'\n' || (0x20..=0x7E).contains(&b))
        .count();
    // Be lenient on UTF-8 multi-byte chars; the from_utf8 check
    // above already proved they're well-formed.
    let total = buf.len();
    (printable * 10) >= (total * 9)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn classify_missing_file_unavailable() {
        let mode = classify_path(Path::new("/this/does/not/exist.txt"));
        assert!(matches!(mode, PreviewMode::Unavailable { .. }));
    }

    #[test]
    fn classify_text_extension_picks_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("hello.md");
        std::fs::write(&p, b"# hi\nthis is markdown\n").unwrap();
        assert_eq!(classify_path(&p), PreviewMode::Text);
    }

    #[test]
    fn classify_unknown_extension_but_text_content_sniffs_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("note.weirdext");
        std::fs::write(&p, b"this is plain text with no recognised extension\n").unwrap();
        assert_eq!(classify_path(&p), PreviewMode::Text);
    }

    #[test]
    fn classify_binary_content_sniffs_hex_or_system_handler() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin.dat");
        let mut f = std::fs::File::create(&p).unwrap();
        // Binary noise — definitely fails the UTF-8 check.
        let bytes: Vec<u8> = (0..=255).cycle().take(1024).collect();
        f.write_all(&bytes).unwrap();
        let mode = classify_path(&p);
        // Either Hex (no registered handler) or SystemHandler if
        // the test box happens to have one registered for `.dat`.
        assert!(matches!(
            mode,
            PreviewMode::Hex | PreviewMode::SystemHandler { .. }
        ));
    }

    #[test]
    fn classify_empty_file_is_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.unknown");
        std::fs::write(&p, b"").unwrap();
        // Empty content sniffs as "text" (sniff returns Ok([])
        // then looks_like_text says true). Acceptable: text viewer
        // can render "<empty>" gracefully.
        assert_eq!(classify_path(&p), PreviewMode::Text);
    }
}
