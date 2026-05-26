//! History tab — #38 v1 + #41 v2.
//!
//! Lists past scans from `crate::scan_history`, newest first. v1 was
//! read-only; v2 adds:
//!
//!   * Resubmit button per row (telemetry-gated). Worker lives in
//!     [`crate::gui::resubmit`]; this panel just dispatches +
//!     surfaces the outcome inline.
//!   * Delete button per row, wired to `scan_history::delete`.
//!   * Last-resubmit-outcome banner pinned above the grid until
//!     the user clicks Dismiss (one banner global; the most recent
//!     outcome wins).
//!
//! App-start crash-detection modal + retention enforcement live in
//! `gui::app` (the panel only sees fresh state via `scan_history::list`
//! per frame).

#![cfg(feature = "gui")]

use egui::{RichText, ScrollArea, Ui};

use crate::gui::theme;
use crate::scan_history::{self, ScanRecord, SubmissionState};

pub fn show(ui: &mut Ui) {
    // #41 — drain the resubmit worker's last outcome (if any) into
    // a panel-local cache so successive frames keep showing it
    // until the user dismisses it. The drain is one-shot per
    // outcome — successive calls return None until the next
    // resubmit finishes.
    #[cfg(feature = "telemetry")]
    {
        if let Some((scan_id, outcome)) = crate::gui::resubmit::drain_outcome() {
            store_last_outcome(scan_id, outcome);
        }
    }
    show_last_outcome_banner(ui);
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

    // Collect rows to act on AFTER rendering — `ui.button` returns
    // Response on click but we want to drive the resubmit worker
    // and scan_history::delete OUTSIDE the per-row borrow. Simple
    // pattern: capture (scan_id, action) tuples and dispatch after
    // the grid closes.
    let mut pending_actions: Vec<RowAction> = Vec::new();

    ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("scan_history_grid")
                .num_columns(7)
                .spacing([12.0, 6.0])
                .striped(true)
                .show(ui, |ui| {
                    header_row(ui);
                    for record in &records {
                        if let Some(act) = record_row(ui, record) {
                            pending_actions.push(act);
                        }
                    }
                });
        });

    for act in pending_actions {
        match act {
            RowAction::Resubmit(_scan_id) => {
                #[cfg(feature = "telemetry")]
                if let Err(e) = crate::gui::resubmit::request_resubmit(&_scan_id) {
                    store_inline_error(_scan_id, e);
                }
            }
            RowAction::Delete(scan_id) => {
                // Idempotent; failures only matter if filesystem
                // blew up. Log + move on so the grid refreshes
                // cleanly on next frame.
                if let Err(e) = scan_history::delete(&scan_id) {
                    tracing::warn!(scan_id = %scan_id, error = %e, "scan_history delete failed");
                }
            }
        }
    }
}

/// Action requested by a History row, dispatched after the grid
/// closes so the panel can re-borrow `scan_history` freely.
#[derive(Debug, Clone)]
enum RowAction {
    Resubmit(String),
    Delete(String),
}

fn header_row(ui: &mut Ui) {
    let hdr = |ui: &mut Ui, text: &str| {
        ui.label(RichText::new(text).color(theme::TEXT_LO).small().strong());
    };
    hdr(ui, "Date");
    hdr(ui, "Scope");
    hdr(ui, "Files");
    hdr(ui, "Duplicates");
    hdr(ui, "Reclaimed");
    hdr(ui, "Status");
    hdr(ui, "Actions");
    ui.end_row();
}

