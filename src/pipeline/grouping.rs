//! Stage 2 — group files by size.
//!
//! This stage is trivial CPU-bound work; no I/O. The only subtlety is the
//! zero-byte handling per the spec: zero-byte files form *one* group if
//! there are several, otherwise they're dropped like any singleton.
//!
//! After grouping, `resolve_file_ids` performs per-file inode id
//! resolution via Win32 — but only for files that survived size
//! grouping (i.e. shared a size with at least one other file). Files
//! with unique sizes can never be hardlinks of each other in this
//! scan and don't need an inode id. On a realistic corpus where most
//! files have unique sizes that's a 70-90 % reduction in
//! `GetFileInformationByHandle` calls compared to resolving during
//! the walk.

use hashbrown::HashMap;

use crate::inventory::FileEntry;

/// A bucket of files that all share the same size.
#[derive(Debug)]
pub struct SizeGroup {
    pub size: u64,
    pub files: Vec<FileEntry>,
}

/// Group `files` by size, dropping any class with fewer than two members.
pub fn group_by_size(files: Vec<FileEntry>) -> Vec<SizeGroup> {
    let mut by_size: HashMap<u64, Vec<FileEntry>> = HashMap::new();
    for f in files {
        by_size.entry(f.size).or_default().push(f);
    }
    by_size
        .into_iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(size, files)| SizeGroup { size, files })
        .collect()
}

/// Resolve NTFS file-id + volume-serial for every entry in every
/// size group, in parallel. Two stages:
///
/// 1. **Fast path**: bucket the files-to-resolve by parent directory
///    and open each unique parent once, calling
///    `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` to get
///    every child's inode id in a single batch. For corpora with
///    many files per directory this collapses N per-file CreateFile
///    syscalls into roughly 1 per directory.
/// 2. **Slow path fallback**: for any entry the batched dir-enum
///    didn't fill (dir open denied, file not in the dir enum result,
///    non-NTFS volume), fall back to per-file
///    `walk::file_id_for`.
///
/// The point of doing this AFTER size grouping rather than during
/// enumeration: files with a unique size never end up in a multi-file
/// group, so they can't be hardlinks of each other within this scan.
/// Resolving their inode ids would be a wasted syscall.
///
/// Silent on per-file failure — entries stay at sentinel values
/// (`file_ref=0`, `volume_guid=None`) and Stage-4's link-equivalent
/// check correctly declines to flag them as hardlinks.
#[cfg(windows)]
pub fn resolve_file_ids(groups: &mut [SizeGroup]) {
    use crate::inventory::dir_enum;
    use hashbrown::HashMap;
    use rayon::prelude::*;
    use std::ffi::OsString;
    use std::path::PathBuf;

    // Step 1: collect unique parent directories across all files
    // that still need resolution.
    let mut parent_set: HashMap<PathBuf, ()> = HashMap::new();
    for g in groups.iter() {
        for f in g.files.iter() {
            if f.file_ref != 0 && f.volume_guid.is_some() {
                continue;
            }
            if let Some(parent) = f.path.parent() {
                parent_set.insert(parent.to_path_buf(), ());
            }
        }
    }
    let parents: Vec<PathBuf> = parent_set.into_keys().collect();

    // Step 2: enumerate each unique parent dir in parallel, building
    // a single combined (dir, basename) → file_id map. Each unique
    // dir opens its handle exactly once.
    let dir_results: Vec<(PathBuf, dir_enum::DirInodeMap)> = parents
        .par_iter()
        .filter_map(|p| dir_enum::enumerate_dir(p).map(|m| (p.clone(), m)))
        .collect();
    let mut combined: HashMap<PathBuf, dir_enum::DirInodeMap> = HashMap::new();
    for (p, m) in dir_results {
        combined.insert(p, m);
    }

    // Step 3: fill in file_ref / volume_guid using the dir-enum map
    // first, then fall back to per-file for anything missing.
    groups.par_iter_mut().for_each(|g| {
        for f in g.files.iter_mut() {
            if f.file_ref != 0 && f.volume_guid.is_some() {
                continue;
            }
            // Fast path: look up via the directory-enum cache.
            let parent = f.path.parent();
            let basename = f.path.file_name().map(OsString::from);
            if let (Some(parent), Some(basename)) = (parent, basename) {
                if let Some(dir_map) = combined.get(parent) {
                    if let Some(&id) = dir_map.by_name.get(&basename) {
                        f.file_ref = id;
                        if dir_map.volume_guid.is_some() {
                            f.volume_guid = dir_map.volume_guid.clone();
                        }
                        if f.file_ref != 0 && f.volume_guid.is_some() {
                            continue;
                        }
                    }
                }
            }
            // Slow path: per-file CreateFile + GetFileInformationByHandle.
            if let Some((id, vol)) = crate::inventory::walk::file_id_for(&f.path) {
                if f.file_ref == 0 {
                    f.file_ref = id;
                }
                if f.volume_guid.is_none() {
                    f.volume_guid = vol;
                }
            }
        }
    });
}

#[cfg(not(windows))]
pub fn resolve_file_ids(_groups: &mut [SizeGroup]) {
    // Non-Windows: no inode-id concept matching NTFS file_ref; the
    // fallback walker leaves fields at their defaults and that's fine
    // — Stage 4's hardlink check requires `volume_guid.is_some()` so
    // it correctly declines to flag anything.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn entry(path: &str, size: u64) -> FileEntry {
        FileEntry {
            path: PathBuf::from(path),
            size,
            mtime: 0,
            file_ref: 0,
            parent_ref: 0,
            usn: 0,
            attributes: 0,
            volume_guid: None,
            placeholder: crate::inventory::PlaceholderState::NotPlaceholder,
        }
    }

    #[test]
    fn singletons_dropped() {
        let groups = group_by_size(vec![entry("a", 10), entry("b", 20), entry("c", 10)]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].size, 10);
        assert_eq!(groups[0].files.len(), 2);
    }

    #[test]
    fn zero_byte_grouped_if_multiple() {
        let groups = group_by_size(vec![entry("a", 0), entry("b", 0), entry("c", 0)]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].size, 0);
        assert_eq!(groups[0].files.len(), 3);
    }

    #[test]
    fn empty_input_yields_no_groups() {
        let groups = group_by_size(Vec::new());
        assert!(groups.is_empty());
    }
}
