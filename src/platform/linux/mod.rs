//! Linux platform impls.
//!
//! Per-OS file for L0 Linux work. Each public fn here matches a
//! public fn in `src/platform/mod.rs`; cfg-routing happens in mod.rs,
//! impls land in submodules here.

pub mod mount_info;
pub mod reflink;
pub mod trash;

use super::{PlatformError, PlatformResult};

/// Open a URL in the user's default browser. Uses `xdg-open` —
/// the freedesktop.org convention. `xdg-open` itself dispatches to
/// the user's preferred handler (GNOME `gio open`, KDE `kde-open5`,
/// `gvfs-open`, etc.), so this works across desktops without us
/// having to detect the DE.
///
/// Non-blocking: spawns + returns immediately. The browser process
/// outlives our handle, so we don't wait + we don't capture its
/// exit code. Failure to even spawn (e.g. `xdg-open` missing from
/// PATH on a minimal headless install) surfaces as `Io`.
pub fn open_url(url: &str) -> PlatformResult<()> {
    use std::process::{Command, Stdio};
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| {
            PlatformError::Other(format!(
                "xdg-open failed: {e}. Install xdg-utils or use a desktop session that provides it."
            ))
        })
}

#[cfg(test)]
mod tests {
    // open_url can't be tested in CI without a desktop session;
    // covered by manual QA. The reflink + trash submodules carry
    // their own tests against real temp dirs.
}
