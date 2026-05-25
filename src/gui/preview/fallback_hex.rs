//! Built-in hex viewer. Renders the first 4 KiB and last 4 KiB of
//! a file in 16-byte rows with an ASCII gutter:
//!
//! ```text
//! 00000000  4d 5a 90 00 03 00 00 00  04 00 00 00 ff ff 00 00  MZ.............
//! 00000010  b8 00 00 00 00 00 00 00  40 00 00 00 00 00 00 00  ........@.......
//! ```
//!
//! For small files (< 8 KiB) the full content renders. Larger
//! files show a "[…]" separator between the head and tail blocks.

use std::path::Path;

use egui::{RichText, ScrollArea, Ui};

use crate::gui::theme;

/// Bytes per row in the hex display.
const ROW_BYTES: usize = 16;
/// Head + tail caps. 4 KiB each = 256 rows each = ~512 rows total
/// for a long file. Egui renders that comfortably; bigger caps
/// would risk visible jank on every preview-pane open.
const HEAD_BYTES: usize = 4 * 1024;
const TAIL_BYTES: usize = 4 * 1024;

pub fn show(ui: &mut Ui, path: &Path) {
    let (head, tail, total_size, err) = read_head_and_tail(path);
    if let Some(e) = err {
        ui.label(RichText::new(e).color(theme::HOT));
        return;
    }

    if let Some(total) = total_size {
        let total_human = humansize::format_size(total, humansize::BINARY);
        ui.label(
            RichText::new(format!("Total size: {total_human}"))
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );
        ui.add_space(2.0);
    }

    let truncated = tail.is_some();

    ScrollArea::vertical()
        .id_salt("preview-hex-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            render_block(ui, &head, 0);
            if truncated {
                ui.label(
                    RichText::new("  […]")
                        .color(theme::TEXT_LO)
                        .monospace()
                        .small(),
                );
                if let (Some(tail_bytes), Some(total)) = (tail.as_ref(), total_size) {
                    let tail_start = total.saturating_sub(tail_bytes.len() as u64);
                    render_block(ui, tail_bytes, tail_start);
                }
            }
        });
}

/// Returns (head, tail, total_size, error). When the file is
/// smaller than HEAD_BYTES + TAIL_BYTES, the entire content lands
/// in `head` + `tail` is `None`.
fn read_head_and_tail(path: &Path) -> (Vec<u8>, Option<Vec<u8>>, Option<u64>, Option<String>) {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => return (Vec::new(), None, None, Some(format!("stat failed: {e}"))),
    };
    let size = meta.len();
    if size <= (HEAD_BYTES + TAIL_BYTES) as u64 {
        // Small file — read it all.
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                return (
                    Vec::new(),
                    None,
                    Some(size),
                    Some(format!("read failed: {e}")),
                )
            }
        };
        return (bytes, None, Some(size), None);
    }
    // Big file — head + tail.
    use std::io::{Read, Seek, SeekFrom};
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            return (
                Vec::new(),
                None,
                Some(size),
                Some(format!("open failed: {e}")),
            )
        }
    };
    let mut head = vec![0u8; HEAD_BYTES];
    let n_head = match f.read(&mut head) {
        Ok(n) => n,
        Err(e) => {
            return (
                Vec::new(),
                None,
                Some(size),
                Some(format!("read failed: {e}")),
            )
        }
    };
    head.truncate(n_head);
    let tail_start = size.saturating_sub(TAIL_BYTES as u64);
    if let Err(e) = f.seek(SeekFrom::Start(tail_start)) {
        return (head, None, Some(size), Some(format!("seek failed: {e}")));
    }
    let mut tail = vec![0u8; TAIL_BYTES];
    let n_tail = match f.read(&mut tail) {
        Ok(n) => n,
        Err(e) => return (head, None, Some(size), Some(format!("read failed: {e}"))),
    };
    tail.truncate(n_tail);
    (head, Some(tail), Some(size), None)
}

fn render_block(ui: &mut Ui, bytes: &[u8], offset_base: u64) {
    for (row_idx, chunk) in bytes.chunks(ROW_BYTES).enumerate() {
        let offset = offset_base + (row_idx as u64 * ROW_BYTES as u64);
        let mut hex_part = String::with_capacity(48);
        let mut ascii_part = String::with_capacity(ROW_BYTES);
        for (i, b) in chunk.iter().enumerate() {
            hex_part.push_str(&format!("{b:02x}"));
            // Group bytes 8 apart with an extra space for readability.
            if i + 1 == 8 {
                hex_part.push_str("  ");
            } else if i + 1 < chunk.len() {
                hex_part.push(' ');
            }
            ascii_part.push(if (0x20..=0x7E).contains(b) {
                *b as char
            } else {
                '.'
            });
        }
        // Pad the hex column so short trailing rows still line up
        // with the ASCII gutter.
        // Compute the canonical width: 16 bytes × 2 chars + 15
        // separators + the extra space at the midpoint = 49 chars.
        const FULL_WIDTH: usize = 49;
        let pad = FULL_WIDTH.saturating_sub(hex_part.len());
        let line = format!("{offset:08x}  {hex_part}{}  {ascii_part}", " ".repeat(pad));
        ui.label(
            RichText::new(line)
                .color(theme::TEXT_HI)
                .monospace()
                .small(),
        );
    }
}
