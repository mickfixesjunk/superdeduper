//! Canonical containment-test snapshot — `sd debug snapshot <path>`.
//!
//! Emits the JSON shape locked in
//! `testdesign/specs/containment-fixtures/_snapshot-schema-examples.md`.
//! The harness diffs pre + post snapshots and asserts equality with
//! the per-action expected delta. Engine MUST emit consistent values
//! for the same input; that's the only thing the harness depends on.
//!
//! ## V1 scope (this commit)
//!
//! Fully captured cross-platform: `path`, `type`, `inode`, `size_bytes`,
//! `content_hash_sha256`, `nlink`, `mtime_ns`, `acl_hash`,
//! `is_reparse_point`, `reparse_target` (for symlinks).
//!
//! Reduced-fidelity V1 gaps, documented so testdesign + executors can
//! decide whether they need them before a follow-up lands:
//!
//! - **ACL canonicalisation**: Linux/POSIX uses `mode | uid<<32 | gid<<48`
//!   only (no xattrs); Windows uses the `file_attributes` u32 only
//!   (no SDDL). Containment tests assert opaque equality, so both
//!   variants are stable + sufficient for "did the ACL change?" but
//!   won't detect ACL-only changes that don't move those fields
//!   (e.g. DACL-entry add on Windows). Documented as a V1 limitation.
//! - **Alternate data streams**: always emitted as `[]`. Windows ADS
//!   enumeration via `FindFirstStreamW` is a separate follow-up; the
//!   fixture set should declare ADS coverage explicitly if needed.
//! - **`reparse_tag` for non-symlink reparse points** (cloud
//!   placeholders, junctions, etc.): emitted as `null` even when
//!   `is_reparse_point: true`. The walker has classification logic
//!   we can plumb in; deferred to keep V1 small.
//! - **Cloud-placeholder safety**: content hash is `null` for entries
//!   where the file's metadata indicates a recall-on-open / recall-on-
//!   data-access reparse point. We never open such files for read.
//!
//! ## Determinism
//!
//! - Entries sorted by `path` (UTF-8 byte ascending) per spec §1.
//! - Content hash is SHA-256 of the raw byte stream (sparse holes
//!   read as zeros — matches default Linux + Windows behaviour).
//! - `mtime_ns` is nanoseconds since Unix epoch. `mtime_precision`
//!   declares the filesystem-supported granularity so the harness
//!   can relax equality on coarse-precision filesystems.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Schema version emitted in the top-level `schema` field. Bump in
/// lockstep with any breaking shape change; non-breaking additions
/// (new optional fields) don't require a bump.
pub const SCHEMA_VERSION: &str = "superdeduper.snapshot.v1";

/// Top-level snapshot JSON shape (per spec §2.1).
#[derive(Debug, Clone, Serialize)]
pub struct Snapshot {
    pub schema: String,
    pub root_path: String,
    pub captured_at: String,
    pub filesystem: String,
    pub mtime_precision: MtimePrecision,
    pub entries: Vec<SnapshotEntry>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MtimePrecision {
    Ns,
    Us,
    Ms,
    S,
}

/// One entry in the snapshot. Mirrors the shape locked in
/// `_snapshot-schema-examples.md`. `null`-able fields use `Option`;
/// serde drops them as `null` for JSON output (we want explicit
/// nulls so the schema is visually obvious to harness reviewers).
#[derive(Debug, Clone, Serialize)]
pub struct SnapshotEntry {
    pub path: String,
    #[serde(rename = "type")]
    pub entry_type: EntryType,
    pub inode: String,
    pub size_bytes: Option<u64>,
    pub content_hash_sha256: Option<String>,
    pub nlink: u32,
    pub mtime_ns: i128,
    pub acl_hash: String,
    pub is_reparse_point: bool,
    pub reparse_tag: Option<String>,
    pub reparse_target: Option<String>,
    pub alternate_data_streams: Vec<AdsEntry>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryType {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Serialize)]
pub struct AdsEntry {
    pub name: String,
    pub size_bytes: u64,
    pub content_hash_sha256: String,
}

/// Top-level entry point. Walks `root` recursively + builds a
/// deterministic Snapshot. Symlinks are NOT followed; their target
/// string is captured as metadata only.
///
/// Returns `Err` on:
/// - Root path doesn't exist (`ErrorKind::NotFound`)
/// - Permission denied to enumerate any subtree
/// - I/O error during a metadata or content-hash read
pub fn capture(root: &Path) -> io::Result<Snapshot> {
    let canonical = root.canonicalize().or_else(|_| -> io::Result<PathBuf> {
        // canonicalize() rejects non-existent paths; we want the
        // not-found error to surface from the walker's first stat
        // call rather than here so the error message is consistent
        // with "root doesn't exist."
        Ok(root.to_path_buf())
    })?;
    let mut entries: Vec<SnapshotEntry> = Vec::new();
    walk_recursive(&canonical, &mut entries)?;
    entries.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let captured_at = rfc3339_now();
    let filesystem = detect_filesystem(&canonical);
    let mtime_precision = detect_mtime_precision(&filesystem);
    Ok(Snapshot {
        schema: SCHEMA_VERSION.to_string(),
        root_path: canonical.display().to_string(),
        captured_at,
        filesystem,
        mtime_precision,
        entries,
    })
}

fn walk_recursive(path: &Path, out: &mut Vec<SnapshotEntry>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    out.push(build_entry(path, &metadata)?);
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        let mut children: Vec<PathBuf> = fs::read_dir(path)?
            .filter_map(|r| r.ok().map(|e| e.path()))
            .collect();
        children.sort();
        for child in children {
            walk_recursive(&child, out)?;
        }
    }
    Ok(())
}

