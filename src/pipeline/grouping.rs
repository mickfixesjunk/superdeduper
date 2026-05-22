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
/// size group, in parallel. Entries whose enumeration didn't already
/// fill these (the typical case — see `inventory::walk` which leaves
/// the fields at their default `0`/`None`) get a per-file
/// `GetFileInformationByHandle` here.
///
/// The point of doing this AFTER size grouping rather than during
/// enumeration: files with a unique size never end up in a multi-file
/// group, so they can't be hardlinks of each other within this scan.
/// Resolving their inode ids would be a wasted syscall. On a typical
/// corpus where most files have unique sizes that's the bulk of the
/// files in the run.
///
/// Failure to resolve (file locked, deleted between walk and now,
/// non-NTFS volume) is silent — the entry stays at the default
/// sentinel values, and Stage 4's link-equivalent check correctly
/// declines to flag it as a hardlink.
#[cfg(windows)]
pub fn resolve_file_ids(groups: &mut [SizeGroup]) {
    use rayon::prelude::*;
    groups.par_iter_mut().for_each(|g| {
        for f in g.files.iter_mut() {
            if f.file_ref != 0 && f.volume_guid.is_some() {
                // Already populated (e.g. by the MFT-enum inventory
                // path which fills both fields). Skip the redundant
                // syscall.
                continue;
            }
            if let Some((id, vol)) = crate::inventory::walk::file_id_for(&f.path) {
                f.file_ref = id;
                f.volume_guid = vol;
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
