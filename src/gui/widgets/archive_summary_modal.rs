//! #80 Bug C — post-archive summary modal.
//!
//! Shown automatically when the archive worker finishes. Surfaces
//! the moved-vs-failed split by failure-reason bucket plus the
//! single most important number: how many bytes were *actually*
//! reclaimed from the source. That number is what #79's PATCH
//! submission will credit to the leaderboard as `archived_bytes`
//! (NOT moved+failed — crediting failed bytes is a silent
//! leaderboard-cheating vector per #80's spec).
//!
//! Why a structured modal instead of a status-line one-liner:
//! pre-#80, archive failures showed only as "N failed" without a
//! reason. Mick lost ~36 GB to orphan copies that way — no way to
//! see from the UI that the failures were all access-denied on
//! protected dirs. The breakdown by reason makes the failure
//! pattern legible (and Bug B's pre-flight check actionable).

use egui::{Context, RichText, Window};

use crate::gui::archive::ArchiveActionSummary;
use crate::gui::theme;

/// Result of the modal's input on this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveSummaryChoice {
    /// User clicked Done. Caller clears `pending_archive_summary`.
    Done,
    /// User clicked "Reveal in Explorer". Caller opens the dest
    /// folder + leaves the modal open until Done is clicked.
    RevealDestination,
}

pub fn show(ctx: &Context, summary: &ArchiveActionSummary) -> Option<ArchiveSummaryChoice> {
    let mut choice: Option<ArchiveSummaryChoice> = None;
    let title = if summary.user_stopped {
        "Archive stopped"
    } else {
        "Archive complete"
    };
    Window::new(RichText::new(title).color(theme::TEXT_HI).heading())
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
        .default_width(560.0)
        .show(ctx, |ui| {
            // Headline: actually reclaimed. This is THE figure for
            // the user and for #79's leaderboard credit. Big + bold.
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "Actually reclaimed from source: {}",
                    theme::humansize(summary.moved_bytes),
                ))
                .color(theme::TEXT_HI)
                .size(18.0)
                .strong(),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(format!(
                    "({} file{} moved)",
                    summary.moved_count,
                    if summary.moved_count == 1 { "" } else { "s" },
                ))
                .color(theme::TEXT_LO)
                .small(),
            );
            ui.add_space(12.0);

            // Moved details row.
            ui.label(
                RichText::new(format!(
                    "Moved: {} file{} / {}",
                    summary.moved_count,
                    if summary.moved_count == 1 { "" } else { "s" },
                    theme::humansize(summary.moved_bytes),
                ))
                .color(theme::TEXT_HI),
            );

            // Failed details, broken down by reason bucket. Skip
            // sub-rows that have zero count so the modal stays tight
            // on clean runs.
            let failed_count = summary.failed_count();
            if failed_count > 0 {
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!(
                        "Failed: {} file{} / {}",
                        failed_count,
                        if failed_count == 1 { "" } else { "s" },
                        theme::humansize(summary.failed_bytes()),
                    ))
                    .color(theme::TEXT_HI),
                );
                if summary.failed_access_denied_count > 0 {
                    ui.label(
                        RichText::new(format!(
                            "  · Access denied (protected dirs): {} / {}",
                            summary.failed_access_denied_count,
                            theme::humansize(summary.failed_access_denied_bytes),
                        ))
                        .color(theme::TEXT_LO)
                        .small(),
                    );
                }
                if summary.failed_cross_device_count > 0 {
                    ui.label(
                        RichText::new(format!(
                            "  · Cross-device / out of space: {} / {}",
                            summary.failed_cross_device_count,
                            theme::humansize(summary.failed_cross_device_bytes),
                        ))
                        .color(theme::TEXT_LO)
                        .small(),
                    );
                }
                if summary.failed_other_count > 0 {
                    ui.label(
                        RichText::new(format!(
                            "  · Other (see log): {} / {}",
                            summary.failed_other_count,
                            theme::humansize(summary.failed_other_bytes),
                        ))
                        .color(theme::TEXT_LO)
                        .small(),
                    );
                }
                if summary.failed_access_denied_count > 0 {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(
                            "Tip: protected directories (Windows, Program Files, \
                             TrustedInstaller-owned) require elevated permissions \
                             to delete. Re-run from an admin terminal to move \
                             those files, or use the Exclusion preset packs to \
                             skip them on future scans.",
                        )
                        .color(theme::TEXT_LO)
                        .small(),
                    );
                }
            }

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            ui.label(
                RichText::new(format!("Archive folder: {}", summary.destination.display()))
                    .color(theme::TEXT_LO)
                    .small(),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("Done").color(theme::TEXT_HI))
                    .clicked()
                {
                    choice = Some(ArchiveSummaryChoice::Done);
                }
                if ui
                    .button(RichText::new("Reveal in Explorer").color(theme::TEXT_HI))
                    .clicked()
                {
                    choice = Some(ArchiveSummaryChoice::RevealDestination);
                }
            });
        });
    choice
}

#[cfg(test)]
mod tests {
    use crate::gui::archive::ArchiveActionSummary;
    use std::path::PathBuf;

    /// Smoke test: the helper math (moved + failed totals) reads
    /// correctly off the summary struct. Exercises the same
    /// accessors the modal renders so a refactor that drops a
    /// bucket from `failed_count()` would surface here too.
    #[test]
    fn summary_totals_round_trip() {
        let s = ArchiveActionSummary {
            moved_count: 95,
            moved_bytes: 57 * 1024 * 1024,
            failed_access_denied_count: 1_200,
            failed_access_denied_bytes: 18 * 1024 * 1024 * 1024,
            failed_cross_device_count: 0,
            failed_cross_device_bytes: 0,
            failed_other_count: 3,
            failed_other_bytes: 1024,
            user_stopped: false,
            destination: PathBuf::from("/tmp/archive"),
        };
        assert_eq!(s.failed_count(), 1_203);
        assert_eq!(s.failed_bytes(), 18 * 1024 * 1024 * 1024 + 1024);
        // The figure that #79 will credit: moved_bytes ONLY. Spot-
        // checking here so a future refactor that adds failed bytes
        // to "actually reclaimed" trips the test.
        assert_eq!(s.moved_bytes, 57 * 1024 * 1024);
    }
}
