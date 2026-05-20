//! Top status bar — the "what's happening right now" line.

use egui::{vec2, Align, Layout, RichText, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;

pub fn show(ui: &mut Ui, state: &UiState, demo_mode: bool) {
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("superdupe").color(theme::ACCENT).heading());
        ui.label(RichText::new(env!("CARGO_PKG_VERSION")).color(theme::TEXT_LO));
        if demo_mode {
            ui.label(
                RichText::new("DEMO")
                    .color(theme::WARN)
                    .small()
                    .strong(),
            );
        }
        ui.separator();
        ui.label(RichText::new(&state.status).color(theme::TEXT_HI));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            big_stat(ui, "Reclaimable", &theme::humansize(state.totals.reclaimable_bytes), theme::HOT);
            big_stat(
                ui,
                "Duplicates",
                &state.totals.duplicates.to_string(),
                theme::ACCENT,
            );
            big_stat(
                ui,
                "Read",
                &theme::humansize(state.totals.bytes_read),
                theme::COOL,
            );
            if let Some(elapsed) = state.scan_elapsed() {
                big_stat(
                    ui,
                    "Elapsed",
                    &format!("{:.1}s", elapsed.as_secs_f64()),
                    theme::TEXT_HI,
                );
            }
        });
    });
}

fn big_stat(ui: &mut Ui, label: &str, value: &str, color: egui::Color32) {
    // Allocate a fixed-size sub-region so the outer right-to-left layout
    // doesn't break the inner vertical's text into single-char lines.
    ui.allocate_ui_with_layout(
        vec2(140.0, 38.0),
        Layout::top_down(Align::Max),
        |ui| {
            ui.label(RichText::new(value).color(color).strong().size(18.0));
            ui.label(RichText::new(label).color(theme::TEXT_LO).small());
        },
    );
    ui.add_space(12.0);
}
