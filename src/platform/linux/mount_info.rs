//! #15 L2 — Per-path mount introspection for Linux.
//!
//! Parses `/proc/self/mounts` and resolves a scan-root path back to
//! the mount entry that hosts it (longest-prefix match). Surfaces
//! filesystem type, mount options, source device, and a handful of
//! derived flags the GUI / CLI use to warn the user about
//! non-obvious dedupe-impacting setups:
//!
//!   * **ZFS with `dedup=on`** — pool-level dedup makes our
//!     reclaim numbers a lie (data is already shared at the block
//!     layer). Caveat the user.
//!   * **LUKS / dm-crypt encrypted volumes** — recycle + reflink
//!     still work, but the "I deleted X bytes" reclaim is the
//!     plaintext view, not the ciphertext disk savings (which
//!     equal plaintext on LUKS since it's full-block encryption).
//!     Just an FYI; no functional gotcha.
//!   * **Network mounts** (cifs/smb/nfs/sshfs/fuse-remote) —
//!     throughput is capped by the network link, not the SSD.
//!     Surface the type so the user sees why the scan is slow.
//!   * **Reflink-capable filesystems** (btrfs, xfs, bcachefs,
//!     zfs ≥ 2.2, ocfs2) — hint at the `--action reflink` flag
//!     for instant zero-cost dedup.
//!
//! Lookup is a single `/proc/self/mounts` read + linear scan. Cost
//! is O(num_mounts) per call; typical Linux box has 30-50 mounts so
//! that's microseconds. Cached at the call site if we ever care
//! about the cost.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};

/// Resolved mount entry for one scan-root path. Mirrors the columns
/// of `/proc/self/mounts` plus derived flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountInfo {
    /// Mount source — `/dev/sda3`, `tmpfs`, `192.168.1.10:/home`,
    /// `/dev/mapper/luks-…`, etc. The literal first column.
    pub source: String,
    /// Mount point — `/`, `/home`, `/mnt/backup`, etc. Used for
    /// longest-prefix matching against scan-root paths.
    pub mountpoint: PathBuf,
    /// Filesystem type — `ext4`, `btrfs`, `xfs`, `zfs`, `cifs`,
    /// `nfs4`, `fuse.sshfs`, `overlay`, etc. The third column.
    pub fs_type: String,
    /// Comma-separated mount-option list — `rw,relatime,errors=remount-ro`
    /// etc. Parsed downstream into flag predicates.
    pub options: String,
    /// `true` when the source path lives under `/dev/mapper/` —
    /// typical for LUKS / dm-crypt volumes opened via cryptsetup.
    /// False positives possible (LVM also uses `/dev/mapper/`) so
    /// downstream code treats this as a HINT, not authoritative.
    pub is_dm_mapped: bool,
    /// `true` when the filesystem type is one we recognise as a
    /// network protocol — surface a "this is a network mount"
    /// warning so the user knows why throughput is limited.
    pub is_network: bool,
    /// `true` when the filesystem natively supports the `FICLONE`
    /// ioctl (or its `BTRFS_IOC_CLONE` predecessor). Hint at
    /// `--action reflink` when set.
    pub supports_reflink: bool,
    /// `true` when the filesystem MIGHT have pool-level dedup
    /// turned on (we can't tell without a privileged call to
    /// `zpool get dedup`; surface the warning generically and let
    /// the user decide).
    pub may_have_pool_dedup: bool,
}

impl MountInfo {
    /// User-facing single-line description of the mount's
    /// relevance to a scan. Shown next to the scan-root in the
    /// GUI Roots panel + on the CLI scan-start banner.
    pub fn summary_line(&self) -> String {
        let mut bits: Vec<String> = Vec::new();
        bits.push(self.fs_type.clone());
        if self.is_network {
            bits.push("network".to_string());
        }
        if self.is_dm_mapped {
            bits.push("dm-mapped (LUKS?)".to_string());
        }
        if self.supports_reflink {
            bits.push("reflink".to_string());
        }
        if self.may_have_pool_dedup {
            bits.push("pool-dedup-capable".to_string());
        }
        format!("{} · {}", self.mountpoint.display(), bits.join(" · "))
    }

