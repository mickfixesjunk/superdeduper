//! Compile-time build metadata. Exposes the current git short SHA
//! as `SD_BUILD_SHA` (e.g. "ce0ea9f"), or "dev" when building from a
//! tarball / non-git tree. The GUI header renders this alongside the
//! Cargo version so beta testers can verify which build they're
//! running without us shipping uniquely-named EXEs.
//!
//! Rerun triggers: any commit (refreshes HEAD), any HEAD-ref change
//! (branch switch). The script does not gate the build on git being
//! present — missing git just falls through to "dev".

use std::process::Command;

fn main() {
    // Refresh SD_BUILD_SHA when HEAD moves (commit, branch switch,
    // rebase). Cheap; ignored when the file doesn't exist.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let sha = git_short_sha().unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=SD_BUILD_SHA={sha}");
}

fn git_short_sha() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8(output.stdout).ok()?;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