fn build_entry(path: &Path, metadata: &fs::Metadata) -> io::Result<SnapshotEntry> {
    let file_type = metadata.file_type();
    let entry_type = if file_type.is_symlink() {
        EntryType::Symlink
    } else if file_type.is_dir() {
        EntryType::Directory
    } else {
        EntryType::File
    };

    let (inode_value, nlink, attributes) = platform_inode_nlink_attrs(path, metadata)?;
    let inode = format!("0x{:016x}", inode_value);

    let mtime_ns = mtime_to_ns(metadata);
    let acl_hash = platform_acl_hash(metadata);

    let is_symlink = matches!(entry_type, EntryType::Symlink);
    let is_reparse_point = is_symlink || platform_is_reparse_point(attributes);
    let reparse_tag = if is_symlink {
        Some("IO_REPARSE_TAG_SYMLINK".to_string())
    } else {
        // Non-symlink reparse points (cloud placeholders, junctions,
        // etc.) — V1 reports presence via is_reparse_point but
        // leaves the tag null. Follow-up can plumb in the walker's
        // PlaceholderState classification.
        None
    };
    let reparse_target = if is_symlink {
        fs::read_link(path).ok().map(|p| p.display().to_string())
    } else {
        None
    };

    // Content hash + size for files; null/null for directories +
    // reparse points (spec §2.1 reparse-point handling rule:
    // TAG-ONLY, no content fetch — protects cloud-placeholder
    // safety + matches T2.1 walker's classify() flow).
    let (size_bytes, content_hash_sha256) = match entry_type {
        EntryType::Directory => (None, None),
        EntryType::Symlink => (None, None),
        EntryType::File if is_reparse_point => (Some(metadata.len()), None),
        EntryType::File => {
            let h = hash_file_content(path)?;
            (Some(metadata.len()), Some(h))
        }
    };

    Ok(SnapshotEntry {
        path: path.display().to_string(),
        entry_type,
        inode,
        size_bytes,
        content_hash_sha256,
        nlink,
        mtime_ns,
        acl_hash,
        is_reparse_point,
        reparse_tag,
        reparse_target,
        alternate_data_streams: Vec::new(),
    })
}

fn hash_file_content(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn mtime_to_ns(metadata: &fs::Metadata) -> i128 {
    metadata
        .modified()
        .ok()
        .and_then(|t| {
            t.duration_since(SystemTime::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_nanos() as i128)
                .or_else(|| {
                    // Pre-1970 mtime (rare but possible). Negative.
                    SystemTime::UNIX_EPOCH
                        .duration_since(t)
                        .ok()
                        .map(|d| -(d.as_nanos() as i128))
                })
        })
        .unwrap_or(0)
}

fn rfc3339_now() -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    // Civil-from-days (Howard Hinnant) — copied from action_receipt
    // for self-containment; keep in sync if the upstream changes.
    let days = secs.div_euclid(86_400);
    let secs_in_day = secs.rem_euclid(86_400) as u32;
    let hours = secs_in_day / 3600;
    let minutes = (secs_in_day % 3600) / 60;
    let seconds = secs_in_day % 60;
    let (y, m, d) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y, m, d, hours, minutes, seconds
    )
}

