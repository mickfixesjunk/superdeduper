//! Read-only History tab — #38 v1.
//!
//! Lists past scans from `crate::scan_history`, newest first.
//! Resubmit button + crash-detection prompt are v2 work; this panel
//! is purely "look at what you've scanned." Empty state shows a
//! pointer to running a first scan.

#![cfg(feature = "gui")]

use egui::{RichText, ScrollArea, Ui};

use crate::gui::theme;
use crate::scan_history::{self, ScanRecord, SubmissionState};

pub fn show(ui: &mut Ui) {
    // Per-frame disk read. Cost is small (one read_dir + N file
    // reads, where N is rows; typical user has <100 scans), well
    // under a frame budget. Keeps the panel always-fresh — no
    // explicit refresh button needed; the next scan's row appears
    // the next time the user clicks this tab.
    let records = match scan_history::list() {
        Ok(r) => r,
        Err(e) => {
            ui.label(RichText::new(format!("Couldn't read scan history: {e}")).color(theme::WARN));
            return;
        }
    };

    if records.is_empty() {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("No scans yet")
                    .color(theme::TEXT_HI)
                    .heading(),
            );
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "Run a scan from the Roots panel — once it finishes, \
                     a row will appear here.",
                )
                .color(theme::TEXT_LO),
            );
        });
        return;
    }

    ui.add_space(4.0);
    ui.label(
        RichText::new(format!(
            "{} past scan{}",
            records.len(),
            if records.len() == 1 { "" } else { "s" }
        ))
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(4.0);

    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("scan_history_grid")
                .num_columns(5)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    header_row(ui);
                    for record in &records {
                        record_row(ui, record);
                    }
                });
        });
}

fn header_row(ui: &mut Ui) {
    let hdr = |ui: &mut Ui, text: &str| {
        ui.label(RichText::new(text).color(theme::TEXT_LO).small().strong());
    };
    hdr(ui, "Date");
    hdr(ui, "Scope");
    hdr(ui, "Files");
    hdr(ui, "Duplicates");
    hdr(ui, "Status");
    ui.end_row();
}

fn record_row(ui: &mut Ui, record: &ScanRecord) {
    // Date — UTC-ish formatting (we store unix seconds; the GUI is
    // not timezone-aware yet). Same date helper that
    // platform::linux::trash uses for trashinfo files, inlined here
    // to keep scan_history a leaf module (no cross-platform pull).
    let date = format_unix_local(record.started_at_unix);
    ui.label(RichText::new(date).color(theme::TEXT_HI));

    // Scope — first root + " +N more" if multiple. Truncated to
    // keep the grid from blowing horizontally; full list available
    // on hover.
    let scope_short = if record.roots.is_empty() {
        "(no roots)".to_string()
    } else if record.roots.len() == 1 {
        truncate_tail(&record.roots[0], 48)
    } else {
        format!(
            "{} +{} more",
            truncate_tail(&record.roots[0], 32),
            record.roots.len() - 1
        )
    };
    let scope_label = ui.label(RichText::new(scope_short).color(theme::TEXT_HI));
    if !record.roots.is_empty() {
        scope_label.on_hover_text(record.roots.join("\n"));
    }

    // Files scanned.
    ui.label(
        RichText::new(format_count(record.total_files))
            .color(theme::TEXT_HI)
            .monospace(),
    );

    // Duplicates + reclaim summary, combined.
    let reclaim = humansize::format_size(record.reclaimable_bytes, humansize::BINARY);
    ui.label(
        RichText::new(format!(
            "{} groups · {reclaim}",
            format_count(record.total_dups)
        ))
        .color(theme::TEXT_HI),
    );

    // Status pill. v1 always reads "pending" because the resubmit
    // path is v2 work, but we render it as a proper pill anyway so
    // v2 just changes the data (no UI change needed).
    let (status_text, status_color) = match record.submission_state {
        SubmissionState::Pending => ("⏳ pending", theme::WARN),
        SubmissionState::Submitted => ("✓ submitted", theme::COOL),
        SubmissionState::Failed => ("⚠ failed", theme::HOT),
        SubmissionState::Interrupted => ("🛑 interrupted", theme::HOT),
    };
    ui.label(
        RichText::new(status_text)
            .color(status_color)
            .monospace()
            .small(),
    );

    ui.end_row();
}

/// Render a Unix timestamp as `YYYY-MM-DD HH:MM`. Local-time intent
/// but we're not pulling chrono for v1 — emit UTC and let the user
/// mentally adjust. v2 can swap in chrono or jiff once we adopt it
/// elsewhere.
fn format_unix_local(secs: u64) -> String {
    // Convert seconds since 1970 to date components. Same arithmetic
    // as `iso8601_local_seconds` in src/platform/linux/trash.rs, kept
    // local here to avoid pulling a Linux-only path into a Windows
    // build. Constants reference Howard Hinnant's
    // chrono-compatible civil_from_days algorithm.
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let h = rem / 3600;
    let m = (rem / 60) % 60;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (y + if mo <= 2 { 1 } else { 0 }) as i32;
    format!("{year:04}-{mo:02}-{day:02} {h:02}:{m:02}")
}

/// Tail-keep truncation for long paths. Keeps the last `cap` chars
/// (prefixed with an ellipsis) so the filename + immediate parent
/// survive — that's what the user recognises at a glance.
fn truncate_tail(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let skip = s.chars().count() - cap + 1;
    let tail: String = s.chars().skip(skip).collect();
    format!("…{tail}")
}

/// Compact count formatting: `42`, `1.2k`, `15.3k`, `2.1M`. Matches
/// the voice of the rest of the GUI (badge counts use the same
/// shape via `theme::compact`).
fn format_count(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 1_000_000 {
        format!("{:.1}k", (n as f64) / 1_000.0)
    } else if n < 1_000_000_000 {
        format!("{:.1}M", (n as f64) / 1_000_000.0)
    } else {
        format!("{:.1}B", (n as f64) / 1_000_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_local_renders_known_epoch() {
        // 1970-01-01T00:00:00 UTC.
        assert_eq!(format_unix_local(0), "1970-01-01 00:00");
        // 2024-01-01T00:00:00 UTC — well-known test vector.
        assert_eq!(format_unix_local(1_704_067_200), "2024-01-01 00:00");
    }

    #[test]
    fn truncate_tail_keeps_filename() {
        let p = "/home/long/path/that/keeps/going/and/going/file.bin";
        let out = truncate_tail(p, 20);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("file.bin"), "got: {out}");
    }

    #[test]
    fn truncate_tail_passes_short_strings() {
        assert_eq!(truncate_tail("/tmp/foo", 20), "/tmp/foo");
    }

    #[test]
    fn format_count_thresholds() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1.0k");
        assert_eq!(format_count(15_300), "15.3k");
        assert_eq!(format_count(2_100_000), "2.1M");
    }
}
