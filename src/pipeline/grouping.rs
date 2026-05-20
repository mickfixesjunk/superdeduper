//! Stage 2 — group files by size.
//!
//! This stage is trivial CPU-bound work; no I/O. The only subtlety is the
//! zero-byte handling per the spec: zero-byte files form *one* group if
//! there are several, otherwise they're dropped like any singleton.

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
