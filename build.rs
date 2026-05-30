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

    // CARGO_CFG_TARGET_OS reflects the TARGET (not the host). Embed the
    // Windows .exe icon only on TARGET=windows. Further: skip when
    // cross-compiling from Linux via cargo-zigbuild, because zigbuild's
    // linker wrapper rejects the static `libresource.a` link-arg
    // winresource emits (errors with "unsupported linker arg: ...
    // libresource.a"). Native Windows builds (host=target=windows) work
    // fine and DO get the .exe icon embedded.
    let target_is_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let host_is_windows = cfg!(target_os = "windows");
    if target_is_windows {
        let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
        if target_arch == "aarch64" {
            // Arm64 Windows target: we don't have an arm64 mingw windres on
            // the WSL toolchain (only x86_64-w64-mingw32-windres is in
            // /usr/bin), so producing an arm64-compatible .o for the icon
            // resource isn't a one-line `cargo:warning` we can emit and
            // continue. Skip the .exe icon embed on arm64; the runtime egui
            // title-bar icon (from OUT_DIR/app_icon.bin) still works. v0.2.56
            // ships the aarch64-pc-windows-msvc target without the .exe icon
            // in Explorer; follow-up if needed once binutils-mingw-w64-aarch64
            // or llvm-rc is available.
            println!(
                "cargo:warning=skipping .exe-icon embed on aarch64-pc-windows-* target \
                 (no arm64 mingw windres available; runtime egui icon still works)"
            );
        } else if host_is_windows {
            embed_windows_icon_via_winresource();
        } else {
            // Cross-compile path (Linux/macOS host -> Windows target via
            // cargo-zigbuild). winresource produces a static archive
            // (libresource.a) that zigbuild's linker wrapper rejects. We
            // instead invoke the mingw windres directly to compile a
            // tiny .rc -> .o object, then pass it via
            // cargo:rustc-link-arg-bins so the linker picks it up.
            // zigbuild forwards .o object files in the link command.
            embed_windows_icon_via_windres_o();
        }
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
/// Native-Windows-host path: winresource wraps rc.exe / windres into a
/// static archive that the MSVC / mingw linker picks up automatically.
/// Used when host==target==windows.
fn embed_windows_icon_via_winresource() {
    let ico_path = "assets/sdd.ico";
    println!("cargo:rerun-if-changed={ico_path}");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path);
    res.set("ProductName", "SuperDeDuper");
    res.set("FileDescription", "SuperDeDuper — duplicate file finder");
    if let Err(e) = res.compile() {
        println!("cargo:warning=winresource embed failed: {e} (continuing without .exe icon)");
    }
}

/// Cross-compile-from-Linux path: invoke `x86_64-w64-mingw32-windres`
/// directly on a tiny `.rc` file that references `assets/sdd.ico`, write
/// the resulting `.o` to OUT_DIR, then add it as a per-bin link-arg.
/// The .o file is a regular ELF/COFF object, which cargo-zigbuild's
/// linker wrapper forwards unchanged (unlike winresource's
/// static-archive `libresource.a` which it rejects with
/// "unsupported linker arg").
///
/// Result: the same .exe-icon-in-Explorer outcome as winresource but
/// reachable from a Linux host with `cargo zigbuild --target
/// x86_64-pc-windows-gnu` in the release pipeline.
fn embed_windows_icon_via_windres_o() {
    let ico_path = "assets/sdd.ico";
    println!("cargo:rerun-if-changed={ico_path}");

    // Locate mingw windres. The release pipeline runs on a WSL host
    // where x86_64-w64-mingw32-windres is installed at /usr/bin; the
    // pure-zigbuild path doesn't include it. Probe PATH and bail
    // gracefully (warn + skip) if absent.
    let windres = ["x86_64-w64-mingw32-windres", "windres"]
        .iter()
        .find_map(|name| {
            std::process::Command::new(name)
                .arg("--version")
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|_| name.to_string())
        });
    let windres = match windres {
        Some(w) => w,
        None => {
            println!(
                "cargo:warning=skipping .exe-icon embed: mingw windres not found on PATH \
                 (apt install binutils-mingw-w64-x86-64). Runtime egui icon still works."
            );
            return;
        }
    };

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let rc_path = format!("{out_dir}/sdd_icon.rc");
    let o_path = format!("{out_dir}/sdd_icon.o");

    // Minimal Windows .rc syntax: resource_id ICON "path/to/file.ico".
    // ID 1 (IDI_APPLICATION default) is what Explorer / Win32
    // LoadIcon(NULL, IDI_APPLICATION) resolves to as the main app icon
    // when the binary is launched.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set");
    let ico_abs = format!("{manifest_dir}/{ico_path}");
    let rc_contents = format!("1 ICON \"{}\"\n", ico_abs.replace('\\', "/"));
    std::fs::write(&rc_path, rc_contents).unwrap_or_else(|e| panic!("write {rc_path}: {e}"));

    let status = std::process::Command::new(&windres)
        .args(["-O", "coff", "-i", &rc_path, "-o", &o_path])
        .status()
        .unwrap_or_else(|e| panic!("spawn {windres}: {e}"));
    if !status.success() {
        println!(
            "cargo:warning=skipping .exe-icon embed: {windres} exit code {:?}",
            status.code()
        );
        return;
    }

    // Pass the .o through to every binary's linker invocation. zigbuild
    // forwards .o files in the link command (unlike .a static archives
    // wrapped in --whole-archive).
    println!("cargo:rustc-link-arg-bins={o_path}");
}
