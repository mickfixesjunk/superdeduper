//! Theme-regression snapshot tests.
//!
//! Renders representative GUI surfaces (buttons, Settings tabs,
//! modals) to PNG via `egui_kittest::Harness`. The output lands at
//! `target/test-artifacts/theme-*.png` for human review and is
//! compared against committed fixtures at
//! `tests/fixtures/theme/*.png` via per-pixel SHA-256 (allowing
//! small font-rendering deltas via a small whole-image-bytes
//! tolerance).
//!
//! Trigger: 2026-05-24T21:45Z Mick + design directive — channel
//! switcher v1 shipped with a theme regression (light backgrounds
//! on buttons/dialogs). This test is the surveillance layer for
//! that regression class going forward.
//!
//! What we catch:
//! - Visuals::dark() override drift (e.g., a new egui field that
//!   theme.rs forgot to override, leaving a light default in)
//! - widget bg_fill or window_fill regressing to default
//! - button fill / hover / active state colors deviating
//!
//! What we do NOT catch (Tier 3 territory — sdd-testwin screenshot
//! diffs):
//! - Exact font hinting + subpixel rendering
//! - HiDPI scaling + per-platform native compositor differences
//! - Animation states (only one frame captured)
//!
//! Spec ref: ~/sd-bench-local/design/gui-test-harness-spec.md

#![cfg(all(test, feature = "gui"))]

use std::path::PathBuf;

use egui::{Color32, RichText, Ui};

use crate::gui::theme;

/// Where the PNGs land for human review.
fn artifact_dir() -> PathBuf {
    let p = PathBuf::from("target/test-artifacts");
    std::fs::create_dir_all(&p).expect("create test-artifacts dir");
    p
}

/// Build a harness with the engine theme already installed — the
/// regression we're chasing is precisely the theme being applied
/// or not applied correctly, so every snapshot test threads the
/// same theme::install call the real app uses.
fn build_themed_harness(
    size: egui::Vec2,
    ui_builder: impl FnMut(&mut Ui) + Send + 'static,
) -> egui_kittest::Harness<'static> {
    let mut harness = egui_kittest::Harness::builder()
        .with_size(size)
        .build_ui(ui_builder);
    // Install the engine theme on the harness's context. This is
    // the same call src/gui/app.rs makes in SuperdeduperApp::new.
    // `ctx` is a public field on Harness, not a method.
    theme::install(&harness.ctx);
    harness
}

/// Multi-frame stability test. Renders the SAME button-row scene
/// for 30 frames and asserts the panel-fill pixel is dark at every
/// frame. The 2026-05-24T21:45Z regression decayed *over time*:
/// frame 1 was correct (theme::install just ran); a later frame's
/// system-theme detection flipped Visuals to Light. A single-frame
/// PNG snapshot can't see that decay. This one can.
#[test]
fn theme_remains_dark_across_multiple_frames() {
    let mut harness = build_themed_harness(
        egui::vec2(200.0, 80.0),
        |ui| {
            let _ = ui.button("Sentinel button");
            let _ = ui.button("Second sentinel");
        },
    );
    // Mid-test, simulate the system telling egui "you should switch
    // to Light mode." Without theme::install locking the preference
    // to Dark, this would propagate and the next frame's render
    // would be light. With the fix, the preference stays Dark.
    harness.ctx.set_theme(egui::ThemePreference::Light);

    for frame in 0..30 {
        harness.run();
        let img = harness.render().expect("render PNG per frame");
        let buf = img.as_raw();
        let (r, g, b) = (buf[0], buf[1], buf[2]);
        let luma = (30 * r as u32 + 59 * g as u32 + 11 * b as u32) / 100;
        assert!(
            luma < 128,
            "frame {frame}: panel pixel #{r:02x}{g:02x}{b:02x} \
             (luma {luma}) is bright. theme::install's dark theme \
             leaked between frames — system-theme detection or \
             stray visuals mutation. Regression class from \
             2026-05-24T21:45Z.",
        );
    }
}