    /// Human-readable warnings (zero or more) about dedupe pitfalls
    /// implied by this mount type. Used by the GUI to surface
    /// inline notes + by the CLI to log a one-line caveat per root.
    pub fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        if self.may_have_pool_dedup {
            out.push(format!(
                "{} is on {} which may have pool-level dedup enabled — \
                 reclaim numbers can over-report the actual disk savings \
                 (data may already be shared at the block layer).",
                self.mountpoint.display(),
                self.fs_type,
            ));
        }
        if self.is_network {
            out.push(format!(
                "{} is a network mount ({}) — hashing throughput is \
                 capped by the network link, not the local disk.",
                self.mountpoint.display(),
                self.fs_type,
            ));
        }
        if self.is_dm_mapped {
            out.push(format!(
                "{} is on a device-mapper volume ({}). Likely LUKS or LVM. \
                 Recycle + reflink still work; reclaim numbers reflect \
                 plaintext view (matches ciphertext disk savings on LUKS).",
                self.mountpoint.display(),
                self.source,
            ));
        }
        if self.supports_reflink {
            out.push(format!(
                "{} is on {} which supports reflinks — try \
                 `--action reflink` for instant zero-cost dedup.",
                self.mountpoint.display(),
                self.fs_type,
            ));
        }
        out
    }
}

/// Resolve `path` to its mount entry. Returns `None` if the path
/// doesn't exist, can't be canonicalised, or `/proc/self/mounts`
/// isn't readable. The lookup is best-effort: bare-metal Linux
/// always has `/proc` mounted, but a chroot / container might not.
pub fn for_path(path: &Path) -> Option<MountInfo> {
    // Canonicalise so we match against the realpath, not whatever
    // symlink the user typed. Falls back to the input if canonicalise
    // fails — bare-fs symlinks land where they should anyway.
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mounts = parse_mounts_file("/proc/self/mounts").ok()?;
    longest_prefix_match(&mounts, &canonical)
}

/// Read + parse `/proc/self/mounts`. Public so unit tests can run
/// against a fixture file rather than the live `/proc`. Each line
/// is `source mountpoint fs_type options dump pass`; we only need
/// the first four columns.
pub fn parse_mounts_file(path: &str) -> std::io::Result<Vec<MountInfo>> {
    let body = std::fs::read_to_string(path)?;
    Ok(parse_mounts_body(&body))
}

/// Parse mounts-file content. Empty lines + lines with fewer than
/// four columns are skipped. mountpoint values are
/// octal-escape-decoded (`\040` → space, `\011` → tab, `\134` → `\`)
/// because `/proc/self/mounts` uses that encoding for whitespace.
fn parse_mounts_body(body: &str) -> Vec<MountInfo> {
    body.lines().filter_map(parse_mount_line).collect()
}

fn parse_mount_line(line: &str) -> Option<MountInfo> {
    let mut parts = line.split_whitespace();
    let source = parts.next()?;
    let mountpoint_raw = parts.next()?;
    let fs_type = parts.next()?;
    let options = parts.next()?;
    let mountpoint = decode_octal(mountpoint_raw);
    let source = decode_octal(source);

    let is_network = matches!(
        fs_type,
        "cifs"
            | "smb3"
            | "smbfs"
            | "nfs"
            | "nfs4"
            | "afpfs"
            | "afs"
            | "ceph"
            | "9p"
            | "fuse.sshfs"
            | "fuse.gvfs"
            | "fuse.gvfsd-fuse"
            | "fuse.rclone"
            | "fuse.s3fs"
    );
    let is_dm_mapped = source.starts_with("/dev/mapper/");
    let supports_reflink = matches!(
        fs_type,
        // FICLONE-capable per the in-tree kernel filesystem drivers
        // + OpenZFS 2.2's clone_file_range support.
        "btrfs" | "xfs" | "bcachefs" | "zfs" | "ocfs2"
    );
    let may_have_pool_dedup = matches!(fs_type, "zfs" | "btrfs");

    Some(MountInfo {
        source,
        mountpoint: PathBuf::from(mountpoint),
        fs_type: fs_type.to_string(),
        options: options.to_string(),
        is_dm_mapped,
        is_network,
        supports_reflink,
        may_have_pool_dedup,
    })
}

