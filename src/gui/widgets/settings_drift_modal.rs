//! "Settings changed since the paused scan" modal — #51.
//!
//! Shown when the user clicks Start scan with roots/settings that no
//! longer match a checkpoint sitting on disk. Without this prompt,
//! the engine's `live.rs::run` filter at the resume site silently
//! drops the prior progress and starts a cold scan with the new
//! settings — Mick's "sometimes restarts from scratch after
//! inventory" symptom.
//!
//! Choices:
//! * Continue — proceed with the NEW settings, archive the
//!   checkpoint, fresh scan.
//! * Revert  — adopt the checkpoint's saved roots + settings,
//!   then launch (the engine sees a match → real resume).
//! * Cancel  — close the modal without doing anything.

use egui::{Context, RichText, Window};

use crate::gui::checkpoint::CheckpointSummary;
use crate::gui::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsDriftChoice {
    /// Discard the paused scan, continue with the new settings.
    ContinueWithNew,
    /// Adopt the checkpoint's saved roots+settings so the engine
    /// actually resumes.
    RevertToPaused,
    Cancel,
}

pub fn show(ctx: &Context, summary: &CheckpointSummary) -> Option<SettingsDriftChoice> {
    let mut choice: Option<SettingsDriftChoice> = None;
    Window::new(
        RichText::new("Settings changed since the paused scan")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
    .default_width(560.0)
    .show(ctx, |ui| {
        ui.label(
            RichText::new(
                "The roots or scan settings have changed since the paused scan was saved. \
                 Continuing now will throw away the prior progress and start a fresh scan \
                 with your current settings.",
            )
            .color(theme::TEXT_HI),
        );
        ui.add_space(8.0);
        ui.label(
            RichText::new(format!(
                "Paused scan: {} duplicate group(s) confirmed · {} root(s){}",
                summary.duplicate_count,
                summary.roots.len(),
                if summary.has_saved_inventory {
                    " · inventory saved (Stage 1 would be skipped)"
                } else {
                    " · inventory not yet saved"
                }
            ))
            .color(theme::TEXT_LO)
            .small(),
        );
        for r in summary.roots.iter().take(4) {
            let star = if r.is_reference { "★ " } else { "  " };
            ui.label(
                RichText::new(format!("{star}{}", r.path.display()))
                    .color(theme::TEXT_LO)
                    .monospace()
                    .small(),
            );
        }
        if summary.roots.len() > 4 {
            ui.label(
                RichText::new(format!("  + {} more…", summary.roots.len() - 4))
                    .color(theme::TEXT_LO)
                    .small(),
            );
        }
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Revert restores the saved roots + settings into the GUI before launching, \
                 so the engine picks the prior progress back up cleanly. The hash cache is \
                 untouched in every case.",
            )
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            let revert_btn = egui::Button::new(
                RichText::new("↩  Revert to paused settings")
                    .color(theme::PANEL_DEEP)
                    .strong(),
            )
            .fill(theme::ACCENT)
            .min_size(egui::vec2(220.0, 32.0));
            if ui
                .add(revert_btn)
                .on_hover_text(
                    "Restores the roots + settings from the paused scan and starts the \
                     engine — your in-flight settings changes are discarded.",
                )
                .clicked()
            {
                choice = Some(SettingsDriftChoice::RevertToPaused);
            }
            let continue_btn = egui::Button::new(
                RichText::new("⚠  Continue with new settings").color(theme::TEXT_HI),
            )
            .min_size(egui::vec2(220.0, 32.0));
            if ui
                .add(continue_btn)
                .on_hover_text(
                    "Archives the previous checkpoint to a .bak sibling and starts a \
                     fresh scan with your current settings.",
                )
                .clicked()
            {
                choice = Some(SettingsDriftChoice::ContinueWithNew);
            }
            let cancel_btn = egui::Button::new(RichText::new("Cancel").color(theme::TEXT_HI))
                .min_size(egui::vec2(100.0, 32.0));
            if ui.add(cancel_btn).clicked() {
                choice = Some(SettingsDriftChoice::Cancel);
            }
        });
    });
    choice
}
