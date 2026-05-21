//! Top status bar — title, settings button, status line, big stat tiles.

use egui::{vec2, Align, Layout, RichText, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeaderAction {
    None,
    OpenSettings,
    StartDemo,
}

pub fn show(ui: &mut Ui, state: &UiState, demo_mode: bool, is_scanning: bool) -> HeaderAction {
    let mut action = HeaderAction::None;
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("superdupe").color(theme::ACCENT).heading());
        ui.label(RichText::new(env!("CARGO_PKG_VERSION")).color(theme::TEXT_LO));
        if demo_mode {
            ui.label(RichText::new("DEMO").color(theme::WARN).small().strong());
        }
        ui.add_space(8.0);

        let settings_btn = egui::Button::new(RichText::new("⚙  Settings").color(theme::TEXT_HI))
            .fill(theme::PANEL_DEEP)
            .min_size(vec2(110.0, 28.0));
        if ui
            .add(settings_btn)
            .on_hover_text("Engine options (size filters, glob patterns, format-aware, threads).")
            .clicked()
        {
            action = HeaderAction::OpenSettings;
        }

        let demo_btn = egui::Button::new(RichText::new("▶  Demo").color(theme::TEXT_HI))
            .fill(theme::PANEL_DEEP)
            .min_size(vec2(80.0, 28.0));
        if ui
            .add_enabled(!is_scanning, demo_btn)
            .on_hover_text("Replay the synthetic engine demo.")
            .clicked()
        {
            action = HeaderAction::StartDemo;
        }

        ui.separator();
        let status = if state.status.is_empty() {
            "Idle — add folders in the sidebar, then click Start scan."
        } else {
            state.status.as_str()
        };
        ui.label(RichText::new(status).color(theme::TEXT_HI));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            big_stat(
                ui,
                "Reclaimable",
                &theme::humansize(state.totals.reclaimable_bytes),
                theme::HOT,
            );
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
    action
}

fn big_stat(ui: &mut Ui, label: &str, value: &str, color: egui::Color32) {
    ui.allocate_ui_with_layout(vec2(140.0, 38.0), Layout::top_down(Align::Max), |ui| {
        ui.label(RichText::new(value).color(color).strong().size(18.0));
        ui.label(RichText::new(label).color(theme::TEXT_LO).small());
    });
    ui.add_space(12.0);
}
