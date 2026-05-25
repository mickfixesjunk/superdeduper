//! Windows platform impls.
//!
//! For tonight's L0 work we delegate to the existing Windows code
//! that already lives in `crate::winapi_wrappers` + `crate::dedupe`
//! + the existing per-call cfg-gated functions. This file is the
//! routing layer; no behaviour changes on Windows. A future
//! refactor can migrate the actual Win32 wrappers into here so the
//! `platform::` boundary becomes the SINGLE place Windows-specific
//! code lives.

use std::path::Path;

use super::{PlatformError, PlatformResult};

/// ReFS block-clone via FSCTL_DUPLICATE_EXTENTS_TO_FILE. Delegates
/// to the existing implementation in winapi_wrappers.
pub fn clone_file(src: &Path, dst: &Path) -> PlatformResult<()> {
    crate::winapi_wrappers::replace_with_reflink(dst, src)
        .map_err(|e| PlatformError::Other(format!("{e}")))
}

/// IFileOperation-based recycle via the existing winapi_wrappers
/// implementation (T2.3 upgrade from SHFileOperationW).
pub fn trash_file(path: &Path) -> PlatformResult<()> {
    crate::winapi_wrappers::recycle(path).map_err(|e| PlatformError::Other(format!("{e}")))
}

/// ShellExecuteW("open", url) — avoids `cmd /c start` mangling URLs
/// that contain `&` (the G1 captcha bug Mick hit on the first
/// browser-open code path).
pub fn open_url(url: &str) -> PlatformResult<()> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let op_w: Vec<u16> = "open\0".encode_utf16().collect();
    let url_w: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    let h = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(op_w.as_ptr()),
            PCWSTR(url_w.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    let raw = h.0 as isize;
    if raw > 32 {
        Ok(())
    } else {
        Err(PlatformError::Other(format!(
            "ShellExecuteW returned {raw} (<=32 indicates SE_ERR_*)"
        )))
    }
}
