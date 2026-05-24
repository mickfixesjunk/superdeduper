//! macOS platform impls. L3 territory per the Linux roadmap; stub
//! impls that return Unsupported so the platform module compiles
//! cleanly on macOS hosts during the L0/L1 phases.

use std::path::Path;

use super::{PlatformError, PlatformResult};

pub fn clone_file(_src: &Path, _dst: &Path) -> PlatformResult<()> {
    // L3: implement via clonefile(2) (APFS).
    Err(PlatformError::Unsupported(
        "macOS clonefile() not yet implemented (L3 roadmap)",
    ))
}

pub fn trash_file(_path: &Path) -> PlatformResult<()> {
    // L3: implement via NSFileManager trashItemAtURL:resultingItemURL:error:.
    Err(PlatformError::Unsupported(
        "macOS trash not yet implemented (L3 roadmap)",
    ))
}

pub fn open_url(url: &str) -> PlatformResult<()> {
    use std::process::{Command, Stdio};
    Command::new("open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| PlatformError::Other(format!("`open` failed: {e}")))
}
