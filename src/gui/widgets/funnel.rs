//! Animated pipeline funnel. For each of the 8 pipeline stages, two
//! lines: a labelled row with the running total, and a thin progress
//! bar whose fill is proportional to that stage's share of the largest
//! stage. A pulse highlights the stage that ticked most recently so the
//! eye is drawn to where the work is happening right now.

use std::time::Instant;

use egui::{vec2, Color32, RichText, Sense, Stroke, Ui};

use crate::gui::events::Stage;
use crate::gui::state::UiState;
use crate::gui::theme;

const BAR_HEIGHT: f32 = 8.0;
const ROW_GAP: f32 = 10.0;

pub fn show(ui: &mut Ui, state: &UiState) {
    ui.label(RichText::new("Pipeline").color(theme::TEXT_LO).strong())
        .on_hover_text(
            "Five-stage funnel showing how many files survive each \
             elimination step. Each bar's fill is that stage's count \
             relative to the largest stage; a recent tick pulses the \
             outline.",
        );
    ui.add_space(6.0);

    let max_count = Stage::ALL
        .iter()
        .map(|s| state.stage_counts.get(s).map(|c| c.total).unwrap_or(0))
        .max()
        .unwrap_or(0)
        .max(1);

    let now = Instant::now();

    for (i, stage) in Stage::ALL.iter().enumerate() {
        let counter = state.stage_counts.get(stage).copied().unwrap_or_default();
        let frac = (counter.total as f32 / max_count as f32).clamp(0.0, 1.0);
        let pulse = counter
            .last_update
            .map(|t| 1.0 - (now.saturating_duration_since(t).as_secs_f32() / 0.8).clamp(0.0, 1.0))
            .unwrap_or(0.0);

        // Label row.
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(stage.label())
                    .color(theme::TEXT_HI)
                    .strong()
                    .size(12.0),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(format_compact(counter.total))
                        .color(theme::TEXT_HI)
                        .monospace()
                        .strong(),
                );
            });
        });

        // Bar row.
        let (rect, resp) = ui.allocate_exact_size(
            vec2(ui.available_width(), BAR_HEIGHT),
            Sense::hover(),
        );
        let stage_label = stage.label();
        let count = counter.total;
        resp.on_hover_text(format!("{stage_label}: {count} file(s)"));
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 2.0, theme::PANEL_DEEP);

        let mut color = theme::STAGE_COLORS[i];
        if pulse > 0.0 {
            color = blend(color, Color32::WHITE, pulse * 0.4);
        }
        let fill_rect = egui::Rect::from_min_size(
            rect.min,
            vec2((rect.width() * frac).max(2.0), rect.height()),
        );
        painter.rect_filled(fill_rect, 2.0, color);

        if pulse > 0.0 {
            painter.rect_stroke(
                rect,
                2.0,
                Stroke::new(1.0 + pulse, blend(theme::ACCENT, Color32::WHITE, pulse)),
            );
        }

        ui.add_space(ROW_GAP);
    }

    // Survival ratio footer.
    let inv = state.stage_counts.get(&Stage::Inventory).map(|c| c.total).unwrap_or(0);
    let conf = state.stage_counts.get(&Stage::Confirmed).map(|c| c.total).unwrap_or(0);
    if inv > 0 {
        let pct = conf as f64 / inv as f64 * 100.0;
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);
        ui.label(
            RichText::new(format!(
                "Survival ratio:  {} / {}   ({:.3}%)",
                format_compact(conf),
                format_compact(inv),
                pct
            ))
            .color(theme::TEXT_LO)
            .small(),
        );
    }
}

fn format_compact(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.2}B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn blend(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let lerp = |x: u8, y: u8| ((x as f32) * (1.0 - t) + (y as f32) * t) as u8;
    Color32::from_rgb(
        lerp(a.r(), b.r()),
        lerp(a.g(), b.g()),
        lerp(a.b(), b.b()),
    )
}
