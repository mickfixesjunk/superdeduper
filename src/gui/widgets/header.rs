//! Top status bar — title, settings button, status line, big stat tiles.

use egui::{vec2, Align, Color32, Frame, Layout, RichText, Rounding, Sense, Stroke, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;
use crate::pipeline::hash::HashAlgo;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeaderAction {
    None,
    OpenSettings,
}

pub fn show(ui: &mut Ui, state: &UiState, hash_algo: HashAlgo, _is_scanning: bool) -> HeaderAction {
    let mut action = HeaderAction::None;
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("superdeduper").color(theme::ACCENT).heading());
        ui.label(RichText::new(env!("CARGO_PKG_VERSION")).color(theme::TEXT_LO));
        ui.add_space(8.0);

        // Hash-algo pill — shows the active content-hash algo and
        // doubles as a fast-path to the Settings modal (clicking
        // opens settings). Pill colour distinguishes the two algos
        // at a glance.
        if hash_algo_pill(ui, hash_algo) {
            action = HeaderAction::OpenSettings;
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

/// Coloured rounded "pill" showing the currently-selected content-
/// hash algorithm. Whole pill is one click target; clicking returns
/// `true` so the caller can flip its `HeaderAction` to OpenSettings.
/// The pill is the prominent way to see "what am I hashing with right
/// now?" without diving into Settings.
fn hash_algo_pill(ui: &mut Ui, algo: HashAlgo) -> bool {
    let (label, fill) = match algo {
        HashAlgo::Blake3 => ("BLAKE3", theme::COOL),
        HashAlgo::River5 => ("RIVER5", theme::ACCENT),
    };
    let tip = match algo {
        HashAlgo::Blake3 => {
            "Content hash: BLAKE3 (32-byte cryptographic). Click to change in Settings."
        }
        HashAlgo::River5 => {
            "Content hash: RIVER5 (16-byte, AES-NI hardware-accelerated). Click to change in Settings."
        }
    };
    // Pill colours: a translucent fill of the algo's theme colour
    // plus a contrasting border so it reads at a glance even on
    // the dark background.
    let bg = Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 32);
    let frame = Frame::none()
        .fill(bg)
        .rounding(Rounding::same(10.0))
        .stroke(Stroke::new(1.0, fill))
        .inner_margin(egui::Margin::symmetric(10.0, 4.0));
    let resp = frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("#")
                        .color(fill)
                        .monospace()
                        .strong()
                        .size(13.0),
                );
                ui.label(
                    RichText::new(label)
                        .color(fill)
                        .monospace()
                        .strong()
                        .size(13.0),
                );
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_text(tip);
    resp.clicked()
}
