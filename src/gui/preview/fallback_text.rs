//! Built-in text viewer. Renders the first 64 KiB of a file in a
//! read-only egui TextEdit. Designed to be cheap (one bounded read
//! per render is fine; egui caches the resulting widget tree).
//!
//! Syntax highlighting is out-of-scope for v1 — the spec mentions
//! `syntect` as a candidate but pulling it would 10× the binary
//! size. Plain monospace, wrap-on, scroll-on is enough for the
//! "which copy do I keep?" use case.

use std::path::Path;

use egui::{RichText, ScrollArea, Ui};

use crate::gui::theme;

/// Max bytes we read for the text viewer. Keeps egui's text
/// shaping bounded — a multi-megabyte log file would lock up
/// the GUI if rendered in full.
const MAX_BYTES: usize = 64 * 1024;

pub fn show(ui: &mut Ui, path: &Path) {
    // Read up to MAX_BYTES + 1 so we can detect truncation by
    // checking whether n > MAX_BYTES.
    let mut buf = vec![0u8; MAX_BYTES + 1];
    let (n, read_err) = match std::fs::File::open(path) {
        Ok(mut f) => {
            use std::io::Read;
            match f.read(&mut buf) {
                Ok(n) => (n, None),
                Err(e) => (0, Some(format!("read failed: {e}"))),
            }
        }
        Err(e) => (0, Some(format!("open failed: {e}"))),
    };
    if let Some(reason) = read_err {
        ui.label(RichText::new(reason).color(theme::HOT));
        return;
    }
    let truncated = n > MAX_BYTES;
    let n_render = n.min(MAX_BYTES);
    let body = String::from_utf8_lossy(&buf[..n_render]).into_owned();

    if truncated {
        ui.label(
            RichText::new(format!(
                "Showing first {} of {}. Larger files truncate.",
                humansize::format_size(MAX_BYTES as u64, humansize::BINARY),
                humansize::format_size(MAX_BYTES as u64 + 1, humansize::BINARY) // approximate label
            ))
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
        ui.add_space(2.0);
    }
    if body.is_empty() {
        ui.label(
            RichText::new("(empty file)")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return;
    }

    ScrollArea::vertical()
        .id_salt("preview-text-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            // Use a read-only TextEdit so the user can select +
            // copy. monospace + wrap-on so long log lines fit.
            // Multiline gives us the embedded-newlines handling.
            let mut body_ref = body.as_str();
            ui.add_sized(
                ui.available_size(),
                egui::TextEdit::multiline(&mut body_ref)
                    .desired_rows(40)
                    .font(egui::TextStyle::Monospace)
                    .interactive(true)
                    .frame(false),
            );
        });
}