fn record_row(ui: &mut Ui, record: &ScanRecord) -> Option<RowAction> {
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

    // Duplicates + reclaimable summary, combined. (#82: this is
    // the "what was scannable" figure, distinct from the
    // "actually reclaimed" column below.)
    let reclaim = humansize::format_size(record.reclaimable_bytes, humansize::BINARY);
    ui.label(
        RichText::new(format!(
            "{} groups · {reclaim}",
            format_count(record.total_dups)
        ))
        .color(theme::TEXT_HI),
    );

    // #82 — Actually-reclaimed column. None ⇒ scan-only (the
    // user never clicked Go or actions failed); Some ⇒
    // "♻ X.Y GB" with a hover tooltip showing the per-action
    // breakdown. Distinct visual from the "reclaimable" figure
    // above so users can see at a glance what they actually
    // followed through on.
    if record.reclaim_at_unix.is_some() {
        let actually = humansize::format_size(record.actually_reclaimed_bytes, humansize::BINARY);
        let cell = ui.label(
            RichText::new(format!("♻ {actually}"))
                .color(theme::ACCENT)
                .monospace(),
        );
        if !record.action_breakdown.is_empty() {
            let tooltip = action_breakdown_tooltip(record);
            cell.on_hover_text(tooltip);
        }
    } else {
        ui.label(RichText::new("—").color(theme::TEXT_LO).monospace().small())
            .on_hover_text(
                "Scan only — no actions were taken on this run. Run \
             Recycle / Hardlink / Reflink / Archive on the groups \
             to credit reclaim bytes to your profile.",
            );
    }

    // Status pill. v1 always reads "pending" because the resubmit
    // path is v2 work, but we render it as a proper pill anyway so
    // v2 just changes the data (no UI change needed).
    let (status_text, status_color) = match record.submission_state {
        SubmissionState::Pending => ("⏳ pending", theme::WARN),
        SubmissionState::Submitted => ("✓ submitted", theme::COOL),
        SubmissionState::Failed => ("⚠ failed", theme::HOT),
        SubmissionState::Interrupted => ("🛑 interrupted", theme::HOT),
    };
    let status_label = ui.label(
        RichText::new(status_text)
            .color(status_color)
            .monospace()
            .small(),
    );
    // Surface attempt history on hover for any row that's been
    // retried — quiet for the first attempt so the tooltip stays
    // signal-only.
    if record.attempt_count >= 2 {
        status_label.on_hover_text(format!(
            "Retried {}× (most recent: {})",
            record.attempt_count,
            record
                .last_attempt_at_unix
                .map(format_unix_local)
                .unwrap_or_else(|| "—".into())
        ));
    }

    // #41 — Actions column: Resubmit + Delete. The Resubmit button
    // is enabled only when (a) the row carries a captured payload
    // and (b) the submission_state allows another attempt, and (c)
    // no resubmit is currently in flight elsewhere.
    let mut action: Option<RowAction> = None;
    ui.horizontal(|ui| {
        // Resubmit visibility/enablement rules:
        //  • State must be Pending / Failed / Interrupted
        //    (Submitted means the server already has it; clicking
        //     again would just 409).
        //  • Row must have a captured payload (older v1/v2 rows
        //    don't, and unregistered scans don't either).
        //  • No global resubmit currently running (we serialise
        //    to keep the worker simple + the user's mental model
        //    "one click at a time").
        let resubmittable_state = matches!(
            record.submission_state,
            SubmissionState::Pending | SubmissionState::Failed | SubmissionState::Interrupted
        );
        let has_payload = record.submission_payload.is_some();
        let in_flight_elsewhere = {
            #[cfg(feature = "telemetry")]
            {
                crate::gui::resubmit::in_flight_scan_id().is_some()
            }
            #[cfg(not(feature = "telemetry"))]
            {
                false
            }
        };
        let am_in_flight = {
            #[cfg(feature = "telemetry")]
            {
                crate::gui::resubmit::in_flight_scan_id().as_deref() == Some(&record.scan_id)
            }
            #[cfg(not(feature = "telemetry"))]
            {
                false
            }
        };
        let resubmit_enabled =
            resubmittable_state && has_payload && (!in_flight_elsewhere || am_in_flight);
        let resubmit_btn_text = if am_in_flight {
            "⏳ submitting…"
        } else {
            "↻ Resubmit"
        };
        let resubmit_response = ui.add_enabled(
            resubmit_enabled,
            egui::Button::new(RichText::new(resubmit_btn_text).color(theme::TEXT_HI))
                .min_size(egui::vec2(110.0, 22.0)),
        );
        let tooltip = if am_in_flight {
            "Resubmit in flight…".to_string()
        } else if !has_payload {
            "This row was recorded before payload-capture (v1/v2); rescan to get a resubmittable row.".to_string()
        } else if !resubmittable_state {
            format!("State `{:?}` — server already has this submission.", record.submission_state)
        } else if in_flight_elsewhere {
            "Another resubmit is in flight; click again once it completes.".to_string()
        } else {
            format!(
                "Resubmit to {} (channel: {}).",
                record
                    .submission_channel
                    .as_deref()
                    .unwrap_or(record.channel.as_str()),
                record.channel,
            )
        };
        let resubmit_response = resubmit_response.on_hover_text(tooltip);
        if resubmit_response.clicked() && resubmit_enabled && !am_in_flight {
            action = Some(RowAction::Resubmit(record.scan_id.clone()));
        }

        let delete_response = ui
            .add(
                egui::Button::new(RichText::new("✕ Delete").color(theme::TEXT_LO))
                    .min_size(egui::vec2(80.0, 22.0)),
            )
            .on_hover_text("Permanently remove this row from local history. No server delete.");
        if delete_response.clicked() {
            action = Some(RowAction::Delete(record.scan_id.clone()));
        }
    });

    ui.end_row();
    action
}

