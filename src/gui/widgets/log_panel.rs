//! Log panel — surfaces engine warnings, errors, and per-stage info.
//!
//! Solves the "scanned Program Files, got no results, no idea why"
//! problem: walker errors (permission denied, broken reparse points,
//! files-in-use) and engine status updates land here so the user can
//! see what actually happened.

use egui::{RichText, ScrollArea, Ui};

use crate::gui::events::LogLevel;
use crate::gui::state::UiState;
use crate::gui::theme;

pub fn show(ui: &mut Ui, state: &UiState) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("Engine log").color(theme::TEXT_LO).strong());
        let (warn, err) = state
            .logs
            .iter()
            .fold((0u32, 0u32), |(w, e), l| match l.level {
                LogLevel::Warn => (w + 1, e),
                LogLevel::Error => (w, e + 1),
                _ => (w, e),
            });
        if warn > 0 {
            ui.label(
                RichText::new(format!("{} warn", warn))
                    .color(theme::WARN)
                    .small(),
            );
        }
        if err > 0 {
            ui.label(
                RichText::new(format!("{} err", err))
                    .color(theme::HOT)
                    .small(),
            );
        }
    });
    ui.add_space(2.0);

    if state.logs.is_empty() {
        ui.label(
            RichText::new("No log entries yet.")
                .color(theme::TEXT_LO)
                .italics()
                .small(),
        );
        return;
    }

    ScrollArea::vertical()
        .id_source("log-panel")
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in state.logs.iter().rev().take(500).rev() {
                let (tag, color) = match entry.level {
                    LogLevel::Info => ("info ", theme::TEXT_LO),
                    LogLevel::Warn => ("warn ", theme::WARN),
                    LogLevel::Error => ("error", theme::HOT),
                };
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tag).color(color).monospace().small().strong());
                    ui.label(
                        RichText::new(&entry.message)
                            .color(theme::TEXT_HI)
                            .monospace()
                            .small(),
                    );
                });
            }
        });
}
