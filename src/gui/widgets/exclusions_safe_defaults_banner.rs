//! #81 — One-shot "Exclusions enabled with safe defaults" banner.
//!
//! v0.2.7 turns on the safe-defaults exclusion filter for every
//! existing install (Mick directive 2026-05-25). A silent flip
//! without a notice would leave users wondering why .dll groups
//! they saw in v0.2.6 are missing — so this banner surfaces the
//! change once, with a deeplink into the Settings → Exclusions
//! tab where they can see what's protected (and turn off any pack
//! that conflicts with their workflow).
//!
//! Dismissed via the "Got it" button (flips
//! `ScanSettings::dismissed_v0_2_7_exclusion_banner` to true) or
//! by clicking "See what's filtered" which opens Settings AND
//! dismisses in one click.
//!
//! Non-modal, non-dimming. Rendered as a TopBottomPanel above the
//! main UI so it doesn't sit ON TOP of any other content.

#![cfg(feature = "gui")]

use egui::{Color32, Layout, RichText, TopBottomPanel};

use crate::gui::theme;

/// Banner height — sized to fit the headline + body + buttons on
/// one row at typical zoom levels.
pub const BANNER_HEIGHT: f32 = 60.0;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BannerAction {
    /// User clicked "See what's filtered" — caller opens Settings
    /// → Exclusions AND flips the dismissed flag.
    OpenSettings,
    /// User clicked "Got it" — caller just flips the dismissed flag.
    Dismiss,
}

pub fn show(ctx: &egui::Context) -> Option<BannerAction> {
    let mut action: Option<BannerAction> = None;
    TopBottomPanel::top("sd_exclusions_safe_defaults_banner")
        .exact_height(BANNER_HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::NONE
                .fill(Color32::from_rgb(0x1f, 0x2c, 0x3a))
                .stroke(egui::Stroke::new(1.0, theme::ACCENT))
                .inner_margin(egui::Margin::symmetric(12, 8)),
        )
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Exclusions enabled with safe defaults")
                            .color(theme::TEXT_HI)
                            .size(13.0)
                            .strong(),
                    );
                    ui.label(
                        RichText::new(
                            "v0.2.7 skips known-dangerous files (system libraries, \
                             .git internals, OS-protected paths, AV signature databases) \
                             so .dll / .sys / C:\\Windows entries don't show up as dupes.",
                        )
                        .color(theme::TEXT_LO)
                        .small(),
                    );
                });
                ui.with_layout(Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(RichText::new("Got it").color(theme::TEXT_HI))
                        .clicked()
                    {
                        action = Some(BannerAction::Dismiss);
                    }
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("See what's filtered")
                                    .color(theme::PANEL_DEEP)
                                    .strong(),
                            )
                            .fill(theme::ACCENT),
                        )
                        .clicked()
                    {
                        action = Some(BannerAction::OpenSettings);
                    }
                });
            });
        });
    action
}