// ============================================================
// Panel-local "last resubmit outcome" cache.
// ============================================================

/// Cached banner state. Stored process-globally rather than threaded
/// through `show()` so the App doesn't need to know about it.
fn last_outcome_slot() -> &'static parking_lot::Mutex<Option<LastOutcome>> {
    use std::sync::OnceLock;
    static SLOT: OnceLock<parking_lot::Mutex<Option<LastOutcome>>> = OnceLock::new();
    SLOT.get_or_init(|| parking_lot::Mutex::new(None))
}

#[derive(Debug, Clone)]
struct LastOutcome {
    scan_id: String,
    message: String,
    color: egui::Color32,
}

#[cfg(feature = "telemetry")]
fn store_last_outcome(scan_id: String, outcome: crate::leaderboard::submission::SubmitOutcome) {
    use crate::leaderboard::submission::SubmitOutcome;
    let (message, color) = match outcome {
        SubmitOutcome::Accepted { submission_id, .. } => {
            (format!("Resubmit accepted ({submission_id})."), theme::COOL)
        }
        SubmitOutcome::DuplicateNoChange => (
            "Resubmit: server already had this submission (no change).".to_string(),
            theme::COOL,
        ),
        SubmitOutcome::Rejected { status, reason } => (
            format!("Resubmit rejected ({status}): {reason}"),
            theme::HOT,
        ),
        SubmitOutcome::Transient { reason } => (
            format!("Resubmit transient failure (will stay Pending): {reason}"),
            theme::WARN,
        ),
        SubmitOutcome::FlaggedForReview { .. } => (
            "Resubmit flagged for review (uncommon path).".to_string(),
            theme::WARN,
        ),
    };
    *last_outcome_slot().lock() = Some(LastOutcome {
        scan_id,
        message,
        color,
    });
}

/// Pre-resubmit-dispatch error (e.g. cross-install, no payload).
/// Renders in the same banner as outcomes.
#[cfg(feature = "telemetry")]
fn store_inline_error(scan_id: String, message: String) {
    *last_outcome_slot().lock() = Some(LastOutcome {
        scan_id,
        message: format!("Couldn't start resubmit: {message}"),
        color: theme::HOT,
    });
}

fn show_last_outcome_banner(ui: &mut Ui) {
    let slot = last_outcome_slot();
    let outcome = slot.lock().clone();
    let Some(outcome) = outcome else { return };
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(&outcome.message)
                .color(outcome.color)
                .monospace()
                .small(),
        );
        if ui
            .add(
                egui::Button::new(RichText::new("✕").color(theme::TEXT_LO))
                    .min_size(egui::vec2(20.0, 18.0)),
            )
            .on_hover_text("Dismiss this banner.")
            .clicked()
        {
            *slot.lock() = None;
        }
    });
    let _ = outcome.scan_id; // reserved for future "scroll to that row" UX
    ui.add_space(4.0);
}

/// Render a Unix timestamp as `YYYY-MM-DD HH:MM`. Local-time intent
/// but we're not pulling chrono for v1 — emit UTC and let the user
/// mentally adjust. v2 can swap in chrono or jiff once we adopt it
/// elsewhere.
pub(crate) fn format_unix_local(secs: u64) -> String {
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

/// #82 — Compose the hover tooltip for the Reclaimed column. One
/// line per locked-action key with non-zero bytes, plus a header
/// summary. Stable key ordering matches the `LOCKED_ACTION_KEYS`
/// list so the breakdown reads the same way across rows.
fn action_breakdown_tooltip(record: &ScanRecord) -> String {
    let mut lines: Vec<String> = Vec::new();
    let reclaim_at = record
        .reclaim_at_unix
        .map(format_unix_local)
        .unwrap_or_else(|| "—".into());
    lines.push(format!("Reclaim landed: {reclaim_at}"));
    if let Some(updated) = record.reclaim_updated_at_unix {
        if Some(updated) != record.reclaim_at_unix {
            lines.push(format!(
                "Most recent update: {}",
                format_unix_local(updated)
            ));
        }
    }
    lines.push(String::new()); // blank between header + body
    let labels = [
        ("deleted_to_recycle_bytes", "Recycle"),
        ("deleted_permanently_bytes", "Remove"),
        ("hardlink_replaced_bytes", "Hardlink"),
        ("reflink_replaced_bytes", "Reflink"),
        ("archived_bytes", "Archive"),
    ];
    for (key, label) in labels {
        if let Some(bytes) = record.action_breakdown.get(key) {
            if *bytes > 0 {
                lines.push(format!(
                    "  {label}: {}",
                    humansize::format_size(*bytes, humansize::BINARY),
                ));
            }
        }
    }
    lines.join("\n")
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