fn days_to_ymd(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i32 + (era as i32) * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(unix)]
fn platform_inode_nlink_attrs(
    _path: &Path,
    metadata: &fs::Metadata,
) -> io::Result<(u64, u32, u64)> {
    use std::os::unix::fs::MetadataExt;
    Ok((
        metadata.ino(),
        metadata.nlink() as u32,
        metadata.mode() as u64,
    ))
}

#[cfg(windows)]
fn platform_inode_nlink_attrs(path: &Path, metadata: &fs::Metadata) -> io::Result<(u64, u32, u64)> {
    use std::os::windows::fs::MetadataExt;
    // file_attributes() is cheap + already-resolved on the metadata
    // we have; cover that path. nlink requires a separate open +
    // GetFileInformationByHandle call (the same one the walker uses
    // at inventory/walk.rs:695). Defer to the winapi_wrappers
    // helper so we don't duplicate the unsafe block.
    let attributes = metadata.file_attributes() as u64;
    let (inode, nlink) = win_file_info(path).unwrap_or((0, 1));
    Ok((inode, nlink, attributes))
}

#[cfg(windows)]
fn win_file_info(path: &Path) -> Option<(u64, u32)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_NO_RECALL, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    // Wide-string nul-terminated for CreateFileW. Reuses the
    // verbatim-path convention the walker established (commit
    // 7826172): long paths + reserved names go through cleanly.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle: HANDLE = unsafe {
        CreateFileW(
            PCWSTR(wide.as_ptr()),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            // BACKUP_SEMANTICS so directories can be opened too;
            // OPEN_REPARSE_POINT so we get the link itself, not the
            // target; OPEN_NO_RECALL so cloud placeholders don't
            // hydrate when we touch them. Same flag set the walker
            // uses for safe-stat.
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_OPEN_NO_RECALL,
            None,
        )
        .ok()?
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info).is_ok() };
    unsafe {
        let _ = CloseHandle(handle);
    }
    if !ok {
        return None;
    }
    let inode = ((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64);
    Some((inode, info.nNumberOfLinks))
}

#[cfg(not(any(unix, windows)))]
fn platform_inode_nlink_attrs(
    _path: &Path,
    _metadata: &fs::Metadata,
) -> io::Result<(u64, u32, u64)> {
    // Unsupported platform; emit conservative defaults so the
    // snapshot still completes.
    Ok((0, 1, 0))
}

#[cfg(unix)]
fn platform_acl_hash(metadata: &fs::Metadata) -> String {
    use std::os::unix::fs::MetadataExt;
    let mode = metadata.mode();
    let uid = metadata.uid();
    let gid = metadata.gid();
    let mut hasher = Sha256::new();
    hasher.update(b"posix:");
    hasher.update(mode.to_le_bytes());
    hasher.update(uid.to_le_bytes());
    hasher.update(gid.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(windows)]
fn platform_acl_hash(metadata: &fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    let attributes = metadata.file_attributes();
    let mut hasher = Sha256::new();
    hasher.update(b"win:");
    hasher.update(attributes.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(not(any(unix, windows)))]
fn platform_acl_hash(_metadata: &fs::Metadata) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"unsupported");
    format!("{:x}", hasher.finalize())
}

#[cfg(windows)]
fn platform_is_reparse_point(attributes: u64) -> bool {
    // FILE_ATTRIBUTE_REPARSE_POINT = 0x400.
    (attributes & 0x400) != 0
}

#[cfg(not(windows))]
fn platform_is_reparse_point(_attributes: u64) -> bool {
    // POSIX: only symlinks register as reparse points in our shape;
    // the symlink check happens upstream of this function.
    false
}