/// `/proc/self/mounts` encodes whitespace + backslash in
/// mountpoints as octal escapes (e.g. `\040` for space). Decode
/// the four standard escapes the kernel emits; bytes outside that
/// set pass through.
fn decode_octal(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 3 < bytes.len() {
            let octal_str = std::str::from_utf8(&bytes[i + 1..i + 4]).unwrap_or("");
            if let Ok(v) = u32::from_str_radix(octal_str, 8) {
                if let Some(c) = char::from_u32(v) {
                    out.push(c);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Pick the mount entry whose mountpoint is the longest prefix of
/// `path`. `/` is always a prefix; ties (rare in practice — same
/// mountpoint repeated) resolve to the LAST one in the file order,
/// matching how the kernel resolves on the live system.
fn longest_prefix_match(mounts: &[MountInfo], path: &Path) -> Option<MountInfo> {
    let mut best: Option<&MountInfo> = None;
    let mut best_len: usize = 0;
    for m in mounts {
        let mp = &m.mountpoint;
        if path.starts_with(mp) {
            let len = mp.as_os_str().len();
            if len >= best_len {
                best_len = len;
                best = Some(m);
            }
        }
    }
    best.cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PROC_MOUNTS: &str = "\
/dev/sda3 / ext4 rw,relatime,errors=remount-ro 0 0
/dev/sda1 /boot/efi vfat rw,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev 0 0
/dev/mapper/cryptroot /home ext4 rw,relatime 0 0
/dev/sdb1 /mnt/btrfs btrfs rw,relatime,space_cache=v2 0 0
zpool0/dataset /mnt/zfs zfs rw,relatime,xattr,noacl 0 0
192.168.1.10:/exports /mnt/nas nfs4 rw,relatime,vers=4.1 0 0
//server/share /mnt/win cifs rw,relatime,vers=3.0 0 0
fuse.sshfs /mnt/remote fuse.sshfs rw,nosuid,nodev 0 0
/dev/sdc1 /mnt/with\\040space ext4 rw,relatime 0 0
";

    #[test]
    fn parses_proc_mounts_canonical_shape() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        assert_eq!(entries.len(), 10);
        assert_eq!(entries[0].source, "/dev/sda3");
        assert_eq!(entries[0].mountpoint, PathBuf::from("/"));
        assert_eq!(entries[0].fs_type, "ext4");
    }

    #[test]
    fn decodes_octal_escapes_in_mountpoint() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let space = entries
            .iter()
            .find(|m| m.mountpoint.to_string_lossy().contains("with"))
            .expect("with-space entry present");
        assert_eq!(space.mountpoint, PathBuf::from("/mnt/with space"));
    }

    #[test]
    fn flags_network_mounts() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let nfs = entries.iter().find(|m| m.fs_type == "nfs4").unwrap();
        assert!(nfs.is_network);
        let cifs = entries.iter().find(|m| m.fs_type == "cifs").unwrap();
        assert!(cifs.is_network);
        let sshfs = entries.iter().find(|m| m.fs_type == "fuse.sshfs").unwrap();
        assert!(sshfs.is_network);
        let ext4 = entries
            .iter()
            .find(|m| m.mountpoint == Path::new("/"))
            .unwrap();
        assert!(!ext4.is_network);
    }

    #[test]
    fn flags_dm_mapped() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let luks = entries
            .iter()
            .find(|m| m.source.starts_with("/dev/mapper/"))
            .unwrap();
        assert!(luks.is_dm_mapped);
        assert_eq!(luks.fs_type, "ext4");
    }

    #[test]
    fn flags_reflink_capable() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let btrfs = entries.iter().find(|m| m.fs_type == "btrfs").unwrap();
        assert!(btrfs.supports_reflink);
        let zfs = entries.iter().find(|m| m.fs_type == "zfs").unwrap();
        assert!(zfs.supports_reflink);
        let ext4 = entries
            .iter()
            .find(|m| m.fs_type == "ext4" && m.mountpoint == Path::new("/"))
            .unwrap();
        assert!(!ext4.supports_reflink);
    }

    #[test]
    fn flags_pool_dedup_capable() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let zfs = entries.iter().find(|m| m.fs_type == "zfs").unwrap();
        assert!(zfs.may_have_pool_dedup);
        let btrfs = entries.iter().find(|m| m.fs_type == "btrfs").unwrap();
        assert!(btrfs.may_have_pool_dedup);
        let ext4 = entries
            .iter()
            .find(|m| m.fs_type == "ext4" && m.mountpoint == Path::new("/"))
            .unwrap();
        assert!(!ext4.may_have_pool_dedup);
    }

    #[test]
    fn longest_prefix_match_finds_deepest_mount() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        // /home is on cryptroot; /home/anything should resolve there
        // rather than /.
        let m = longest_prefix_match(&entries, Path::new("/home/user/photos")).unwrap();
        assert_eq!(m.mountpoint, PathBuf::from("/home"));
        // /mnt/btrfs/subdir resolves to /mnt/btrfs, not /.
        let m = longest_prefix_match(&entries, Path::new("/mnt/btrfs/snap")).unwrap();
        assert_eq!(m.fs_type, "btrfs");
    }

    #[test]
    fn longest_prefix_match_falls_back_to_root() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let m = longest_prefix_match(&entries, Path::new("/etc/passwd")).unwrap();
        assert_eq!(m.mountpoint, PathBuf::from("/"));
    }

    #[test]
    fn warnings_zfs_includes_pool_dedup_caveat() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let zfs = entries.iter().find(|m| m.fs_type == "zfs").unwrap();
        let warnings = zfs.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("pool-level dedup")),
            "zfs warnings should mention pool-dedup: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("reflink")),
            "zfs warnings should hint at reflink action: {warnings:?}"
        );
    }

    #[test]
    fn warnings_network_mount_includes_throughput_note() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let nfs = entries.iter().find(|m| m.fs_type == "nfs4").unwrap();
        let warnings = nfs.warnings();
        assert!(
            warnings.iter().any(|w| w.contains("network")),
            "nfs warnings should mention network: {warnings:?}"
        );
    }

    #[test]
    fn warnings_dm_mapped_mentions_luks() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let luks = entries.iter().find(|m| m.is_dm_mapped).unwrap();
        let warnings = luks.warnings();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("LUKS") || w.contains("LVM")),
            "dm-mapped warnings should mention LUKS/LVM: {warnings:?}"
        );
    }

    #[test]
    fn summary_line_contains_fs_type_and_mountpoint() {
        let entries = parse_mounts_body(SAMPLE_PROC_MOUNTS);
        let zfs = entries.iter().find(|m| m.fs_type == "zfs").unwrap();
        let s = zfs.summary_line();
        assert!(s.contains("zfs"), "summary should contain fs_type: {s}");
        assert!(
            s.contains("/mnt/zfs"),
            "summary should contain mountpoint: {s}"
        );
        assert!(
            s.contains("reflink"),
            "zfs summary should advertise reflink capability: {s}"
        );
        assert!(
            s.contains("pool-dedup"),
            "zfs summary should advertise pool-dedup capability: {s}"
        );
    }
}
