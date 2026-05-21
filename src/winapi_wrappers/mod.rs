//! Safe wrappers over the Win32 / NT APIs we need for the scan pipeline.
//!
//! Every `unsafe` block in `superdeduper` lives in this module. Each `unsafe`
//! block carries a `// SAFETY:` comment explaining the FFI contract it
//! upholds. Callers outside this module see only safe Rust types.
//!
//! On non-Windows targets these wrappers are intentionally absent so the
//! crate still compiles for cross-platform development of the
//! platform-agnostic pieces (CLI, config, output, grouping logic).

#![allow(dead_code)]

#[cfg(windows)]
mod windows_impl;

#[cfg(windows)]
pub use windows_impl::*;

#[cfg(not(windows))]
mod stub;
#[cfg(not(windows))]
pub use stub::*;
