//! #41 — App-start "Resubmit N pending scans?" modal.
//!
//! Pops on app launch when [`scan_history::list_pending_older_than`]
//! returns one or more rows. Three choices: kick the resubmit
//! pipeline against all of them (the worker serialises), open the
//! History tab so the user can pick row-by-row, or dismiss for this
//! session (rows stay Pending — next launch will re-prompt).

#![cfg(all(feature = "gui", feature = "telemetry"))]

use egui::{Context, RichText, Window};

use crate::gui::theme;
use crate::scan_history::ScanRecord;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResubmitPromptChoice {
    /// Fire-and-forget every row in order. The resubmit worker
    /// serialises — only one POST in flight at a time — so the
    /// caller queues them and the worker drains.
    ResubmitAll,
    /// Switch the Results tab to History so the user can pick
    /// row-by-row. The modal dismisses.
    OpenHistory,
    /// Close the modal; rows stay Pending. Next launch will
    /// re-prompt (with one fewer row, presumably, if the user
    /// resubmitted any from History since).
    NotNow,
}

pub fn show(ctx: &Context, rows: &[ScanRecord]) -> Option<ResubmitPromptChoice> {
    let mut choice: Option<ResubmitPromptChoice> = None;
    Window::new(
        RichText::new(format!(
            "{} pending submission{} from prior session{}",
            rows.len(),
            if rows.len() == 1 { "" } else { "s" },
            if rows.len() == 1 { "" } else { "s" },
        ))
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
                "Some prior scans recorded a submittable payload but never \
                 made it to the leaderboard. The most likely reasons are a \
                 network blip at scan-end or the app exiting before submit.",
            )
            .color(theme::TEXT_HI),
        );
        ui.add_space(8.0);

        // Compact row preview. Cap at 5 so the modal doesn't get
        // oversized when a user comes back from a multi-week trip.
        let max_rows = 5;
        for r in rows.iter().take(max_rows) {
            ui.label(
                RichText::new(format!(
                    "• {} — {} root(s), {} group(s), channel {}",
                    super::scan_history_panel::format_unix_local(r.started_at_unix),
                    r.roots.len(),
                    r.total_dups,
                    r.submission_channel.as_deref().unwrap_or(&r.channel),
                ))
                .color(theme::TEXT_LO)
                .small()
                .monospace(),
            );
        }
        if rows.len() > max_rows {
            ui.label(
                RichText::new(format!("  + {} more…", rows.len() - max_rows))
                    .color(theme::TEXT_LO)
                    .small(),
            );
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Resubmit replays the canonical payload captured at scan time \
                 — no live re-detection of hardware or counts. Cross-install \
                 / cross-channel rows surface a clear error per row.",
            )
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            let resubmit_btn = egui::Button::new(
                RichText::new("↻  Resubmit all")
                    .color(theme::PANEL_DEEP)
                    .strong(),
            )
            .fill(theme::ACCENT)
            .min_size(egui::vec2(160.0, 32.0));
            if ui
                .add(resubmit_btn)
                .on_hover_text(
                    "Queue every listed row through the resubmit worker. \
                     Worker handles them one at a time; check the History tab \
                     for per-row outcomes.",
                )
                .clicked()
            {
                choice = Some(ResubmitPromptChoice::ResubmitAll);
            }
            let history_btn =
                egui::Button::new(RichText::new("📂  Open History").color(theme::TEXT_HI))
                    .min_size(egui::vec2(160.0, 32.0));
            if ui
                .add(history_btn)
                .on_hover_text("Switch to the History tab and pick rows individually.")
                .clicked()
            {
                choice = Some(ResubmitPromptChoice::OpenHistory);
            }
            let not_now_btn = egui::Button::new(RichText::new("Not now").color(theme::TEXT_HI))
                .min_size(egui::vec2(100.0, 32.0));
            if ui
                .add(not_now_btn)
                .on_hover_text("Dismiss for this session; the prompt fires again next launch.")
                .clicked()
            {
                choice = Some(ResubmitPromptChoice::NotNow);
            }
        });
    });
    choice
}
