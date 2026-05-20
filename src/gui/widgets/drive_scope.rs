//! Per-drive live scope. For each detected physical drive we render:
//!
//! * a header row with model, type (HDD/SSD), and current MB/s,
//! * a throughput sparkline (last 30 s of bytes-per-second),
//! * a 2-D LCN-vs-time read trace — the killer visualization that
//!   shows the LCN-sorted read scheduler doing its job. On an HDD the
//!   dots crawl monotonically up the Y axis; on an SSD they spray.

use std::time::Instant;

use egui::{vec2, Color32, FontId, RichText, Sense, Stroke, Ui};

use crate::gui::state::{DriveLive, UiState, THROUGHPUT_WINDOW_SECS};
use crate::gui::theme;

const SCOPE_HEIGHT: f32 = 120.0;
const SPARK_HEIGHT: f32 = 36.0;

pub fn show(ui: &mut Ui, state: &UiState) {
    ui.label(RichText::new("Drive scope").color(theme::TEXT_LO).strong());
    ui.add_space(4.0);

    if state.drives.is_empty() {
        ui.label(
            RichText::new("waiting for drives…")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return;
    }

    let mut ids: Vec<_> = state.drives.keys().copied().collect();
    ids.sort();

    let now = Instant::now();
    for id in ids {
        let drive = &state.drives[&id];
        draw_drive_panel(ui, drive, now);
        ui.add_space(8.0);
    }
}

fn draw_drive_panel(ui: &mut Ui, drive: &DriveLive, now: Instant) {
    let frame = egui::Frame::none()
        .fill(theme::PANEL_DEEP)
        .inner_margin(8.0)
        .rounding(6.0)
        .stroke(Stroke::new(1.0, Color32::from_rgb(0x1f, 0x28, 0x36)));

    frame.show(ui, |ui| {
        let mbps = drive.current_mbps();
        let type_color = if drive.info.has_seek_penalty {
            theme::HDD
        } else {
            theme::SSD
        };
        let type_label = if drive.info.has_seek_penalty { "HDD" } else { "SSD" };

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("● {}", type_label))
                    .color(type_color)
                    .strong(),
            );
            ui.label(RichText::new(&drive.info.model).color(theme::TEXT_HI));
            ui.label(
                RichText::new(format!("/ {}", drive.info.volume_label)).color(theme::TEXT_LO),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format!("{:>6.1} MB/s", mbps))
                        .color(theme::ACCENT)
                        .monospace()
                        .strong(),
                );
                ui.label(
                    RichText::new(format!("peak {:.0}", drive.peak_mbps))
                        .color(theme::TEXT_LO)
                        .monospace()
                        .small(),
                );
            });
        });

        ui.add_space(2.0);
        draw_sparkline(ui, drive, now);
        ui.add_space(4.0);
        draw_lcn_trace(ui, drive, now);
    });
}

fn draw_sparkline(ui: &mut Ui, drive: &DriveLive, now: Instant) {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), SPARK_HEIGHT),
        Sense::hover(),
    );
    resp.on_hover_text(
        "Throughput sparkline — bytes-per-second the engine is reading \
         from this drive over the last 30 seconds. Right edge = now.",
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::BG);

    if drive.throughput.is_empty() {
        return;
    }

    let max_mbps = drive
        .throughput
        .iter()
        .map(|(_, b)| *b as f32 / 1_048_576.0)
        .fold(1.0_f32, f32::max);

    let now_s = now;
    let window = THROUGHPUT_WINDOW_SECS as f32;

    let mut prev: Option<egui::Pos2> = None;
    for (t, bytes) in &drive.throughput {
        let age = now_s.saturating_duration_since(*t).as_secs_f32();
        let x_frac = 1.0 - (age / window).clamp(0.0, 1.0);
        let y_frac = (*bytes as f32 / 1_048_576.0) / max_mbps;
        let p = rect.left_bottom()
            + vec2(rect.width() * x_frac, -rect.height() * y_frac.clamp(0.0, 1.0));
        if let Some(pp) = prev {
            painter.line_segment([pp, p], Stroke::new(1.5, theme::ACCENT));
        }
        prev = Some(p);
    }

    // Axis hint.
    painter.text(
        rect.right_top() + vec2(-4.0, 2.0),
        egui::Align2::RIGHT_TOP,
        format!("{:.0} MB/s", max_mbps),
        FontId::proportional(10.0),
        theme::TEXT_LO,
    );
}

fn draw_lcn_trace(ui: &mut Ui, drive: &DriveLive, now: Instant) {
    let (rect, resp) = ui.allocate_exact_size(
        vec2(ui.available_width(), SCOPE_HEIGHT),
        Sense::hover(),
    );
    let tip = if drive.info.has_seek_penalty {
        "LCN-vs-time read trace (HDD). Y = position on the drive, \
         X = time (right = now). The yellow line climbing diagonally \
         means the scheduler is reading sequentially — the cheap \
         pattern on a spinning disk."
    } else {
        "LCN-vs-time read trace (SSD). Y = position on the drive, \
         X = time (right = now). The teal cloud is the random spray \
         pattern SSDs love — no seek penalty so we read all over \
         the address space in parallel."
    };
    resp.on_hover_text(tip);
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0, theme::BG);

    // Faint axis label.
    painter.text(
        rect.left_top() + vec2(4.0, 2.0),
        egui::Align2::LEFT_TOP,
        "LCN ↑   time →",
        FontId::proportional(10.0),
        theme::TEXT_LO,
    );

    if drive.reads.is_empty() {
        return;
    }

    let window = THROUGHPUT_WINDOW_SECS as f32;
    let max_lcn = drive
        .reads
        .iter()
        .map(|r| r.lcn_bytes)
        .max()
        .unwrap_or(1)
        .max(1) as f32;

    let dot_color = if drive.info.has_seek_penalty {
        theme::HDD
    } else {
        theme::SSD
    };

    // Density-aware rendering: very large sample sets are subsampled
    // and rendered with low alpha so the trace reads as a heat plot,
    // not a single opaque blob.
    let total = drive.reads.len();
    let max_dots = if drive.info.has_seek_penalty { 1024 } else { 768 };
    let stride = (total / max_dots).max(1);
    let alpha = if drive.info.has_seek_penalty { 0.85 } else { 0.35 };
    let radius = if drive.info.has_seek_penalty { 2.0 } else { 1.4 };

    for (i, r) in drive.reads.iter().enumerate() {
        if i % stride != 0 {
            continue;
        }
        let age = now.saturating_duration_since(r.at).as_secs_f32();
        if age > window {
            continue;
        }
        let x_frac = 1.0 - (age / window);
        let y_frac = (r.lcn_bytes as f32) / max_lcn;
        let p = rect.left_bottom()
            + vec2(rect.width() * x_frac, -rect.height() * y_frac.clamp(0.0, 1.0));
        painter.circle_filled(p, radius, dot_color.gamma_multiply(alpha));
    }

    // Caption: drive-relative LCN range covered in the visible window.
    if let (Some(first), Some(last)) = (drive.reads.front(), drive.reads.back()) {
        let span = last.lcn_bytes.max(first.lcn_bytes) - last.lcn_bytes.min(first.lcn_bytes);
        let traversed = if drive.info.has_seek_penalty {
            format!("sequential   Δ {} this window", theme::humansize(span))
        } else {
            format!("scattered    Δ {} this window", theme::humansize(span))
        };
        painter.text(
            rect.right_top() + vec2(-4.0, 2.0),
            egui::Align2::RIGHT_TOP,
            traversed,
            FontId::proportional(10.0),
            theme::TEXT_LO,
        );
    }
}
