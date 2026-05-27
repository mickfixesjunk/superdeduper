//! Launch-time "Resume previous scan or start fresh?" modal.
//!
//! Shown only when a usable scan checkpoint exists on disk at startup.
//! The modal is intentionally blocking so the user has to pick before
//! the rest of the UI becomes interactive — that's the contract that
//! lets "Start Fresh" safely clear in-memory state without surprising
//! the user mid-action.

use std::time::{SystemTime, UNIX_EPOCH};

use egui::{Context, RichText, Window};

use crate::gui::checkpoint::CheckpointSummary;
use crate::gui::resume_tier::ResumeTier;
use crate::gui::theme;
use crate::path_display::for_user_display;

/// What the user picked. `None` ⇒ keep showing the modal until they
/// pick something; the rest of the UI stays disabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeChoice {
    Resume,
    StartFresh,
}

pub fn show(ctx: &Context, summary: &CheckpointSummary, tier: ResumeTier) -> Option<ResumeChoice> {
    let mut choice: Option<ResumeChoice> = None;
    Window::new(
        RichText::new("Previous scan found")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, -40.0])
    .default_width(520.0)
    .show(ctx, |ui| {
        let when = human_time_since(summary.created_at_unix);
        ui.label(
            RichText::new(format!("From {when}."))
                .color(theme::TEXT_HI)
                .size(14.0),
        );
        ui.add_space(4.0);
        ui.label(
            RichText::new(format!(
                "{} duplicate group(s) already confirmed · {} root(s){}",
                summary.duplicate_count,
                summary.roots.len(),
                if summary.has_saved_inventory {
                    " · file inventory saved (Stage 1 will be skipped)"
                } else {
                    " · inventory not yet saved"
                }
            ))
            .color(theme::TEXT_LO)
            .small(),
        );
        ui.add_space(2.0);
        // Show the roots so the user knows what they're committing
        // to. Cap at 4 to avoid an oversized modal on big root sets.
        for (i, r) in summary.roots.iter().take(4).enumerate() {
            let star = if r.is_reference { "★ " } else { "  " };
            ui.label(
                RichText::new(format!("{star}{}", for_user_display(&r.path)))
                    .color(theme::TEXT_LO)
                    .monospace()
                    .small(),
            );
            let _ = i;
        }
        if summary.roots.len() > 4 {
            ui.label(
                RichText::new(format!("  + {} more…", summary.roots.len() - 4))
                    .color(theme::TEXT_LO)
                    .small(),
            );
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(8.0);

        // #99 PR2 — Tier-specific copy. Pre-#99 the modal always
        // said "Resume previous scan"; with the ResumeTier model,
        // we surface what's actually going to happen so the user
        // knows whether to expect a cold-cache re-validation pass,
        // a settings-rerun, or a clean full resume.
        let tier_blurb = match tier {
            ResumeTier::Full => {
                "All your prior work is intact — hash cache hot, settings unchanged. \
                 This will be a fast continuation."
            }
            ResumeTier::Warm => {
                "Engine was upgraded since the pause; hash cache had to be wiped. \
                 Your duplicate list will be restored, but files will be re-validated \
                 against current disk state (cache misses are expected)."
            }
            ResumeTier::InventoryOnly => {
                "Scan settings changed since the pause. Your file list is still good, \
                 but grouping + hashing will be redone under the new settings."
            }
            ResumeTier::Marker => {
                "The previous scan was paused before any duplicates were found. \
                 Resuming starts a fresh walk; nothing is restored."
            }
            ResumeTier::Fresh => {
                "Nothing from the previous run can be reused (roots don't match, \
                 or no usable state). Both buttons will start a fresh scan."
            }
        };
        ui.label(RichText::new(tier_blurb).color(theme::TEXT_HI).size(13.0));
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "Starting fresh keeps your hash cache and renames the previous \
                     progress file for safekeeping.",
            )
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
        ui.add_space(10.0);

        ui.horizontal(|ui| {
            // #99 PR2 — Resume button label tracks the tier so the
            // user knows what they're committing to. Fresh tier
            // shows a no-op-equivalent label since both buttons
            // produce the same outcome.
            let resume_label = tier.modal_label();
            let resume_btn = egui::Button::new(
                RichText::new(format!("▶  {resume_label}"))
                    .color(theme::PANEL_DEEP)
                    .strong(),
            )
            .fill(theme::ACCENT)
            .min_size(egui::vec2(280.0, 32.0));
            if ui.add(resume_btn).clicked() {
                choice = Some(ResumeChoice::Resume);
            }
            let fresh_btn =
                egui::Button::new(RichText::new("✨  Start fresh").color(theme::TEXT_HI))
                    .min_size(egui::vec2(140.0, 32.0));
            if ui
                .add(fresh_btn)
                .on_hover_text(
                    "Renames the prior checkpoint to a .bak sibling (never deletes it) \
                         and clears the GUI back to an empty roots panel. Your BLAKE3 / \
                         DDH-128 cache is preserved.",
                )
                .clicked()
            {
                choice = Some(ResumeChoice::StartFresh);
            }
        });
    });
    choice
}

/// Render a unix timestamp as a human phrase relative to now. Used
/// in the modal copy so a 30-day-old checkpoint reads "30 days ago"
/// instead of an ISO blob.
fn human_time_since(unix: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if unix > now {
        return "the future (clock skew)".into();
    }
    let delta = now - unix;
    if delta < 60 {
        return "just now".into();
    }
    if delta < 3_600 {
        return format!("{} minute(s) ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{} hour(s) ago", delta / 3_600);
    }
    let days = delta / 86_400;
    if days < 30 {
        return format!("{days} day(s) ago");
    }
    // Older than 30d: also include a calendar-ish stamp so stale
    // checkpoints look stale.
    format!("{days} days ago")
}
