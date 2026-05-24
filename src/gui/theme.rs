//! Dark palette inspired by oscilloscope CRTs — high contrast, easy on
//! the eyes during long scans. All colors live here so the rest of the
//! GUI never hardcodes RGB.

use egui::{Color32, FontFamily, FontId, Style, TextStyle, Visuals};

pub const BG: Color32 = Color32::from_rgb(0x0a, 0x0d, 0x12);
pub const PANEL: Color32 = Color32::from_rgb(0x10, 0x15, 0x1d);
pub const PANEL_DEEP: Color32 = Color32::from_rgb(0x07, 0x0a, 0x10);
pub const TEXT_HI: Color32 = Color32::from_rgb(0xe8, 0xee, 0xf5);
pub const TEXT_LO: Color32 = Color32::from_rgb(0x8c, 0x9a, 0xae);
pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0xd1, 0xc5);
pub const ACCENT_DIM: Color32 = Color32::from_rgb(0x21, 0x6a, 0x65);
pub const WARN: Color32 = Color32::from_rgb(0xf6, 0xad, 0x55);
pub const HOT: Color32 = Color32::from_rgb(0xfc, 0x53, 0x85);
pub const COOL: Color32 = Color32::from_rgb(0x69, 0x9b, 0xff);
pub const HDD: Color32 = Color32::from_rgb(0xf6, 0xad, 0x55);
pub const SSD: Color32 = Color32::from_rgb(0x4f, 0xd1, 0xc5);

/// Eight bucket colors for the eight pipeline stages. Funnel + stage
/// list reuse these so the eye associates each color with a stage.
pub const STAGE_COLORS: [Color32; 8] = [
    Color32::from_rgb(0x69, 0x9b, 0xff), // Inventory — blue
    Color32::from_rgb(0x8a, 0xb4, 0xff), // SizeGroup
    Color32::from_rgb(0xa0, 0xc6, 0xff), // LayoutResolve
    Color32::from_rgb(0x4f, 0xd1, 0xc5), // Tier0 — teal
    Color32::from_rgb(0x9a, 0xe6, 0xb4), // Tier1
    Color32::from_rgb(0xf6, 0xe0, 0x5d), // Tier2 — yellow
    Color32::from_rgb(0xfc, 0xa6, 0x52), // Tier3 — orange
    Color32::from_rgb(0xfc, 0x53, 0x85), // Confirmed — pink (the prize)
];

pub fn install(ctx: &egui::Context) {
    // Lock the theme preference to Dark BEFORE writing visuals.
    // egui 0.32+ defaults `ThemePreference::System`, which lets the
    // OS's light/dark setting flip the active style per-frame. On a
    // Windows-Light machine that bypasses every override below
    // because egui silently routes `set_style` writes to the
    // *current-theme* style — which is the Light one we never
    // touched. Setting the preference explicitly to Dark makes our
    // overrides land in the only style ever consulted.
    ctx.set_theme(egui::ThemePreference::Dark);

    let mut style = Style::default();
    let mut v = Visuals::dark();
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.extreme_bg_color = PANEL_DEEP;
    v.faint_bg_color = Color32::from_rgb(0x16, 0x1c, 0x26);
    v.override_text_color = Some(TEXT_HI);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT_DIM;
    v.window_stroke.color = Color32::from_rgb(0x1f, 0x28, 0x36);
    // bg_fill drives stroke / checkbox / non-button surfaces.
    // weak_bg_fill drives the button background since egui 0.32
    // (the field that was at fault for the 2026-05-24T21:45Z
    // regression — egui's default dark `weak_bg_fill` is gray(60),
    // ok-ish, but goes very light if system-theme detection flips
    // to Light mode mid-frame). Set both so every code path lands
    // on the engine palette.
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.inactive.bg_fill = Color32::from_rgb(0x18, 0x20, 0x2c);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(0x18, 0x20, 0x2c);
    v.widgets.hovered.bg_fill = Color32::from_rgb(0x24, 0x2f, 0x40);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(0x24, 0x2f, 0x40);
    v.widgets.active.bg_fill = ACCENT_DIM;
    v.widgets.active.weak_bg_fill = ACCENT_DIM;
    v.widgets.open.bg_fill = Color32::from_rgb(0x18, 0x20, 0x2c);
    v.widgets.open.weak_bg_fill = Color32::from_rgb(0x18, 0x20, 0x2c);
    style.visuals = v;

    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(20.0, FontFamily::Proportional),
        ),
        (TextStyle::Body, FontId::new(13.0, FontFamily::Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
        (
            TextStyle::Button,
            FontId::new(13.0, FontFamily::Proportional),
        ),
        (
            TextStyle::Small,
            FontId::new(11.0, FontFamily::Proportional),
        ),
    ]
    .into();

    ctx.set_style(style);
}

/// Format `bytes` using binary prefixes, fixed-width-ish.
pub fn humansize(bytes: u64) -> String {
    humansize::format_size(bytes, humansize::BINARY)
}
