# Walker fast path — design spec (Block N implementation plan)

> **Status:** spec for next engine session.
> Engine-side implementation deferred — the existing walker is correct
> and the win from this change is incremental (Knight Rider bench
> already 1.83x faster than cz without it). Worth doing right.

---

## The opportunity

Currently `src/inventory/walk.rs::walk()` uses `std::fs::read_dir`,
which on Windows calls `FindFirstFileW` + `FindNextFileW`. For each
returned entry we then call `entry.metadata()` (returns cached
`WIN32_FIND_DATA` — no extra syscall) and push a FileEntry. After the
walk, `pipeline::grouping::resolve_file_ids` runs an EXTRA pass
(stage 2b) using `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`
to populate the `file_ref` field for hardlink detection.

**The duplication:** `FileIdBothDirectoryInfo` (already used by
`src/inventory/dir_enum.rs`) returns a single batched buffer that
includes everything `FindFirstFile*W` returns PLUS the NTFS file ID
(inode). So we could land all the metadata + the inode info in ONE
pass through the directory. Stage 2b becomes unnecessary for the
walker path.

## Expected perf win

* Eliminates Stage 2b's per-directory `CreateFile + Get*ByHandleEx`
  pass entirely on the walker path.
* Reduces walker syscalls per directory from `1 (read_dir) + N
  (file metadata for stages with file_ref needed)` to `1 (open dir) +
  ~ceil(total_entry_bytes / 64KiB) batched calls`.
* On Knight Rider (22k small files, 1 deep tree level): probably
  10–30% wall improvement.
* On hardlink-heavy corpora (System32 ↔ WinSxS): potentially larger
  improvement because Stage 2b currently has to touch every file
  that survived size grouping.

## Implementation outline

### 1. New function in `src/inventory/dir_enum.rs`

```rust
/// Full-info equivalent of `enumerate_dir` — returns a Vec of entries
/// with name + size + attributes + file_id + parent_ref + mtime, in
/// the order Windows returned them. Lets the walker build FileEntry
/// directly without a separate metadata pass.
pub struct DirEntryFull {
    pub name: OsString,
    pub size: u64,
    pub attributes: u32,
    pub file_id: u64,         // inode
    pub mtime_filetime: i64,  // 100ns ticks since 1601
    pub is_dir: bool,
}

pub struct DirEnumeration {
    pub volume_guid: Option<String>,
    pub entries: Vec<DirEntryFull>,
}

pub fn enumerate_dir_full(dir: &Path) -> Option<DirEnumeration> {
    // Same shape as enumerate_dir, but pushes a DirEntryFull per
    // entry instead of just (name → inode).
}
```

### 2. Walker uses the new function

In `walk.rs::walk()`, replace `fs::read_dir(dir)` with
`enumerate_dir_full(dir)`. On Windows, take the fast path; fall back
to `fs::read_dir` if `enumerate_dir_full` returns None.

```rust
#[cfg(windows)]
let entries = match dir_enum::enumerate_dir_full(dir) {
    Some(e) => e,
    None => return walk_fallback(dir, cfg, out, callback, depth, cancel),
};
#[cfg(not(windows))]
let entries = walk_fallback(...);  // existing read_dir path

for entry in entries.entries {
    // Build FileEntry with file_ref already populated, no second
    // metadata() syscall needed.
    ...
}
```

### 3. Skip stage 2b for files from the fast path

`pipeline::grouping::resolve_file_ids` currently re-fetches file_ref
for every file in size groups. With the walker fast path,
`FileEntry.file_ref` is already non-zero AND `FileEntry.volume_guid`
is already populated. So `resolve_file_ids` should short-circuit
when both are already non-default.

```rust
pub fn resolve_file_ids(groups: &mut [SizeGroup]) {
    for group in groups {
        if group.files.iter().all(|f| f.file_ref != 0 && f.volume_guid.is_some()) {
            // Walker fast path already populated; skip.
            continue;
        }
        // Existing slow-path logic.
    }
}
```

## Risks / edge cases

1. **`FILE_ID_BOTH_DIR_INFO` may not work on non-NTFS volumes.**
   FAT32 / exFAT / network shares may return errors or wrong inode
   values. Must fall through to `fs::read_dir` on failure.
2. **Buffer size matters.** dir_enum uses 64 KiB; that's enough for
   most directories. For directories with 10k+ entries we'd loop;
   make sure that's wired correctly.
3. **Mtime format.** `FILE_ID_BOTH_DIR_INFO.LastWriteTime` is a
   LARGE_INTEGER 100ns-FILETIME — matches what walker currently uses,
   so this should be direct.
4. **Filename encoding.** Windows returns UTF-16LE; need to handle
   non-BMP characters (surrogate pairs) and decode safely. Already
   handled in dir_enum for the name-only case; replicate.

## Testing

Add a unit test that compares the output of:
1. `walker_with_fast_path` (Block N implementation)
2. `walker_with_fallback` (current `fs::read_dir` path)

Against the same temporary directory containing a known set of
files (a few sub-MB regular files, a hardlink pair, a small symlink).
Both must produce equivalent FileEntry vecs (file_ref + volume_guid
populated; size + attrs match; names match).

Bench-validate on Knight Rider corpus post-implementation. Expected
sd wall drops from 911 ms to ~700-800 ms; further improvement when
combined with the IO scheduling work in Block O.

## What this DOESN'T do

* Doesn't help on non-NTFS (which falls through to the existing
  read_dir path).
* Doesn't help the MFT path (which already uses
  `FSCTL_ENUM_USN_DATA`).
* Doesn't change Tier 3 IO (that's Block O — IO scheduling).
* Doesn't add long-path support beyond what's already there.

## Sequence with Block O

Block O (Tier 3 IO scheduling) is independent of Block N. Land in
either order. Both bench-verify against current v4 baseline (r8)
and Dropbox-class data (dropbox-r1).
