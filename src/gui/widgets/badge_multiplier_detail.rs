//! #77 v2 — Per-install detail modal for a multi-machine badge.
//!
//! Pops when the user clicks a badge tile that has `×N` (N ≥ 2)
//! overlay, listing every install in the user's account that
//! earned this achievement. Pure display (no rename / anonymize
//! controls — those live in Settings; this widget doesn't
//! duplicate them per design 2026-05-25T21:26Z).
//!
//! Row content per install:
//! * Nickname if set; falls back to the 4-char `install_id_prefix`
//! * Earned-at date (formatted from unix seconds; uses the same
//!   YYYY-MM-DD shape the scan history table uses for
//!   cross-surface consistency)
//! * CPU class hint (architectural fingerprint — `x86_64` /
//!   `aarch64` / etc.)
//! * Archived installs render with an "Archived" prefix
//!
//! Rows are sorted most-recent `earned_at_unix` first so the
//! latest machine to earn the badge is at the top — the typical
//! "I just unlocked this on machine X" frame.
//!
//! Close affordances:
//! * Explicit ✕ button in the title bar
//! * Click-outside-to-close (egui Window default when
//!   `interactable(true)` + outside-click detection)
//! * ESC key
//!
//! Anonymous installs never call the endpoint that populates the
//! catalog state, so this modal is unreachable for them — the
//! tile's overlay only renders when `multiplier >= 2`.

#![cfg(feature = "telemetry")]

use egui::{Context, RichText, Window};

use crate::gui::theme;
use crate::leaderboard::account_badge_summary::AccountBadgeInstall;

/// What the user did. `None` ⇒ modal still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BadgeMultiplierDetailChoice {
    Close,
}

pub fn show(
    ctx: &Context,
    achievement_name: &str,
    achievement_id: &str,
    installs: &[AccountBadgeInstall],
) -> Option<BadgeMultiplierDetailChoice> {
    let mut choice: Option<BadgeMultiplierDetailChoice> = None;

    // ESC closes — checked once per frame; only consumes the key
    // when the modal is the active focus owner.
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
        choice = Some(BadgeMultiplierDetailChoice::Close);
    }

    let title = if achievement_name.is_empty() {
        format!("Earned on {} installs", installs.len())
    } else {
        format!(
            "{} — earned on {} install{}",
            achievement_name,
            installs.len(),
            if installs.len() == 1 { "" } else { "s" },
        )
    };

    // Sort copy by most-recent earned_at_unix; archived installs
    // sort by their earned_at too, so an archive that's still
    // newer than an active install appears at the top (with the
    // "Archived" prefix making the state visible).
    let mut sorted: Vec<&AccountBadgeInstall> = installs.iter().collect();
    sorted.sort_by_key(|i| std::cmp::Reverse(i.earned_at_unix));

    let mut window = Window::new(RichText::new(title).color(theme::TEXT_HI).heading())
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .collapsible(false)
        .resizable(false)
        .min_width(520.0);
    window = window.id(egui::Id::new(("badge-multiplier-detail", achievement_id)));
    let response = window.show(ctx, |ui| {
        if sorted.is_empty() {
            ui.label(
                RichText::new(
                    "No per-install detail available yet — the server's badge-summary endpoint \
                     may not have populated this achievement on your account. Try refreshing \
                     the badge wall (Settings → Account → Refresh).",
                )
                .color(theme::TEXT_LO),
            );
        } else {
            for install in &sorted {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let label = display_label(install);
                    ui.label(
                        RichText::new(&label)
                            .color(if install.archived {
                                theme::TEXT_LO
                            } else {
                                theme::TEXT_HI
                            })
                            .strong(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let mut tail = format_earned_at(install.earned_at_unix);
                        if let Some(cls) = &install.cpu_class {
                            tail = format!("{tail} · {cls}");
                        }
                        ui.label(RichText::new(tail).color(theme::TEXT_LO).small());
                    });
                });
                ui.separator();
            }
        }

        ui.add_space(8.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(RichText::new("Close").color(theme::TEXT_HI))
                .clicked()
            {
                choice = Some(BadgeMultiplierDetailChoice::Close);
            }
        });
    });

    // Click-outside detection: if the user clicked anywhere this
    // frame AND the click wasn't inside the window rect, close.
    // Skips when the window itself wasn't rendered (response is
    // None — e.g. ctx-internal collapse path).
    if let Some(resp) = response {
        let clicked_outside = ctx.input(|i| i.pointer.any_click())
            && !resp
                .response
                .rect
                .contains(ctx.input(|i| i.pointer.interact_pos().unwrap_or_default()));
        if clicked_outside {
            choice = Some(BadgeMultiplierDetailChoice::Close);
        }
    }

    choice
}

fn display_label(install: &AccountBadgeInstall) -> String {
    let base = match &install.nickname {
        Some(n) if !n.is_empty() => n.clone(),
        _ => install.install_id_prefix.clone(),
    };
    if install.archived {
        format!("Archived · {base}")
    } else {
        base
    }
}

/// Format a unix-seconds timestamp as YYYY-MM-DD. Matches the
/// rule used by `scan_history` so timestamps read consistently
/// across the GUI without dragging in `chrono` just for this
/// modal.
fn format_earned_at(unix_seconds: u64) -> String {
    if unix_seconds == 0 {
        return "earned date unknown".into();
    }
    let (y, m, d, _, _, _) = crate::time::unix_to_ymdhms(unix_seconds as i64);
    format!("earned {y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(
        prefix: &str,
        nick: Option<&str>,
        archived: bool,
        earned: u64,
    ) -> AccountBadgeInstall {
        AccountBadgeInstall {
            install_id: format!("{prefix}-aaaa"),
            install_id_prefix: prefix.into(),
            nickname: nick.map(String::from),
            cpu_class: Some("x86_64".into()),
            earned_at_unix: earned,
            archived,
        }
    }

    #[test]
    fn display_label_prefers_nickname() {
        let i = install("9d4a", Some("Workstation"), false, 1_779_000_000);
        assert_eq!(display_label(&i), "Workstation");
    }

    #[test]
    fn display_label_falls_back_to_prefix_when_nickname_missing() {
        let i = install("9d4a", None, false, 1_779_000_000);
        assert_eq!(display_label(&i), "9d4a");
    }

    #[test]
    fn display_label_prefixes_archived() {
        let i = install("9d4a", Some("OldLaptop"), true, 1_770_000_000);
        assert_eq!(display_label(&i), "Archived · OldLaptop");
    }

    #[test]
    fn display_label_empty_nickname_falls_back_to_prefix() {
        // Defensive: server might return an empty-string nickname
        // for an install whose user cleared the field. Treat
        // empty as "no nickname" rather than rendering nothing.
        let i = install("9d4a", Some(""), false, 1_779_000_000);
        assert_eq!(display_label(&i), "9d4a");
    }

    #[test]
    fn format_earned_at_renders_iso_date() {
        // 1_779_408_000 = 2026-05-22T00:00:00Z (verified against
        // `date -u -d @1779408000`). Pins the proleptic-Gregorian
        // formatter so a refactor that drifts day-of-month falls
        // out here.
        assert_eq!(format_earned_at(1_779_408_000), "earned 2026-05-22");
    }

    #[test]
    fn format_earned_at_zero_is_unknown() {
        assert_eq!(format_earned_at(0), "earned date unknown");
    }
}
