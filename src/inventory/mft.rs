//! Stage 1, fast path — direct MFT enumeration via `FSCTL_ENUM_USN_DATA`.
//!
//! For every NTFS volume covered by the scan roots, we:
//! 1. Open `\\.\Volume{…}\` with `CreateFileW`.
//! 2. Page through `FSCTL_ENUM_USN_DATA`, accumulating
//!    `(file_ref, parent_ref, name, attributes, usn)` for every
//!    on-volume record.
//! 3. Reconstruct full paths bottom-up by walking parent refs, caching
//!    every directory's resolved path so each is built at most once.
//! 4. Filter the result to records whose reconstructed path falls
//!    under one of the user's scan roots and matches the include /
//!    exclude globs.
//!
//! Records we can't reconstruct (orphans, unreachable parents) are
//! logged at debug level and dropped — they never appear in the
//! output. The walker fallback picks them up if the user disabled MFT
//! enumeration entirely.

use std::path::{Path, PathBuf};

use crate::config::ScanConfig;
use crate::inventory::FileEntry;
use crate::Result;

#[cfg(windows)]
pub fn enumerate(cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    use hashbrown::HashMap;

    use crate::winapi_wrappers::volume_for_path;

    // Group roots by volume.
    //
    // `std::fs::canonicalize` on Windows returns a verbatim path
    // (`\\?\F:\Github`), but the path we reconstruct from MFT
    // records starts with `F:\…` — no verbatim prefix. Comparing
    // those with `starts_with` returns false for every file, which
    // before this fix meant a scan of `F:\Github` from an
    // MFT-capable shell finished in seconds with 0 files. Strip the
    // verbatim prefix here so the two path shapes line up.
    let mut roots_by_volume: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for root in &cfg.roots {
        let abs = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let abs = strip_verbatim_prefix(&abs);
        let vol = volume_for_path(&abs)?;
        roots_by_volume.entry(vol).or_default().push(abs);
    }

    let mut out = Vec::new();
    for (volume, roots) in roots_by_volume {
        out.extend(enumerate_volume(&volume, &roots, cfg)?);
    }
    Ok(out)
}

#[cfg(not(windows))]
pub fn enumerate(_cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    Err(crate::Error::Unsupported(
        "inventory::mft: only available on Windows",
    ))
}

#[cfg(windows)]
fn enumerate_volume(volume: &str, roots: &[PathBuf], cfg: &ScanConfig) -> Result<Vec<FileEntry>> {
    use hashbrown::HashMap;

    use crate::winapi_wrappers::{UsnEnum, UsnRecord};

    let mut by_ref: HashMap<u64, UsnRecord> = HashMap::new();
    let mut max_usn: i64 = 0;

    let mut enumerator = UsnEnum::open(volume)?;
    while let Some(batch) = enumerator.next_batch()? {
        for r in batch {
            if r.usn > max_usn {
                max_usn = r.usn;
            }
            by_ref.insert(r.file_ref, r);
        }
    }

    let volume_root = PathBuf::from(volume.trim_end_matches('\\'));
    let mut path_cache: HashMap<u64, PathBuf> = HashMap::new();
    let mut out = Vec::new();
    for (_ref, record) in &by_ref {
        // Skip directories — only files become FileEntry rows.
        let is_dir = (record.attributes & 0x10) != 0; // FILE_ATTRIBUTE_DIRECTORY
        if is_dir {
            continue;
        }
        let full = match reconstruct_path(record.file_ref, &by_ref, &mut path_cache, &volume_root) {
            Some(p) => p,
            None => continue,
        };
        if !under_any_root(&full, roots) {
            continue;
        }
        // FSCTL_ENUM_USN_DATA doesn't include file size. We fetch it via
        // GetFileAttributesEx for files that survived the path filter.
        let size = match std::fs::metadata(&full) {
            Ok(m) => m.len(),
            Err(_) => continue,
        };
        if size < cfg.min_size {
            continue;
        }
        if let Some(max) = cfg.max_size {
            if size > max {
                continue;
            }
        }
        if !path_passes_globs(&full, cfg) {
            continue;
        }
        out.push(FileEntry {
            path: full,
            size,
            mtime: record.mtime_filetime,
            file_ref: record.file_ref,
            parent_ref: record.parent_ref,
            usn: record.usn,
            attributes: record.attributes,
            volume_guid: Some(volume.to_string()),
        });
    }

    Ok(out)
}

#[cfg(windows)]
fn reconstruct_path(
    file_ref: u64,
    by_ref: &hashbrown::HashMap<u64, crate::winapi_wrappers::UsnRecord>,
    cache: &mut hashbrown::HashMap<u64, PathBuf>,
    volume_root: &Path,
) -> Option<PathBuf> {
    if let Some(p) = cache.get(&file_ref) {
        return Some(p.clone());
    }
    // Walk parents up to a sane recursion bound to break loops on bad
    // metadata.
    let record = by_ref.get(&file_ref)?;
    let mut segments = vec![record.name.clone()];
    let mut cursor = record.parent_ref;
    for _ in 0..1024 {
        if cursor == 0 || cursor == record.file_ref {
            break;
        }
        let parent = match by_ref.get(&cursor) {
            Some(p) => p,
            None => break,
        };
        if (parent.attributes & 0x10) == 0 {
            // Non-directory parent ⇒ malformed.
            return None;
        }
        if parent.name.is_empty() || parent.parent_ref == cursor {
            break;
        }
        segments.push(parent.name.clone());
        cursor = parent.parent_ref;
    }
    let mut path = volume_root.to_path_buf();
    for s in segments.iter().rev() {
        path.push(s);
    }
    cache.insert(file_ref, path.clone());
    Some(path)
}

/// Strip Windows' `\\?\` verbatim prefix from a canonicalised path
/// so it compares cleanly against the `Drive:\…` style paths that
/// MFT enumeration produces. No-op on non-Windows shapes.
#[cfg(windows)]
fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut comps = p.components();
    if let Some(Component::Prefix(prefix)) = comps.next() {
        match prefix.kind() {
            Prefix::VerbatimDisk(letter) => {
                // `\\?\C:\foo` → `C:\foo`.
                let mut out = PathBuf::from(format!("{}:\\", letter as char));
                for c in comps {
                    if matches!(c, Component::RootDir) {
                        continue;
                    }
                    out.push(c.as_os_str());
                }
                return out;
            }
            Prefix::VerbatimUNC(_, _) | Prefix::Verbatim(_) => {
                // Less common shapes — at least drop the `\\?\`.
                let s = p.to_string_lossy();
                if let Some(rest) = s.strip_prefix(r"\\?\") {
                    return PathBuf::from(rest);
                }
            }
            _ => {}
        }
    }
    p.to_path_buf()
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn strip_verbatim_prefix(p: &Path) -> PathBuf {
    p.to_path_buf()
}

#[allow(dead_code)]
fn under_any_root(path: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return true;
    }
    for r in roots {
        if path.starts_with(r) {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
fn path_passes_globs(path: &Path, cfg: &ScanConfig) -> bool {
    if let Some(inc) = &cfg.include {
        if !inc.is_match(path) {
            return false;
        }
    }
    if let Some(exc) = &cfg.exclude {
        if exc.is_match(path) {
            return false;
        }
    }
    true
}
