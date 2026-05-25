//! Linux trash via the freedesktop.org XDG Trash specification.
//!
//! Spec: https://specifications.freedesktop.org/trash-spec/trashspec-1.0.html
//!
//! Per `linux-roadmap.md` §5.1 L0 deliverable. The XDG Trash spec is
//! how GNOME / KDE / Cinnamon / XFCE etc. all interoperate: a file
//! sent to the trash from any tool can be restored from any other.
//!
//! ## Implementation choice
//!
//! Two approaches were considered:
//!
//! 1. **Shell out to `gio trash`** — works on most modern desktops
//!    (depends on glib2). Pros: short code, free interop with the
//!    user's chosen file manager. Cons: requires gio binary present
//!    (fails on headless servers + minimal installs); shells out
//!    per file (slow at scale); error reporting is via exit code,
//!    so we lose useful diagnostic context.
//!
//! 2. **Implement the spec directly.** Pros: no external binary
//!    dependency; faster (no fork/exec per file); typed error
//!    surface; works on headless servers via the home-dir trash.
//!    Cons: more code; must track the spec across versions.
//!
//! Going with (2) — pure-Rust implementation. The spec is small +
//! stable, the diagnostic value is real, and we avoid a runtime
//! dependency on gio.
//!
//! ## Trash directory selection
//!
//! Per spec §2:
//! * **Home filesystem files**: trash to `$XDG_DATA_HOME/Trash`
//!   (default `~/.local/share/Trash`).
//! * **External filesystem files** (e.g. a file on a separate
//!   mounted volume): trash to `<mount>/.Trash-$UID` (admin-
//!   trash) or `<mount>/.Trash/$UID` (user-trash, less preferred
//!   because the spec requires the admin-trash to exist + pass
//!   sticky-bit checks). For L0 we ALWAYS use the home trash;
//!   cross-volume support can land later (most user-facing files
//!   are under $HOME anyway).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{PlatformError, PlatformResult};

/// Outcome of a successful [`trash_file`] call. Caller (the dedupe
/// pipeline) feeds these fields into the action_receipt's
/// `recycle_bin_entry` block so integration-test harnesses can
/// verify the trash spec invariants without parsing the .trashinfo
/// file format directly. Per GH #33.
#[derive(Debug, Clone)]
pub struct TrashEntry {
    /// Absolute original path that got trashed (matches the `Path=`
    /// field in the .trashinfo file, pre URL-escaping).
    pub original_path: PathBuf,
    /// Trash root that received the file (typically
    /// `$XDG_DATA_HOME/Trash` or `$HOME/.local/share/Trash`).
    pub container: PathBuf,
    /// Full path to the `<base>.trashinfo` metadata file. Lives
    /// under `<container>/info/`.
    pub info_file: PathBuf,
    /// Full path to the moved-aside data file. Lives under
    /// `<container>/files/`. Same basename as info_file's stem.
    pub data_file: PathBuf,
}

pub fn trash_file(path: &Path) -> PlatformResult<TrashEntry> {
    let abs = fs::canonicalize(path).map_err(PlatformError::Io)?;

    let trash_root = trash_root()?;
    let files_dir = trash_root.join("files");
    let info_dir = trash_root.join("info");
    fs::create_dir_all(&files_dir).map_err(PlatformError::Io)?;
    fs::create_dir_all(&info_dir).map_err(PlatformError::Io)?;

    // The spec requires a unique filename in the files/ + info/
    // subdirs. Use the original filename; if it collides with an
    // existing trashed file (e.g. two files with the same name
    // trashed at different times), suffix with a counter.
    let base_name = abs
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unnamed".to_string());
    let (target_in_files, info_path) = pick_unique_name(&files_dir, &info_dir, &base_name)?;

    // The .trashinfo file must be written BEFORE the file is moved,
    // per spec §3 — if the file move succeeds but the info write
    // fails, the user has a file in trash that can't be restored.
    // Reverse order (write info, then move file) keeps the
    // invariant: info-file-without-data is fine (restore is a no-op)
    // but data-without-info is unrecoverable.
    let info_content = build_trashinfo(&abs);
    let mut info_f = fs::File::create(&info_path).map_err(PlatformError::Io)?;
    info_f
        .write_all(info_content.as_bytes())
        .map_err(PlatformError::Io)?;
    info_f.sync_all().map_err(PlatformError::Io)?;
    drop(info_f);

    // Now move the file. `fs::rename` is atomic on same-filesystem;
    // if src + trash are on different volumes, rename(2) returns
    // EXDEV and we'd need to copy + delete. For L0 we propagate
    // that as an Io error; cross-volume trash is a follow-up.
    if let Err(e) = fs::rename(&abs, &target_in_files) {
        // Clean up the orphaned info file so we don't leave
        // dangling references.
        let _ = fs::remove_file(&info_path);
        return Err(PlatformError::Io(e));
    }

    Ok(TrashEntry {
        original_path: abs,
        container: trash_root,
        info_file: info_path,
        data_file: target_in_files,
    })
}

