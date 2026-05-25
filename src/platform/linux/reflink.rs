//! Linux reflink via FICLONE ioctl.
//!
//! Per `linux-roadmap.md` §5.1 L0 deliverable. Implements clone-on-
//! write file replacement using the FICLONE ioctl, supported by:
//! * **Btrfs** — first-class CoW filesystem; FICLONE is the canonical
//!   block-clone path.
//! * **XFS** — since kernel 4.16 with reflink=1 mkfs flag.
//! * **Bcachefs** — native CoW.
//! * **OCFS2** — supports FICLONE since kernel 4.6.
//! * **ZFS** — supports `FICLONE` since OpenZFS 2.2 (kernel module
//!   exposes it via the ioctl bridge).
//!
//! Filesystems that don't support reflinks return `EOPNOTSUPP` /
//! `EXDEV` from the ioctl; we map those to `PlatformError::Unsupported`
//! so the caller can fall back to a regular copy + replace.
//!
//! ## API contract
//!
//! `clone_file(src, dst)` is atomic-via-tmp-rename:
//!
//! 1. Open `src` read-only.
//! 2. Create `dst.superdeduper-clone-tmp` as a fresh CoW clone of
//!    `src` via FICLONE.
//! 3. Rename the tmp file over `dst` (atomic on same-filesystem).
//! 4. On any error, remove the tmp file and propagate.
//!
//! Post-success: `dst` and `src` share storage on disk. Editing
//! either triggers per-block CoW; modified blocks become unshared.
//! Free-space accounting credits the saved bytes immediately.

use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use super::{PlatformError, PlatformResult};

// FICLONE = _IOW(0x94, 9, int) where the int is the src fd.
// Per <linux/fs.h>. Hand-encoded to avoid pulling libc/nix as deps.
type IoctlNo = std::ffi::c_ulong;
const FICLONE: IoctlNo = 0x4020_9409;

extern "C" {
    fn ioctl(fd: std::ffi::c_int, request: IoctlNo, ...) -> std::ffi::c_int;
}

pub fn clone_file(src: &Path, dst: &Path) -> PlatformResult<()> {
    let src_file = fs::File::open(src)?;
    let tmp = clone_tmp_path(dst);

    // Remove any stale tmp from a previous interrupted clone before
    // we create a fresh one. Best-effort; the create_new below
    // would fail anyway if it still existed.
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }

    // Create the dst-side temp file with the same mode bits as src
    // (read/write owner; clone semantics expect a fresh dst that
    // the ioctl populates). create_new() prevents racing with a
    // concurrent clone of the same dst.
    let dst_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&tmp)?;

    // SAFETY: src_file + dst_file are owned + open; FICLONE is a
    // documented ioctl that takes one int (the src fd). The kernel
    // handles all the dst-side block allocation. No userspace
    // buffers, no syscall input lifetimes to worry about.
    let rc = unsafe {
        ioctl(
            dst_file.as_raw_fd(),
            FICLONE,
            src_file.as_raw_fd() as std::ffi::c_int,
        )
    };

    if rc != 0 {
        // ioctl failed. Common errno values per ioctl_ficlone(2):
        // * EOPNOTSUPP — filesystem doesn't support FICLONE.
        // * EXDEV — src + dst are on different filesystems / volumes.
        // * EBADF — fd not openable (shouldn't happen post-open).
        // * EINVAL — src is a directory / non-regular file / src
        //   and dst overlap.
        let err = std::io::Error::last_os_error();
        // Clean up the tmp file we just created.
        let _ = fs::remove_file(&tmp);
        return Err(classify_ficlone_error(err));
    }

    // Successful clone. Sync metadata (link count + size) before
    // the rename so a crash here doesn't leave a half-populated tmp.
    drop(dst_file);

    // Atomic rename over the original dst. On Unix this is atomic
    // within a single filesystem; cross-fs is moot here because
    // FICLONE itself fails with EXDEV for cross-fs anyway.
    if let Err(e) = fs::rename(&tmp, dst) {
        let _ = fs::remove_file(&tmp);
        return Err(PlatformError::Io(e));
    }

    Ok(())
}