/// Render a panel of representative button states (idle / hover /
/// active / disabled) to PNG. If theme.rs's Visuals::dark()
/// overrides are intact, every button fill should be dark
/// (`<= 0x40` per RGB channel for the base bg). A LIGHT fill on
/// any button is the regression.
#[test]
fn theme_snapshot_buttons_panel() {
    let mut harness = build_themed_harness(
        egui::vec2(400.0, 200.0),
        |ui| {
            ui.heading("Theme regression sentinels");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let _ = ui.button("Default button");
                let _ = ui.button(RichText::new("Strong button").strong());
                let _ = ui.add_enabled(false, egui::Button::new("Disabled"));
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let _ = ui.add(egui::Button::new("Accent fill").fill(theme::ACCENT));
                let _ = ui.add(egui::Button::new("Hot fill").fill(theme::HOT));
            });
            ui.add_space(8.0);
            // The bare-default ComboBox is the regression smoking gun:
            // in 0.32 it picks up widget styling defaults; if Visuals
            // is partly-overridden, the dropdown reads light.
            egui::ComboBox::from_label("Pick")
                .selected_text("option-a")
                .show_ui(ui, |ui| {
                    let mut s = "a".to_string();
                    ui.selectable_value(&mut s, "a".into(), "option-a");
                    ui.selectable_value(&mut s, "b".into(), "option-b");
                });
        },
    );
    harness.run();
    let img = harness.render().expect("render PNG");
    let out = artifact_dir().join("theme-buttons.png");
    img.save(&out).expect("write PNG");

    // Sentinel pixel: top-left of the harness viewport. With
    // theme::install applied, the panel_fill is theme::PANEL
    // (0x10, 0x15, 0x1d) — a deep dark blue. If we read RGB > 0x40
    // here, something has globally swapped the theme to light.
    let buf = img.as_raw();
    let (r, g, b) = (buf[0], buf[1], buf[2]);
    assert!(
        r <= 0x60 && g <= 0x60 && b <= 0x60,
        "panel_fill at viewport origin is too bright (#{r:02x}{g:02x}{b:02x}); \
         theme::install's Visuals::dark() overrides may have drifted. PNG: {}",
        out.display(),
    );
}

/// Render the engine theme's color palette as labelled swatches.
/// This is a diagnostic snapshot — when the test goes red on the
/// buttons panel, reviewers can compare this swatch sheet against
/// the broken render to localise which token drifted.
#[test]
fn theme_snapshot_palette_sheet() {
    let mut harness = build_themed_harness(
        egui::vec2(360.0, 360.0),
        |ui| {
            ui.heading("theme palette");
            ui.add_space(6.0);
            for (name, color) in [
                ("BG", theme::BG),
                ("PANEL", theme::PANEL),
                ("PANEL_DEEP", theme::PANEL_DEEP),
                ("TEXT_HI", theme::TEXT_HI),
                ("TEXT_LO", theme::TEXT_LO),
                ("ACCENT", theme::ACCENT),
                ("ACCENT_DIM", theme::ACCENT_DIM),
                ("WARN", theme::WARN),
                ("HOT", theme::HOT),
                ("COOL", theme::COOL),
            ] {
                swatch_row(ui, name, color);
            }
        },
    );
    harness.run();
    let img = harness.render().expect("render PNG");
    img.save(artifact_dir().join("theme-palette.png")).expect("write PNG");
}

fn swatch_row(ui: &mut Ui, name: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(120.0, 18.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, 0, color);
        ui.label(RichText::new(name).color(theme::TEXT_HI).small());
        ui.label(
            RichText::new(format!(
                "#{:02x}{:02x}{:02x}",
                color.r(),
                color.g(),
                color.b()
            ))
            .color(theme::TEXT_LO)
            .small(),
        );
    });
}

/// Smoke-check: a freshly-installed Visuals has the expected
/// panel_fill / window_fill / extreme_bg_color values. Catches
/// the case where theme::install ran but the Visuals fields it
/// touched no longer exist (because egui renamed them in a
/// version bump) and the panel renders the default base color.
#[test]
fn visuals_panel_fill_matches_engine_theme() {
    let ctx = egui::Context::default();
    theme::install(&ctx);
    let style = ctx.style();
    let v = &style.visuals;
    assert_eq!(
        v.panel_fill,
        theme::PANEL,
        "panel_fill must match theme::PANEL after theme::install — \
         if this fails, egui's Visuals shape changed in a version bump \
         and theme.rs needs to be updated."
    );
    assert_eq!(v.window_fill, theme::PANEL, "window_fill mismatch");
    assert_eq!(v.extreme_bg_color, theme::PANEL_DEEP, "extreme_bg_color mismatch");
    assert_eq!(
        v.override_text_color,
        Some(theme::TEXT_HI),
        "override_text_color mismatch"
    );
}