/// `$XDG_DATA_HOME/Trash` (default `$HOME/.local/share/Trash`).
fn trash_root() -> PlatformResult<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let mut p = PathBuf::from(xdg);
        if p.is_absolute() {
            p.push("Trash");
            return Ok(p);
        }
        // Per XDG spec, non-absolute XDG_DATA_HOME values are
        // ignored.
    }
    let home = std::env::var_os("HOME").ok_or_else(|| {
        PlatformError::Other("HOME is not set; cannot resolve XDG Trash root".to_string())
    })?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    p.push("Trash");
    Ok(p)
}

/// Spec §3: the info file is a desktop-entry-format file with two
/// keys: `Path` (URL-escaped original path) and `DeletionDate` (RFC
/// 3339, local-time-without-timezone is what most implementations
/// emit — the spec is loose here but the common form is what
/// GNOME / KDE produce).
fn build_trashinfo(original: &Path) -> String {
    let path_escaped = url_escape_for_trashinfo(&original.to_string_lossy());
    let date = iso8601_local_seconds(now_unix_local());
    format!("[Trash Info]\nPath={path_escaped}\nDeletionDate={date}\n",)
}

/// Escape characters that have special meaning in a .trashinfo
/// Path field. Per spec §3, this should follow RFC 2396 ("URI"
/// generic syntax) — unreserved chars pass through, reserved +
/// non-ASCII get %XX-encoded.
///
/// Most existing implementations (gvfs, KIO) escape conservatively:
/// alphanumeric + `/._-~` pass through; everything else percent-
/// encodes. We follow that convention for max interop.
fn url_escape_for_trashinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let safe = b.is_ascii_alphanumeric()
            || b == b'/'
            || b == b'.'
            || b == b'_'
            || b == b'-'
            || b == b'~';
        if safe {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

fn pick_unique_name(
    files_dir: &Path,
    info_dir: &Path,
    base: &str,
) -> PlatformResult<(PathBuf, PathBuf)> {
    let candidate = files_dir.join(base);
    let info_candidate = info_dir.join(format!("{base}.trashinfo"));
    if !candidate.exists() && !info_candidate.exists() {
        return Ok((candidate, info_candidate));
    }
    // Spec §3 suggests suffixing with " 2", " 3", etc. We use a
    // numeric suffix that's safer for our use case (avoids spaces
    // in filenames + matches what most implementations do for
    // programmatic trashing).
    for n in 2..u32::MAX {
        let suffixed = format!("{base}.{n}");
        let c = files_dir.join(&suffixed);
        let i = info_dir.join(format!("{suffixed}.trashinfo"));
        if !c.exists() && !i.exists() {
            return Ok((c, i));
        }
    }
    Err(PlatformError::Other(
        "could not find a unique trash filename after 2^32 attempts".to_string(),
    ))
}

/// Wall-clock seconds since epoch (local, not UTC — XDG Trash uses
/// local time in the DeletionDate field).
fn now_unix_local() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Render a unix timestamp as `YYYY-MM-DDTHH:MM:SS` (no timezone).
/// XDG Trash spec is loose on TZ — most implementations omit it.
/// Hand-rolled to avoid pulling chrono just for one format string.
fn iso8601_local_seconds(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let h = (secs / 3_600) as u32;
    let m = ((secs % 3_600) / 60) as u32;
    let s = (secs % 60) as u32;
    // Howard Hinnant civil-from-days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (y + if mo <= 2 { 1 } else { 0 }) as i32;
    format!("{year:04}-{mo:02}-{day:02}T{h:02}:{m:02}:{s:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Process-wide serial gate for the env-mutating tests below.
    /// Three tests (`trash_root_uses_xdg_data_home_when_absolute`,
    /// `trash_root_falls_back_to_home_local_share`,
    /// `trash_file_round_trip_under_isolated_home`) all
    /// `env::set_var("XDG_DATA_HOME", ...)` + `env::set_var("HOME", ...)`,
    /// then restore on exit. When cargo runs them in parallel within
    /// the same binary the env vars race — test A's `trash_root()`
    /// can read test B's mid-flight HOME and write to the wrong
    /// directory. Manifested as
    /// `trash_file_round_trip_under_isolated_home` failing on faster
    /// CI runners (locally they happen to serialise by accident).
    ///
    /// Tests that don't touch env (url-escape, build_trashinfo,
    /// iso8601) don't need the gate and stay parallel.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn url_escape_passes_safe_chars() {
        assert_eq!(
            url_escape_for_trashinfo("/home/neomatrix/foo.bin"),
            "/home/neomatrix/foo.bin"
        );
    }

    #[test]
    fn url_escape_escapes_specials() {
        assert_eq!(
            url_escape_for_trashinfo("/path with space"),
            "/path%20with%20space"
        );
        assert_eq!(url_escape_for_trashinfo("/a&b=c"), "/a%26b%3Dc");
        // Unicode passes through as %XX-encoded UTF-8 bytes.
        assert_eq!(url_escape_for_trashinfo("a/é"), "a/%C3%A9");
    }

    #[test]
    fn build_trashinfo_contains_required_keys() {
        let p = Path::new("/home/x/file.bin");
        let s = build_trashinfo(p);
        assert!(s.starts_with("[Trash Info]\n"));
        assert!(s.contains("Path=/home/x/file.bin"));
        assert!(s.contains("DeletionDate="));
    }

    #[test]
    fn iso8601_renders_epoch_correctly() {
        // 1970-01-01T00:00:00 in local-time-without-TZ. We emit
        // the raw seconds-since-epoch so this is what falls out.
        assert_eq!(iso8601_local_seconds(0), "1970-01-01T00:00:00");
        // Test vector also used by submission.rs.
        assert_eq!(iso8601_local_seconds(1_704_067_200), "2024-01-01T00:00:00");
    }

    #[test]
    fn trash_root_uses_xdg_data_home_when_absolute() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // Save + restore so the test is hermetic.
        let prev = std::env::var_os("XDG_DATA_HOME");
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("XDG_DATA_HOME", "/custom/xdg/path");
        std::env::set_var("HOME", "/home/test");
        let root = trash_root().unwrap();
        assert_eq!(root, PathBuf::from("/custom/xdg/path/Trash"));
        // Restore.
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn trash_root_falls_back_to_home_local_share() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("XDG_DATA_HOME");
        let prev_home = std::env::var_os("HOME");
        std::env::remove_var("XDG_DATA_HOME");
        std::env::set_var("HOME", "/home/test");
        let root = trash_root().unwrap();
        assert_eq!(root, PathBuf::from("/home/test/.local/share/Trash"));
        match prev {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn trash_file_round_trip_under_isolated_home() {
        let _guard = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        // Build a synthetic XDG home + HOME, point trash at it,
        // trash a file, then assert it's gone from src + present
        // in `<home>/.local/share/Trash/files/` with a matching
        // .trashinfo.
        let tmpdir = std::env::temp_dir().join(format!(
            "sd-trash-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmpdir).unwrap();
        let fake_home = tmpdir.join("fake_home");
        fs::create_dir_all(&fake_home).unwrap();

        let target = tmpdir.join("doomed.txt");
        fs::write(&target, b"to the trash with you").unwrap();

        // Set HOME so trash_root picks fake_home/.local/share/Trash.
        let prev_home = std::env::var_os("HOME");
        let prev_xdg = std::env::var_os("XDG_DATA_HOME");
        std::env::set_var("HOME", &fake_home);
        std::env::remove_var("XDG_DATA_HOME");

        trash_file(&target).expect("trash_file");

        // Original location: gone.
        assert!(!target.exists(), "original target should be gone");

        // Trash files dir: has doomed.txt.
        let trashed = fake_home.join(".local/share/Trash/files/doomed.txt");
        assert!(trashed.exists(), "doomed.txt should be in trash files dir");
        let bytes = fs::read(&trashed).unwrap();
        assert_eq!(bytes, b"to the trash with you");

        // Info dir: has doomed.txt.trashinfo with Path= matching
        // the canonical original location.
        let info_path = fake_home.join(".local/share/Trash/info/doomed.txt.trashinfo");
        assert!(info_path.exists(), "trashinfo should exist");
        let info = fs::read_to_string(&info_path).unwrap();
        assert!(info.starts_with("[Trash Info]\n"));
        assert!(info.contains("Path="));
        assert!(info.contains("DeletionDate="));

        // Restore env + clean up.
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_xdg {
            Some(v) => std::env::set_var("XDG_DATA_HOME", v),
            None => std::env::remove_var("XDG_DATA_HOME"),
        }
        let _ = fs::remove_dir_all(&tmpdir);
    }
}
