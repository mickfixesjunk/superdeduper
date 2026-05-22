//! Spinner-modal that overlays the GUI while a long-running user-
//! requested action is processing on a background thread.
//!
//! Triggered by `EngineEvent::ActionStarted` (state.action_in_progress
//! becomes `Some`); dismissed by `EngineEvent::ActionFinished`. The
//! modal is non-cancellable for now — actions like safe-rename and
//! archive write through cleanly and pause/abort semantics for
//! destructive ops are a separate UX problem. The spinner just lets
//! the user know the GUI isn't frozen.

use egui::{Align2, Color32, RichText};

use crate::gui::state::ActionState;
use crate::gui::theme;

/// Render the modal. Returns `true` if the Cancel button was
/// clicked this frame so the caller can set the shared cancel flag
/// (worker threads check it between items and bail). Caller is
/// responsible for short-circuiting / not-routing interaction to
/// the rest of the UI while the modal is up (the dim layer
/// communicates this visually).
pub fn show(ctx: &egui::Context, action: &ActionState) -> bool {
    // Dim the rest of the UI so the modal reads as foreground and
    // accidental clicks behind it don't fire.
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Background,
        egui::Id::new("action-progress-dim"),
    ));
    let screen = ctx.screen_rect();
    painter.rect_filled(screen, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 130));

    let mut cancelled = false;
    egui::Window::new(&action.name)
        .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .default_width(440.0)
        .show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(28.0));
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    // Count line: "done / total" if total is known,
                    // otherwise "done processed" so the indeterminate
                    // case still tells the user something is moving.
                    let counter = match action.total {
                        Some(total) => format!("{} / {}", action.done, total),
                        None => format!("{} processed", action.done),
                    };
                    ui.label(
                        RichText::new(counter)
                            .color(theme::TEXT_HI)
                            .strong()
                            .size(15.0),
                    );
                    // Determinate progress bar when total is known.
                    if let Some(total) = action.total {
                        let frac = if total == 0 {
                            0.0
                        } else {
                            (action.done as f32 / total as f32).clamp(0.0, 1.0)
                        };
                        ui.add(
                            egui::ProgressBar::new(frac)
                                .desired_width(360.0)
                                .desired_height(8.0),
                        );
                    }
                });
            });
            ui.add_space(8.0);
            // Current item being processed — short enough to keep
            // the modal a fixed width. Truncate path-style strings
            // from the left so the filename stays visible.
            if let Some(current) = &action.current {
                let display = truncate_path_left(current, 60);
                ui.label(
                    RichText::new(display)
                        .color(theme::TEXT_LO)
                        .monospace()
                        .small(),
                );
            } else {
                // Reserve the slot so the modal doesn't grow/shrink
                // as items appear and disappear.
                ui.label(RichText::new(" ").color(theme::TEXT_LO).small());
            }
            ui.add_space(8.0);
            // Stop button: signals the shared cancel atomic so the
            // worker bails after the current item finishes (we don't
            // interrupt an in-flight file rename / move / hardlink
            // mid-syscall — that'd leave a half-done state on disk).
            ui.horizontal(|ui| {
                if ui
                    .button(RichText::new("⏹ Stop").color(theme::HOT).strong())
                    .on_hover_text(
                        "Cancel the rest of this action. The current file finishes \
                         processing — we don't interrupt mid-operation — but no further \
                         files are touched. Already-processed files keep their changes \
                         (safe-renamed files stay renamed, archived files stay archived).",
                    )
                    .clicked()
                {
                    cancelled = true;
                }
                ui.label(
                    RichText::new("(stops after the current file)")
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                );
            });
        });
    cancelled
}

/// Show the LAST `max_len` chars of a path-like string, with a
/// leading `…` if truncation happened. Keeps the filename visible
/// (which is the part the user cares about) when the parent
/// directory is too long for the modal.
fn truncate_path_left(s: &str, max_len: usize) -> String {
    if s.chars().count() <= max_len {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(max_len - 1)..]
        .iter()
        .collect();
    format!("…{tail}")
}
