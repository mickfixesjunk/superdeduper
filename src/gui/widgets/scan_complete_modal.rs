//! Post-scan leaderboard modal — the "dopamine moment" per client
//! spec §10.1.
//!
//! Triggered when a scan finishes (engine emits `ScanFinished`) and
//! the user's share preference is `AlwaysAsk`. The modal surfaces the
//! scan's headline stats + four actions (Submit / Skip / Auto-submit
//! going forward / What gets shared?). On Submit a worker thread
//! POSTs the leaderboard payload; outcome (rank + achievement-unlock
//! lines) renders in-place once the response lands.
//!
//! State machine:
//!
//!     Hidden ──ScanFinished + AlwaysAsk──► Ready
//!     Ready  ──Submit / AutoSubmit──────► Submitting ──response──► Done
//!     Ready  ──Skip─────────────────────► Hidden
//!     Done   ──Close────────────────────► Hidden
//!     {Ready,Done} ──OpenPreview────────► Preview ──ClosePreview──► back
//!
//! AutoOptIn share preference bypasses the modal — submission happens
//! silently in the background with a brief status-line toast (handled
//! by `app::drain_events`). `Never` skips both.

#![cfg(feature = "telemetry")]

use egui::{Context, RichText, Window};

use crate::gui::theme;
use crate::leaderboard::submission::SubmitOutcome;

#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCompleteState {
    #[default]
    Hidden,
    /// Modal visible; user is reading the stats + choosing an action.
    Ready,
    /// Submit worker in flight; spinner + "Submitting…" line.
    Submitting,
    /// Submit returned; rank + achievements rendered; Close button.
    Done,
    /// "What gets shared?" sub-modal open over the parent modal.
    Preview,
}

/// Snapshot of the scan's headline stats, captured once at scan-end
/// so the modal renders consistent values even if downstream UI state
/// mutates while the modal is open.
#[derive(Debug, Clone)]
pub struct ScanCompleteData {
    pub elapsed_seconds: f32,
    pub reclaimable_bytes: u64,
    pub files_scanned: u64,
    pub bytes_read: u64,
    pub duplicate_groups: u64,
    /// Effective throughput: bytes_read / elapsed_seconds, rounded to
    /// MB/s. Matches the headline number from the spec mockup.
    pub throughput_mbps: f32,
}

impl ScanCompleteData {
    pub fn from_engine_event(
        elapsed_secs: f32,
        reclaimable_bytes: u64,
        files_scanned: u64,
        bytes_read: u64,
        duplicate_groups: u64,
    ) -> Self {
        let throughput_mbps = if elapsed_secs > 0.0 {
            (bytes_read as f32 / 1_000_000.0) / elapsed_secs
        } else {
            0.0
        };
        Self {
            elapsed_seconds: elapsed_secs,
            reclaimable_bytes,
            files_scanned,
            bytes_read,
            duplicate_groups,
            throughput_mbps,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanCompleteAction {
    /// User clicked [Submit to leaderboard]. Caller spawns the
    /// submit worker + transitions to Submitting.
    Submit,
    /// User clicked [Skip this time]. Caller transitions to Hidden;
    /// no submission attempted.
    Skip,
    /// User clicked [Auto-submit going forward]. Caller flips
    /// `ShareDefault` to `AutoOptIn` AND submits this run now.
    AutoSubmit,
    /// User clicked [What gets shared?] → caller transitions to
    /// Preview.
    OpenPreview,
    /// User dismissed the Preview sub-modal.
    ClosePreview,
    /// User clicked Close on the Done state.
    Close,
}

/// Render the modal. `state` drives which content variant renders;
/// `data` is the snapshotted stats; `outcome` is `Some` only in the
/// `Done` state (the worker's response). `payload_preview` is the
/// canonical JSON body the engine would POST; used by the Preview
/// sub-modal.
pub fn show(
    ctx: &Context,
    state: ScanCompleteState,
    data: &ScanCompleteData,
    outcome: Option<&SubmitOutcome>,
    payload_preview: Option<&str>,
) -> Option<ScanCompleteAction> {
    if matches!(state, ScanCompleteState::Hidden) {
        return None;
    }

    if matches!(state, ScanCompleteState::Preview) {
        return render_preview(ctx, payload_preview);
    }

    let mut action: Option<ScanCompleteAction> = None;
    Window::new(
        RichText::new("Scan complete")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
    .default_width(540.0)
    .show(ctx, |ui| {
        render_stats(ui, data);
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);

        match state {
            ScanCompleteState::Ready => {
                render_action_buttons(ui, &mut action);
            }
            ScanCompleteState::Submitting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new("Submitting to leaderboard…")
                            .color(theme::TEXT_HI),
                    );
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("(POST to api.superdeduper.io · 15s timeout)")
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                );
            }
            ScanCompleteState::Done => {
                if let Some(outcome) = outcome {
                    render_outcome(ui, outcome);
                }
                ui.add_space(10.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Close")
                                .color(theme::PANEL_DEEP)
                                .strong(),
                        )
                        .fill(theme::ACCENT)
                        .min_size(egui::vec2(120.0, 28.0)),
                    )
                    .clicked()
                {
                    action = Some(ScanCompleteAction::Close);
                }
            }
            ScanCompleteState::Hidden | ScanCompleteState::Preview => {}
        }
    });
    action
}