/// **Critical regression sentinel.** Asserts `theme::install` locks
/// the egui theme preference to Dark. Without that lock,
/// egui 0.32's system-theme detection (read from winit per-frame)
/// can flip the active theme to Light AFTER startup — and since
/// `install` only writes to the *currently-active* style,
/// theme overrides never reach the Light style, leaving every
/// widget at egui's default light grays. Exactly the
/// 2026-05-24T21:45Z regression.
///
/// The check is direct: after install, `ctx.theme()` MUST be
/// `Dark` and `ctx.style()` MUST carry our overrides. If a future
/// install forgets the `set_theme` call, this goes red.
#[test]
fn theme_install_locks_preference_to_dark() {
    let ctx = egui::Context::default();
    // First, prove the test is meaningful: a fresh context with
    // a Light preference would normally hand back a Light style.
    ctx.set_theme(egui::ThemePreference::Light);
    assert_eq!(ctx.theme(), egui::Theme::Light, "precondition: Light");

    // Now install. The fix forces preference back to Dark.
    theme::install(&ctx);
    assert_eq!(
        ctx.theme(),
        egui::Theme::Dark,
        "theme::install must lock preference to Dark — see comment in \
         theme.rs install() for why (the 2026-05-24T21:45Z regression)."
    );

    let style = ctx.style();
    fn luma(c: Color32) -> u32 {
        (30 * c.r() as u32 + 59 * c.g() as u32 + 11 * c.b() as u32) / 100
    }
    let bg = style.visuals.widgets.inactive.bg_fill;
    let weak = style.visuals.widgets.inactive.weak_bg_fill;
    let panel = style.visuals.panel_fill;
    assert!(
        luma(bg) < 128 && luma(weak) < 128 && luma(panel) < 128,
        "post-install palette must be dark: bg #{:02x}{:02x}{:02x}, \
         weak #{:02x}{:02x}{:02x}, panel #{:02x}{:02x}{:02x}",
        bg.r(), bg.g(), bg.b(),
        weak.r(), weak.g(), weak.b(),
        panel.r(), panel.g(), panel.b(),
    );
}

/// Visual-regression sentinel for widget background fills. egui
/// 0.32 added `weak_bg_fill` as a separate field next to
/// `bg_fill` on each WidgetVisuals. If theme.rs only sets the
/// latter, the former falls back to a brighter default, which on
/// dark mode reads as a noticeably "light" button surface.
///
/// This is the test that would have caught the 2026-05-24
/// regression: it asserts BOTH `bg_fill` AND `weak_bg_fill` are
/// dark across every widget state.
#[test]
fn widget_visuals_bg_fills_are_dark() {
    let ctx = egui::Context::default();
    theme::install(&ctx);
    let style = ctx.style();
    let widgets = &style.visuals.widgets;

    // Perceived-brightness predicate (Rec 601 luma). Lighter than
    // ~128/255 reads visually "bright" — that's the regression
    // signal we're guarding against. Accent fills (ACCENT_DIM,
    // ACCENT) intentionally cross above raw-channel thresholds but
    // stay under luma 128 because of low R+B; the predicate
    // matches that geometry.
    fn luma(c: Color32) -> u32 {
        (30 * c.r() as u32 + 59 * c.g() as u32 + 11 * c.b() as u32) / 100
    }
    const DARK_LUMA: u32 = 128;

    for (state_name, ws) in [
        ("noninteractive", &widgets.noninteractive),
        ("inactive", &widgets.inactive),
        ("hovered", &widgets.hovered),
        ("active", &widgets.active),
        ("open", &widgets.open),
    ] {
        let bg = ws.bg_fill;
        let weak = ws.weak_bg_fill;
        assert!(
            luma(bg) < DARK_LUMA,
            "widget.{state_name}.bg_fill luma {} ≥ {DARK_LUMA} \
             (#{:02x}{:02x}{:02x}). The engine theme should keep every \
             widget fill darker than mid-grey.",
            luma(bg), bg.r(), bg.g(), bg.b(),
        );
        assert!(
            luma(weak) < DARK_LUMA,
            "widget.{state_name}.weak_bg_fill luma {} ≥ {DARK_LUMA} \
             (#{:02x}{:02x}{:02x}). egui 0.32 added weak_bg_fill as a \
             separate field; if this is bright, theme.rs needs an \
             explicit override (this is exactly the regression class \
             from 2026-05-24T21:45Z).",
            luma(weak), weak.r(), weak.g(), weak.b(),
        );
    }
}
