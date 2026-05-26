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
        // Copy-to-clipboard button. Pulls every log entry in the
        // buffer (not just the visible 500), formats as plain text
        // with the same `info / warn / error` prefix the panel
        // shows, and dumps to the OS clipboard via egui's built-in
        // `output_mut`. Useful for Mick's repro-and-share flow
        // where pasting the engine log into a comms channel is the
        // diagnostic step.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button("📋 Copy")
                .on_hover_text(
                    "Copy every log entry to the clipboard so you can paste into a bug report",
                )
                .clicked()
            {
                let mut buf = String::with_capacity(state.logs.len() * 80);
                for entry in &state.logs {
                    let tag = match entry.level {
                        LogLevel::Info => "info ",
                        LogLevel::Warn => "warn ",
                        LogLevel::Error => "error",
                    };
                    buf.push_str(tag);
                    buf.push(' ');
                    buf.push_str(&entry.message);
                    buf.push('\n');
                }
                ui.ctx().copy_text(buf);
            }
        });
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

    // #104 Gap 2 — pin `resume diag:` lines above the rolling tail.
    // Pre-#104, the rolling 500-entry render plus the 1024-entry log
    // cap meant the 5–6 resume diagnostic emits at scan-start rolled
    // out of view within seconds, making the user unable to inspect
    // tier classification or cache state mid-scan. We now collect
    // those entries into a pinned non-scrolling section above the
    // scrollable region and exclude them from the rolling-tail to
    // avoid double-render.
    let resume_diag_entries: Vec<&crate::gui::state::LogEntry> = state
        .logs
        .iter()
        .filter(|e| e.message.starts_with("resume diag:"))
        .collect();
    if !resume_diag_entries.is_empty() {
        for entry in &resume_diag_entries {
            let (tag, color) = match entry.level {
                LogLevel::Info => ("info ", theme::TEXT_LO),
                LogLevel::Warn => ("warn ", theme::WARN),
                LogLevel::Error => ("error", theme::HOT),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new("📌").small());
                ui.label(RichText::new(tag).color(color).monospace().small().strong());
                ui.label(
                    RichText::new(&entry.message)
                        .color(theme::TEXT_HI)
                        .monospace()
                        .small(),
                );
            });
        }
        ui.separator();
    }

    // Collecting first because `.rev().take(500).rev()` requires
    // DoubleEndedIterator which `.filter()` doesn't preserve.
    let non_resume: Vec<&crate::gui::state::LogEntry> = state
        .logs
        .iter()
        .filter(|e| !e.message.starts_with("resume diag:"))
        .collect();
    let tail_start = non_resume.len().saturating_sub(500);
    ScrollArea::vertical()
        .id_salt("log-panel")
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for entry in &non_resume[tail_start..] {
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