fn render_stats(ui: &mut egui::Ui, d: &ScanCompleteData) {
    ui.label(
        RichText::new(format!("{:.1}s wall-clock", d.elapsed_seconds))
            .color(theme::TEXT_LO)
            .small(),
    );
    ui.add_space(8.0);

    egui::Grid::new("scan_complete_stats")
        .num_columns(2)
        .spacing([16.0, 6.0])
        .show(ui, |ui| {
            big_stat_row(ui, "Reclaimable", &theme::humansize(d.reclaimable_bytes), theme::HOT);
            ui.end_row();
            big_stat_row(
                ui,
                "Files scanned",
                &format_thousands(d.files_scanned),
                theme::ACCENT,
            );
            ui.end_row();
            big_stat_row(
                ui,
                "Throughput",
                &format!("{:.0} MB/s", d.throughput_mbps),
                theme::COOL,
            );
            ui.end_row();
            big_stat_row(
                ui,
                "Duplicate groups",
                &format_thousands(d.duplicate_groups),
                theme::TEXT_HI,
            );
            ui.end_row();
        });
}

fn big_stat_row(ui: &mut egui::Ui, label: &str, value: &str, color: egui::Color32) {
    ui.label(RichText::new(label).color(theme::TEXT_LO));
    ui.label(RichText::new(value).color(color).strong().size(18.0));
}

fn render_action_buttons(ui: &mut egui::Ui, action: &mut Option<ScanCompleteAction>) {
    ui.horizontal_wrapped(|ui| {
        if ui
            .add(
                egui::Button::new(
                    RichText::new("Submit to leaderboard")
                        .color(theme::PANEL_DEEP)
                        .strong(),
                )
                .fill(theme::ACCENT)
                .min_size(egui::vec2(190.0, 30.0)),
            )
            .on_hover_text(
                "POST signed payload to api.superdeduper.io/api/v1/submit. \
                 Failed submissions queue for retry on next launch.",
            )
            .clicked()
        {
            *action = Some(ScanCompleteAction::Submit);
        }
        ui.add_space(4.0);
        if ui
            .add(
                egui::Button::new(RichText::new("Skip this time").color(theme::TEXT_HI))
                    .min_size(egui::vec2(120.0, 30.0)),
            )
            .clicked()
        {
            *action = Some(ScanCompleteAction::Skip);
        }
        ui.add_space(4.0);
        if ui
            .add(
                egui::Button::new(
                    RichText::new("Auto-submit going forward").color(theme::TEXT_HI),
                )
                .min_size(egui::vec2(190.0, 30.0)),
            )
            .on_hover_text(
                "Submit this run AND flip your share preference to \
                 auto-submit. Future scans will submit silently with a \
                 brief toast; can be reverted in Settings > Leaderboard.",
            )
            .clicked()
        {
            *action = Some(ScanCompleteAction::AutoSubmit);
        }
    });
    ui.add_space(6.0);
    if ui
        .link(
            RichText::new("What gets shared?")
                .color(theme::TEXT_LO)
                .underline(),
        )
        .clicked()
    {
        *action = Some(ScanCompleteAction::OpenPreview);
    }
}

