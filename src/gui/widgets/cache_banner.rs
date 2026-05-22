//! "Cached scan available" banner that appears above the main scan
//! controls when superdeduper has prior inventory snapshots for at
//! least one of the current scan roots' volumes.
//!
//! Non-blocking: rendering the banner is purely informational and
//! lets the user opt-out of cache reuse via the per-scan toggle.
//! Suppressed entirely when `settings.always_use_cache` is on (the
//! cache is silently reused).
//!
//! Cache existence is computed elsewhere — see
//! `App::refresh_cache_banner`. This widget only renders.

use egui::{Color32, Frame, RichText, Stroke, Ui};

use crate::gui::state::{CacheVolumeSummary, UiState};
use crate::gui::theme;

/// Render the banner if `state.cache_volume_summaries` is non-empty
/// AND `always_use_cache` is false. Toggles
/// `state.use_cache_for_next_scan` in-place when the checkbox is
/// flipped. No-op when the banner shouldn't show.
pub fn show(ui: &mut Ui, state: &mut UiState, always_use_cache: bool) {
    if always_use_cache || state.cache_volume_summaries.is_empty() {
        return;
    }

    let total_files: u64 = state
        .cache_volume_summaries
        .iter()
        .map(|s| s.record_count)
        .sum();
    let oldest = state
        .cache_volume_summaries
        .iter()
        .map(|s| s.captured_at_unix)
        .min();
    let age_text = oldest.map(human_age).unwrap_or_else(|| "earlier".into());
    let volume_count = state.cache_volume_summaries.len();

    // Muted-yellow banner: distinct enough to notice on first
    // glance, calm enough not to read as an error.
    let fill = Color32::from_rgba_unmultiplied(180, 150, 50, 26);
    let stroke = Color32::from_rgba_unmultiplied(180, 150, 50, 140);
    Frame::none()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .rounding(6.0)
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("🗂 Cached scan available")
                        .color(theme::TEXT_HI)
                        .strong(),
                );
                let detail = if volume_count == 1 {
                    format!(
                        " — {} files cached, captured {}.",
                        compact(total_files),
                        age_text
                    )
                } else {
                    format!(
                        " — {} files across {} volumes, oldest {}.",
                        compact(total_files),
                        volume_count,
                        age_text
                    )
                };
                ui.label(RichText::new(detail).color(theme::TEXT_HI));

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.checkbox(&mut state.use_cache_for_next_scan, "Use cache")
                        .on_hover_text(
                            "ON: next scan reuses the cached inventory and skips re-hashing \
                             files whose modified-time hasn't changed. \
                             OFF: scan from scratch (slower, but useful if you suspect the \
                             cache is stale).",
                        );
                });
            });
            // Per-volume detail list when there's more than one.
            if volume_count > 1 {
                ui.add_space(2.0);
                for s in &state.cache_volume_summaries {
                    let line = volume_detail_line(s);
                    ui.label(RichText::new(line).color(theme::TEXT_LO).small());
                }
            }
        });
    ui.add_space(4.0);
}

fn volume_detail_line(s: &CacheVolumeSummary) -> String {
    // The full GUID isn't user-friendly; show the short form.
    let short = s
        .volume_guid
        .trim_start_matches("\\\\?\\Volume{")
        .trim_end_matches("}\\")
        .chars()
        .take(8)
        .collect::<String>();
    format!(
        "  • volume {} — {} files, captured {}",
        short,
        compact(s.record_count),
        human_age(s.captured_at_unix)
    )
}

fn compact(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.0}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn human_age(captured_at_unix: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let age_secs = (now - captured_at_unix).max(0);
    if age_secs < 90 {
        "just now".into()
    } else if age_secs < 3600 {
        format!("{} min ago", age_secs / 60)
    } else if age_secs < 86_400 {
        format!("{} h ago", age_secs / 3600)
    } else if age_secs < 86_400 * 7 {
        format!("{} d ago", age_secs / 86_400)
    } else {
        format!("{} weeks ago", age_secs / (86_400 * 7))
    }
}