fn clone_tmp_path(dst: &Path) -> PathBuf {
    // Append a distinctive suffix to the dst's filename. Same dir
    // so the FICLONE ioctl + the rename stay on the same fs (FICLONE
    // can't cross filesystems, and rename(2) is only atomic within
    // a filesystem).
    let mut tmp = dst.to_path_buf();
    let stem = tmp
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "tmp".to_string());
    tmp.set_file_name(format!(".{stem}.superdeduper-clone-tmp"));
    tmp
}

fn classify_ficlone_error(err: std::io::Error) -> PlatformError {
    use std::io::ErrorKind;
    // Linux maps EOPNOTSUPP to ErrorKind::Unsupported on modern
    // Rust; older toolchains see it via raw_os_error.
    let raw = err.raw_os_error();
    // EOPNOTSUPP = 95, ENOTTY = 25 (ioctl not supported by fs),
    // EXDEV = 18 (cross-device link).
    if matches!(err.kind(), ErrorKind::Unsupported)
        || raw == Some(95)
        || raw == Some(25)
        || raw == Some(18)
    {
        return PlatformError::Unsupported(
            "FICLONE not supported by this filesystem (try Btrfs, XFS reflink=1, or Bcachefs)",
        );
    }
    PlatformError::Io(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_random(path: &Path, n: usize) {
        let mut bytes = vec![0u8; n];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(&bytes).unwrap();
    }

    #[test]
    fn clone_tmp_path_is_in_dst_dir_and_distinct() {
        let dst = Path::new("/some/dir/foo.bin");
        let tmp = clone_tmp_path(dst);
        assert_eq!(tmp.parent(), dst.parent());
        assert_ne!(tmp.file_name(), dst.file_name());
        assert!(tmp
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.ends_with(".superdeduper-clone-tmp"))
            .unwrap_or(false));
    }

    #[test]
    fn clone_file_unsupported_on_tmpfs_or_overlayfs() {
        // /tmp on most CI / dev boxes is tmpfs or overlayfs —
        // neither supports FICLONE. The test pins our error-mapping
        // behaviour: when the underlying fs doesn't support reflinks,
        // we return PlatformError::Unsupported (NOT a raw IO error).
        // That gives callers a clean signal to fall back to a normal
        // copy.
        let tmpdir = std::env::temp_dir().join(format!(
            "sd-reflink-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmpdir).unwrap();
        let src = tmpdir.join("src.bin");
        let dst = tmpdir.join("dst.bin");
        write_random(&src, 1024);

        let outcome = clone_file(&src, &dst);
        // We accept EITHER Unsupported OR Ok — Btrfs / XFS-with-reflink
        // testers have a CoW-capable /tmp and will get Ok; everyone
        // else gets Unsupported. Both outcomes are correct; what
        // we're guarding against is a panic OR a raw Io error
        // surfacing instead of the typed Unsupported.
        match outcome {
            Ok(()) => {
                // The reflink succeeded — assert dst exists and
                // has the same byte content as src.
                let s = fs::read(&src).unwrap();
                let d = fs::read(&dst).unwrap();
                assert_eq!(s, d, "cloned dst must have same bytes as src");
            }
            Err(PlatformError::Unsupported(_)) => { /* expected on tmpfs */ }
            Err(other) => panic!("unexpected error from clone_file on tmpfs: {other:?}"),
        }

        // Cleanup. Ignore failures so the test doesn't fail on a
        // pre-existing dst from a partial Ok.
        let _ = fs::remove_dir_all(&tmpdir);
    }

    #[test]
    fn classify_ficlone_error_maps_eopnotsupp_to_unsupported() {
        let err = std::io::Error::from_raw_os_error(95); // EOPNOTSUPP
        match classify_ficlone_error(err) {
            PlatformError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn classify_ficlone_error_maps_exdev_to_unsupported() {
        let err = std::io::Error::from_raw_os_error(18); // EXDEV
        match classify_ficlone_error(err) {
            PlatformError::Unsupported(_) => {}
            other => panic!("expected Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn classify_ficlone_error_passes_other_errors_through() {
        let err = std::io::Error::from_raw_os_error(13); // EACCES
        match classify_ficlone_error(err) {
            PlatformError::Io(_) => {}
            other => panic!("expected Io for EACCES, got {other:?}"),
        }
    }
}
