//! Scan-mode dropdown — picks the similarity mode for the next scan
//! per spec §3.8.
//!
//! V2.5 scope (this widget):
//!   * Three-way dropdown — Exact (default) / Image / Audio.
//!   * Session-sticky, NOT persisted across runs (per spec).
//!   * Tooltip on each mode explains current engine status (CLI
//!     today, GUI integration coming with v3).
//!
//! Not yet:
//!   * Picking Image doesn't actually drive Tier-4 in the GUI's
//!     scan pipeline — `gui::live::run()` integration is the v3
//!     sub-deliverable. The CLI already runs Tier-4 today via
//!     `superdeduper scan --mode image`.

#![cfg(feature = "gui")]

use egui::{RichText, Ui};

use crate::cli::ScanMode;
use crate::gui::theme;

/// Render the mode picker. Mutates `current` in place. Disabled
/// while a scan is in flight (mode picks at scan-start, not mid-
/// scan).
pub fn show(ui: &mut Ui, current: &mut ScanMode, is_scanning: bool) {
    ui.label(
        RichText::new("Scan mode")
            .color(theme::TEXT_LO)
            .strong()
            .small(),
    );
    ui.add_space(2.0);

    ui.add_enabled_ui(!is_scanning, |ui| {
        egui::ComboBox::from_id_salt("sd_scan_mode_picker")
            .selected_text(label_for(*current))
            .width(180.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(current, ScanMode::Exact, label_for(ScanMode::Exact))
                    .on_hover_text(
                        "Byte-identical duplicate detection — the default. \
                         Finds files whose contents are exactly the same.",
                    );
                ui.selectable_value(current, ScanMode::Image, label_for(ScanMode::Image))
                    .on_hover_text(
                        "Perceptual image similarity (T1.2). \
                         Finds resize / format-conversion / re-encode twins. \
                         CLI today: `superdeduper scan --mode image`. \
                         GUI integration coming with v3 — mode picker \
                         currently records the selection but the engine \
                         falls through to exact-mode behaviour for GUI scans.",
                    );
                ui.selectable_value(current, ScanMode::Audio, label_for(ScanMode::Audio))
                    .on_hover_text(
                        "Acoustic audio similarity (T1.3). \
                         Not yet implemented — placeholder reservation for the \
                         shared mode dropdown.",
                    );
            });
    });

    // Inline status hint when a non-Exact mode is picked but the GUI
    // engine integration isn't wired yet. Honest UX — the user shouldn't
    // click Start scan expecting perceptual results and silently get
    // exact-only behaviour.
    if !matches!(current, ScanMode::Exact) {
        ui.add_space(2.0);
        let msg = match current {
            ScanMode::Image => {
                "Image mode is wired in the CLI today (superdeduper scan \
                 --mode image). The GUI engine integration is the v3 sub-\
                 deliverable per the t1.2 spec. Picking Image and clicking \
                 Start scan will fall through to exact dedup for now."
            }
            ScanMode::Audio => {
                "Audio mode is not yet implemented (T1.3 / #26). The \
                 dropdown is here as a placeholder for the shared mode \
                 picker per Mick's directive."
            }
            ScanMode::Exact => unreachable!(),
        };
        ui.label(RichText::new(msg).color(theme::WARN).small().italics());
    }
}

fn label_for(m: ScanMode) -> &'static str {
    match m {
        ScanMode::Exact => "Exact match",
        ScanMode::Image => "Image similarity",
        ScanMode::Audio => "Audio similarity",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_strings_are_stable() {
        // These strings appear on the dropdown surface; lock the
        // wording so accidental renames in IDE-driven refactors
        // show up as failing tests rather than silent UI changes.
        assert_eq!(label_for(ScanMode::Exact), "Exact match");
        assert_eq!(label_for(ScanMode::Image), "Image similarity");
        assert_eq!(label_for(ScanMode::Audio), "Audio similarity");
    }
}
