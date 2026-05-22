//! Alpha-software warning modal shown on startup.
//!
//! Renders a dismissable modal with a brief safety message. Once
//! the user clicks "Got it — don't show again", the persisted
//! setting `dismissed_alpha_warning` flips to `true` and the modal
//! never reappears (per user, per persisted-config scope).
//!
//! Shown on every launch by default — the warning is cheap, but
//! the consequence of a confused user bulk-deleting is not. Only
//! suppressed after explicit dismissal.

use egui::{Align2, RichText};

use crate::gui::theme;

/// Returned to the caller so it can flip the persisted setting and
/// release the modal on the next frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AlphaWarningChoice {
    /// User clicked "Continue" — close the modal for this session
    /// only. Warning reappears on next launch.
    AcknowledgeOnce,
    /// User clicked "Got it — don't show again" — close AND
    /// persist so it never shows again unless settings reset.
    AcknowledgeForever,
}

/// Render the modal. Returns `Some(choice)` when the user clicks a
/// dismiss button; `None` while the modal is still open. The
/// caller is expected to early-return from its frame render after
/// receiving the choice so the modal disappears cleanly.
pub fn show(ctx: &egui::Context) -> Option<AlphaWarningChoice> {
    // Dim the rest of the UI so the modal reads as foreground.
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("alpha-warning-dim"),
    ));
    let screen = ctx.screen_rect();
    painter.rect_filled(
        screen,
        0.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 140),
    );

    let mut choice: Option<AlphaWarningChoice> = None;
    egui::Window::new("⚠  Alpha software — please read before using")
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .default_width(560.0)
        .show(ctx, |ui| {
            ui.add_space(4.0);
            ui.label(
                RichText::new("superdeduper is alpha-quality software.")
                    .color(theme::TEXT_HI)
                    .strong()
                    .size(15.0),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new(
                    "It performs destructive operations on your files \
                 (Recycle, hardlink replacement, archive move, \
                 safe-rename). Defaults are reversible — but bugs \
                 in this codebase CAN result in permanent data loss.",
                )
                .color(theme::TEXT_HI),
            );
            ui.add_space(8.0);
            ui.label(
                RichText::new("Before pointing it at important data:")
                    .color(theme::TEXT_HI)
                    .strong(),
            );
            ui.add_space(2.0);
            for bullet in [
                "Back up first. Test on copies, not originals.",
                "Read every destructive action's confirmation modal.",
                "Verify the keeper file the smart-keep heuristic picks — \
                 it can be wrong for your use case.",
                "Don't run on system folders (--allow-system-paths is \
                 off for a reason).",
            ] {
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("•").color(theme::ACCENT));
                    ui.label(RichText::new(bullet).color(theme::TEXT_HI));
                });
            }
            ui.add_space(10.0);
            ui.label(
                RichText::new(
                    "This is a personal project shared as-is. The authors accept no \
                               responsibility for lost data.",
                )
                .color(theme::TEXT_LO)
                .italics(),
            );
            ui.add_space(14.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("Continue").color(theme::TEXT_HI))
                    .clicked()
                {
                    choice = Some(AlphaWarningChoice::AcknowledgeOnce);
                }
                if ui
                    .button(
                        RichText::new("Got it — don't show again")
                            .color(theme::TEXT_HI)
                            .strong(),
                    )
                    .clicked()
                {
                    choice = Some(AlphaWarningChoice::AcknowledgeForever);
                }
            });
        });
    choice
}
