//! Compile-time build metadata + app-icon embed.
//!
//! Exposes the current git short SHA as `SD_BUILD_SHA` (e.g. "ce0ea9f"), or
//! "dev" when building from a tarball / non-git tree. The GUI header renders
//! this alongside the Cargo version so beta testers can verify which build
//! they're running without us shipping uniquely-named EXEs.
//!
//! Also pre-decodes the SDD logo PNG into raw RGBA bytes (written to
//! `OUT_DIR/app_icon.bin` with a 16-byte header: u64le rgba_len, u32le width,
//! u32le height) so the GUI bin can `include_bytes!` it at compile time and
//! hand it to `egui::ViewportBuilder::with_icon` without pulling the `image`
//! crate into the runtime tree. On Windows targets, also embeds the .ico via
//! `winresource` so the .exe shows the SDD shield in Explorer / taskbar.
//!
//! Rerun triggers: any commit (refreshes HEAD), any HEAD-ref change (branch
//! switch), or any change to the source PNG / ICO assets.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");

    let sha = git_short_sha().unwrap_or_else(|| "dev".to_string());
    println!("cargo:rustc-env=SD_BUILD_SHA={sha}");

    write_app_icon_rgba();

    // CARGO_CFG_TARGET_OS reflects the TARGET (not the host), so this
    // fires for cross-compile-from-Linux to Windows via cargo-zigbuild.
    // `#[cfg(target_os = "windows")]` would only fire on a Windows host
    // build — wrong for our release pipeline.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_windows_icon();
    }
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

/// Pre-decode assets/sdd.png into raw RGBA bytes for the GUI runtime icon.
/// Output layout in `OUT_DIR/app_icon.bin`:
///   bytes  0.. 4 = u32le width
///   bytes  4.. 8 = u32le height
///   bytes  8..   = width*height*4 bytes RGBA
/// The GUI bin parses this header back at startup. A 256x256 icon is ~256 KB
/// raw which is small enough to ship inline. Downsizing to 128x128 (~64 KB)
/// is fine too and is what we do here — egui scales for the title bar; the
/// .ico path is what Explorer uses for large rendering.
fn write_app_icon_rgba() {
    let png_path = "assets/sdd.png";
    println!("cargo:rerun-if-changed={png_path}");

    let img = image::open(png_path).unwrap_or_else(|e| {
        panic!("build.rs: failed to open {png_path}: {e}");
    });
    // Square-pad + resize to 128x128 RGBA. The source asset is rectangular
    // (640x480); a naive resize would squash the shield. Instead, scale to
    // fit and center on a transparent square canvas.
    let rgba = {
        use image::imageops::FilterType;
        let target = 128u32;
        let (sw, sh) = (img.width(), img.height());
        let scale = (target as f32 / sw.max(sh) as f32).min(1.0);
        let fit_w = ((sw as f32) * scale).round() as u32;
        let fit_h = ((sh as f32) * scale).round() as u32;
        let resized = img.resize_exact(fit_w, fit_h, FilterType::Lanczos3).to_rgba8();
        let mut canvas: image::RgbaImage = image::ImageBuffer::from_pixel(target, target, image::Rgba([0, 0, 0, 0]));
        let ox = (target - fit_w) / 2;
        let oy = (target - fit_h) / 2;
        image::imageops::overlay(&mut canvas, &resized, ox.into(), oy.into());
        canvas
    };
    let (w, h) = rgba.dimensions();
    let raw = rgba.into_raw();
    debug_assert_eq!(raw.len(), (w as usize) * (h as usize) * 4);

    let mut out = Vec::with_capacity(8 + raw.len());
    out.extend_from_slice(&w.to_le_bytes());
    out.extend_from_slice(&h.to_le_bytes());
    out.extend_from_slice(&raw);

    let dst = format!("{}/app_icon.bin", std::env::var("OUT_DIR").unwrap());
    std::fs::write(&dst, &out).unwrap_or_else(|e| panic!("build.rs: write {dst}: {e}"));
}

/// Windows-target: embed assets/sdd.ico into the resulting .exe via
/// `winresource`. Affects ALL binaries in the crate (both the CLI and the
/// GUI superdeduper.exe inherit the icon) so Explorer, taskbar, alt-tab,
/// and the Programs list all show the SDD shield instead of the default.
fn embed_windows_icon() {
    let ico_path = "assets/sdd.ico";
    println!("cargo:rerun-if-changed={ico_path}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path);
    res.set("ProductName", "SuperDeDuper");
    res.set("FileDescription", "SuperDeDuper — duplicate file finder");
    if let Err(e) = res.compile() {
        // Don't fail the build for cross-compile environments that can't run
        // windres / rc.exe; just warn so the .exe still produces (without
        // the embedded icon, which is a polish loss but not a correctness
        // issue). winresource itself prefers windres on mingw cross targets.
        println!("cargo:warning=winresource embed failed: {e} (continuing without .exe icon)");
    }
}