fn detect_filesystem(_root: &Path) -> String {
    // V1: emit a coarse platform tag. Filesystem detail (NTFS vs
    // ReFS, ext4 vs xfs) is filesystem-introspection-API territory
    // and not strictly required for the harness — it only uses this
    // field to relax mtime precision. Follow-up can wire in
    // platform-specific detection.
    #[cfg(target_os = "linux")]
    {
        "ext4-or-similar".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "apfs-or-similar".to_string()
    }
    #[cfg(windows)]
    {
        "ntfs-or-similar".to_string()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        "unknown".to_string()
    }
}

fn detect_mtime_precision(filesystem: &str) -> MtimePrecision {
    // Pessimistic default per filesystem family. Override if we
    // ever wire in real introspection.
    if filesystem.contains("fat") {
        MtimePrecision::S
    } else if filesystem.contains("ntfs") {
        // NTFS records mtime in 100ns ticks → effective precision
        // is 100 nanoseconds; round to `ns` for the schema enum.
        MtimePrecision::Ns
    } else {
        MtimePrecision::Ns
    }
}

/// Stream a snapshot as pretty-printed JSON to `w`. Sorted entries
/// + stable schema make the output diff-friendly.
pub fn write_json<W: io::Write>(snapshot: &Snapshot, mut w: W) -> io::Result<()> {
    let json = serde_json::to_string_pretty(snapshot)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    w.write_all(json.as_bytes())?;
    w.write_all(b"\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_root() -> tempfile::TempDir {
        tempfile::tempdir().expect("create tempdir")
    }

    #[test]
    fn captures_single_file_with_content_hash() {
        let dir = temp_root();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, b"hello world\n").unwrap();
        let snap = capture(dir.path()).expect("snapshot succeeds");
        assert_eq!(snap.schema, SCHEMA_VERSION);
        // Root dir + the file = 2 entries.
        assert_eq!(snap.entries.len(), 2);
        let file_entry = snap
            .entries
            .iter()
            .find(|e| e.path.ends_with("hello.txt"))
            .expect("file entry present");
        assert!(matches!(file_entry.entry_type, EntryType::File));
        assert_eq!(file_entry.size_bytes, Some(12));
        // sha256("hello world\n") =
        // a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447
        assert_eq!(
            file_entry.content_hash_sha256.as_deref().unwrap(),
            "a948904f2f0f479b8f8197694b30184b0d2ed1c1cd2a1ec0fb85d299a192a447"
        );
        assert!(file_entry.nlink >= 1);
        assert!(!file_entry.is_reparse_point);
        assert!(file_entry.alternate_data_streams.is_empty());
    }

    #[test]
    fn directories_have_null_size_and_content_hash() {
        let dir = temp_root();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let snap = capture(dir.path()).expect("snapshot succeeds");
        let subdir_entry = snap
            .entries
            .iter()
            .find(|e| e.path.ends_with("subdir"))
            .expect("subdir entry");
        assert!(matches!(subdir_entry.entry_type, EntryType::Directory));
        assert!(subdir_entry.size_bytes.is_none());
        assert!(subdir_entry.content_hash_sha256.is_none());
    }

    #[test]
    fn entries_sorted_by_path_for_deterministic_diff() {
        let dir = temp_root();
        // Write in scrambled order; expect sorted on output.
        std::fs::write(dir.path().join("c.txt"), b"c").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        let snap = capture(dir.path()).expect("snapshot succeeds");
        let paths: Vec<&str> = snap.entries.iter().map(|e| e.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "entries must be sorted by path");
    }

    #[test]
    fn inode_format_is_0x_prefixed_16_hex_chars() {
        let dir = temp_root();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        let snap = capture(dir.path()).unwrap();
        for entry in &snap.entries {
            assert!(
                entry.inode.starts_with("0x"),
                "inode missing 0x prefix: {}",
                entry.inode
            );
            assert_eq!(
                entry.inode.len(),
                18,
                "inode hex must be 16 chars + 0x: {}",
                entry.inode
            );
            assert!(
                entry.inode[2..].chars().all(|c| c.is_ascii_hexdigit()),
                "inode contains non-hex char: {}",
                entry.inode
            );
        }
    }

    #[test]
    fn acl_hash_is_64_hex_chars() {
        let dir = temp_root();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        let snap = capture(dir.path()).unwrap();
        for entry in &snap.entries {
            assert_eq!(
                entry.acl_hash.len(),
                64,
                "acl_hash must be 64 hex chars (SHA-256): {}",
                entry.acl_hash
            );
            assert!(entry.acl_hash.chars().all(|c| c.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn captured_at_is_rfc3339_zulu() {
        let dir = temp_root();
        let snap = capture(dir.path()).unwrap();
        // Format like "2026-05-24T20:34:51Z".
        assert_eq!(snap.captured_at.len(), 20);
        assert!(snap.captured_at.ends_with('Z'));
        assert_eq!(snap.captured_at.chars().nth(4).unwrap(), '-');
        assert_eq!(snap.captured_at.chars().nth(7).unwrap(), '-');
        assert_eq!(snap.captured_at.chars().nth(10).unwrap(), 'T');
        assert_eq!(snap.captured_at.chars().nth(13).unwrap(), ':');
        assert_eq!(snap.captured_at.chars().nth(16).unwrap(), ':');
    }

    #[test]
    fn schema_field_is_v1() {
        let dir = temp_root();
        let snap = capture(dir.path()).unwrap();
        assert_eq!(snap.schema, "superdeduper.snapshot.v1");
    }

    #[test]
    fn nested_directories_walk_recursively() {
        let dir = temp_root();
        let sub = dir.path().join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("deep.txt"), b"deep").unwrap();
        let snap = capture(dir.path()).unwrap();
        // root + a + a/b + a/b/c + a/b/c/deep.txt = 5 entries.
        assert_eq!(snap.entries.len(), 5);
    }

    #[test]
    fn write_json_round_trips_to_valid_json() {
        let dir = temp_root();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        let snap = capture(dir.path()).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_json(&snap, &mut buf).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(parsed["schema"], "superdeduper.snapshot.v1");
        assert!(parsed["entries"].is_array());
        assert!(parsed["captured_at"].is_string());
    }

    #[test]
    fn missing_root_path_surfaces_io_error() {
        let dir = temp_root();
        let missing = dir.path().join("does-not-exist");
        let err = capture(&missing).expect_err("must error on missing root");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_captured_as_reparse_point_without_content_hash() {
        let dir = temp_root();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, b"target content").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let snap = capture(dir.path()).unwrap();
        let link_entry = snap
            .entries
            .iter()
            .find(|e| e.path.ends_with("link"))
            .expect("symlink entry");
        assert!(matches!(link_entry.entry_type, EntryType::Symlink));
        assert!(link_entry.is_reparse_point);
        assert_eq!(
            link_entry.reparse_tag.as_deref(),
            Some("IO_REPARSE_TAG_SYMLINK")
        );
        assert!(link_entry.content_hash_sha256.is_none());
        assert!(link_entry.reparse_target.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn hardlinks_share_inode_and_report_nlink_gt_1() {
        let dir = temp_root();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        std::fs::write(&a, b"shared").unwrap();
        std::fs::hard_link(&a, &b).unwrap();
        let snap = capture(dir.path()).unwrap();
        let a_entry = snap
            .entries
            .iter()
            .find(|e| e.path.ends_with("a.txt"))
            .unwrap();
        let b_entry = snap
            .entries
            .iter()
            .find(|e| e.path.ends_with("b.txt"))
            .unwrap();
        assert_eq!(a_entry.inode, b_entry.inode, "hardlinks must share inode");
        assert_eq!(a_entry.nlink, 2);
        assert_eq!(b_entry.nlink, 2);
        // Same content → same content hash.
        assert_eq!(a_entry.content_hash_sha256, b_entry.content_hash_sha256);
    }

    #[test]
    fn empty_file_has_well_known_sha256() {
        let dir = temp_root();
        let f = dir.path().join("empty.txt");
        std::fs::File::create(&f).unwrap().flush().unwrap();
        let snap = capture(dir.path()).unwrap();
        let entry = snap
            .entries
            .iter()
            .find(|e| e.path.ends_with("empty.txt"))
            .unwrap();
        // SHA-256 of the empty byte stream.
        assert_eq!(
            entry.content_hash_sha256.as_deref().unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(entry.size_bytes, Some(0));
    }
}
