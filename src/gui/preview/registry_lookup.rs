//! Windows-only: resolve an extension → registered
//! `IPreviewHandler` CLSID via the registry.
//!
//! ## Lookup path
//!
//! Per the IPreviewHandler shell-extension docs:
//!
//! ```text
//! HKEY_CLASSES_ROOT\<ext>\shellex\{8895b1c6-b41f-4c1c-a562-0d564250836f}
//! ```
//!
//! The default value of that key is the handler's CLSID (a
//! braced GUID like `{F0F2DBC8-E2A8-4D31-B43C-A1F0A4B26C8A}`).
//! Some extensions delegate to a class name (e.g. `txtfile`) —
//! we follow that one level via `HKCR\<ext>\(default)` →
//! `HKCR\<class>\shellex\{…}` before giving up.
//!
//! ## What this module is for
//!
//! v1 returns the CLSID string; the actual COM hosting (the
//! `CoCreateInstance(clsid, IID_IPreviewHandler)` +
//! `IInitializeWithFile` → `SetWindow` → `DoPreview` flow) is
//! deferred to a Windows-side iteration session. Today the GUI
//! uses the CLSID purely as a signal that a handler exists.

#![cfg(windows)]

use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CLASSES_ROOT, KEY_READ, REG_VALUE_TYPE,
};

const IPREVIEW_HANDLER_SHELLEX_GUID: &str = "{8895b1c6-b41f-4c1c-a562-0d564250836f}";

/// Returns the CLSID (as a braced GUID string) of the registered
/// `IPreviewHandler` for the given lowercase extension, or `None`
/// when no handler is registered (or any registry read fails).
pub fn handler_clsid(extension: &str) -> Option<String> {
    if extension.is_empty() {
        return None;
    }
    let ext_with_dot = format!(".{extension}");

    // Direct lookup: HKCR\.ext\shellex\{guid}.
    if let Some(clsid) = read_default(&format!(
        "{ext_with_dot}\\shellex\\{IPREVIEW_HANDLER_SHELLEX_GUID}"
    )) {
        return Some(clsid);
    }

    // Class-name redirection: HKCR\.ext\(default) → HKCR\<class>\shellex\{guid}.
    if let Some(class_name) = read_default(&ext_with_dot) {
        if let Some(clsid) = read_default(&format!(
            "{class_name}\\shellex\\{IPREVIEW_HANDLER_SHELLEX_GUID}"
        )) {
            return Some(clsid);
        }
    }

    None
}

/// Read the default (unnamed) string value of an HKEY_CLASSES_ROOT
/// subkey. Returns `None` on any error or when the value is missing
/// / empty / not a string.
fn read_default(subkey: &str) -> Option<String> {
    // Wide-encode the subkey path (null-terminated).
    let wide: Vec<u16> = subkey.encode_utf16().chain(std::iter::once(0)).collect();
    let mut hkey = HKEY::default();
    let open_status = unsafe {
        RegOpenKeyExW(
            HKEY_CLASSES_ROOT,
            PCWSTR(wide.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if open_status != ERROR_SUCCESS {
        return None;
    }

    // Two-pass query: first call asks for the buffer size.
    let mut data_type: REG_VALUE_TYPE = REG_VALUE_TYPE::default();
    let mut data_size: u32 = 0;
    let null_value_name = PCWSTR(std::ptr::null());
    let size_status = unsafe {
        RegQueryValueExW(
            hkey,
            null_value_name,
            None,
            Some(&mut data_type),
            None,
            Some(&mut data_size),
        )
    };
    if size_status != ERROR_SUCCESS || data_size == 0 {
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        return None;
    }

    let mut buf = vec![0u8; data_size as usize];
    let read_status = unsafe {
        RegQueryValueExW(
            hkey,
            null_value_name,
            None,
            Some(&mut data_type),
            Some(buf.as_mut_ptr()),
            Some(&mut data_size),
        )
    };
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    if read_status != ERROR_SUCCESS {
        return None;
    }

    // Convert the wide-string bytes back to UTF-8. Strip the
    // trailing null terminator if present.
    let wide: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&c| c != 0)
        .collect();
    let s = String::from_utf16(&wide).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