fn render_outcome(ui: &mut egui::Ui, outcome: &SubmitOutcome) {
    match outcome {
        SubmitOutcome::Accepted {
            ranks,
            achievements_unlocked,
            profile_url,
            ..
        } => {
            ui.label(
                RichText::new("✓ Submitted")
                    .color(theme::ACCENT)
                    .strong()
                    .size(15.0),
            );
            ui.add_space(6.0);
            if ranks.is_empty() {
                ui.label(
                    RichText::new("Awaiting rank computation…")
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                );
            } else {
                for r in ranks {
                    ui.label(
                        RichText::new(format!(
                            "  Rank #{} of {} ({} / {})",
                            r.rank, r.bucket_size, r.category, r.bracket,
                        ))
                        .color(theme::TEXT_HI),
                    );
                }
            }
            for a in achievements_unlocked {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("  🏆 {}", a))
                        .color(theme::ACCENT)
                        .strong(),
                );
            }
            if let Some(url) = profile_url {
                ui.add_space(6.0);
                ui.hyperlink_to(
                    RichText::new("View profile →").color(theme::ACCENT),
                    url,
                );
            }
        }
        SubmitOutcome::DuplicateNoChange => {
            ui.label(
                RichText::new("Already submitted (no change)")
                    .color(theme::TEXT_LO),
            );
        }
        SubmitOutcome::Rejected { status, reason } => {
            ui.label(
                RichText::new(format!("✗ Rejected ({status})"))
                    .color(theme::HOT)
                    .strong(),
            );
            ui.label(
                RichText::new(reason).color(theme::TEXT_LO).small(),
            );
        }
        SubmitOutcome::Transient { reason } => {
            ui.label(
                RichText::new("Network failure — queued for retry on next launch")
                    .color(theme::WARN),
            );
            ui.label(
                RichText::new(reason).color(theme::TEXT_LO).small(),
            );
        }
    }
}

fn render_preview(ctx: &Context, payload: Option<&str>) -> Option<ScanCompleteAction> {
    let mut action: Option<ScanCompleteAction> = None;
    Window::new(
        RichText::new("What gets shared")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(true)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .default_size(egui::vec2(620.0, 480.0))
    .show(ctx, |ui| {
        ui.label(
            RichText::new(
                "Exact JSON sent to api.superdeduper.io. No usernames, no \
                 file paths, no IPs, no machine names — just hardware bracket, \
                 scan totals, and an opaque corpus signature.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
        ui.add_space(6.0);
        ui.hyperlink_to(
            RichText::new("Privacy policy").color(theme::ACCENT).small(),
            "https://superdeduper.io/privacy/",
        );
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(8.0);

        let body = payload.unwrap_or("(payload not yet built — run a scan first)");
        egui::ScrollArea::vertical()
            .max_height(340.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut body.to_string())
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(20)
                        .interactive(false),
                );
            });

        ui.add_space(10.0);
        if ui
            .add(
                egui::Button::new(
                    RichText::new("Close").color(theme::PANEL_DEEP).strong(),
                )
                .fill(theme::ACCENT)
                .min_size(egui::vec2(120.0, 28.0)),
            )
            .clicked()
        {
            action = Some(ScanCompleteAction::ClosePreview);
        }
    });
    action
}

fn format_thousands(n: u64) -> String {
    let s = n.to_string();
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_thousands_handles_small_and_large() {
        assert_eq!(format_thousands(0), "0");
        assert_eq!(format_thousands(42), "42");
        assert_eq!(format_thousands(999), "999");
        assert_eq!(format_thousands(1_000), "1,000");
        assert_eq!(format_thousands(412_998), "412,998");
        assert_eq!(format_thousands(1_234_567_890), "1,234,567,890");
    }

    #[test]
    fn throughput_division_safe_at_zero_elapsed() {
        let d = ScanCompleteData::from_engine_event(0.0, 0, 0, 1234, 0);
        assert_eq!(d.throughput_mbps, 0.0);
    }

    #[test]
    fn throughput_matches_expected() {
        // 100 MB read in 1s = 100 MB/s
        let d = ScanCompleteData::from_engine_event(1.0, 0, 0, 100_000_000, 0);
        assert!((d.throughput_mbps - 100.0).abs() < 0.01);
    }
}
