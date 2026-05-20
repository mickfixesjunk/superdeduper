//! Top status bar — title, action buttons, status line, big stat tiles.
//!
//! The action area exposes the only two ways the user can change what
//! the engine is doing: pick a folder and start a real scan, or kick
//! off the synthetic demo. A scan-in-progress flag disables the
//! buttons so we don't queue two engines onto the same channel.

use egui::{vec2, Align, Layout, RichText, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;

/// Actions the user can trigger from the header. Bubbled back up to
/// [`SuperdupeApp::update`] which is the only place engine threads
/// get spawned.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeaderAction {
    None,
    PickAndScan,
    StartDemo,
}

pub fn show(
    ui: &mut Ui,
    state: &UiState,
    demo_mode: bool,
    is_scanning: bool,
) -> HeaderAction {
    let mut action = HeaderAction::None;
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
        ui.add_space(8.0);

        // Action buttons.
        let scan_label = if is_scanning { "Scanning…" } else { "📂  Scan a folder" };
        let scan_btn = egui::Button::new(
            RichText::new(scan_label).color(theme::PANEL_DEEP).strong(),
        )
        .fill(theme::ACCENT)
        .min_size(vec2(160.0, 28.0));
        if ui
            .add_enabled(!is_scanning, scan_btn)
            .on_hover_text("Pick a folder and run a real scan.")
            .clicked()
        {
            action = HeaderAction::PickAndScan;
        }

        let demo_btn = egui::Button::new(
            RichText::new("▶  Demo").color(theme::TEXT_HI),
        )
        .fill(theme::PANEL_DEEP)
        .min_size(vec2(80.0, 28.0));
        if ui
            .add_enabled(!is_scanning, demo_btn)
            .on_hover_text("Replay the synthetic demo with two simulated drives.")
            .clicked()
        {
            action = HeaderAction::StartDemo;
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
    action
}

fn big_stat(ui: &mut Ui, label: &str, value: &str, color: egui::Color32) {
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
