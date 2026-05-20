//! Overall-progress strip — a thin bar that sits above the status
//! line and tells the user, at a glance, what stage the engine is in
//! and roughly how much of the work remains.
//!
//! Indeterminate when the engine doesn't yet know the total (Stage 1
//! inventory, before the walk finishes); determinate with an ETA
//! once we're past inventory.

use std::time::Instant;

use egui::{vec2, Color32, Rounding, Sense, Stroke, Ui};

use crate::gui::events::OverallStage;
use crate::gui::state::UiState;
use crate::gui::theme;

const BAR_HEIGHT: f32 = 22.0;

pub fn show(ui: &mut Ui, state: &UiState) {
    let avail = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(avail, BAR_HEIGHT), Sense::hover());
    resp.on_hover_text(
        "Overall scan progress. Stages 1-3 (Inventory / Size grouping / \
         Layout) run indeterminate because the total isn't known until \
         the inventory walk finishes. Stage 4 (Hashing) shows known \
         progress with an estimated time to completion.",
    );
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, Rounding::same(4.0), theme::PANEL_DEEP);
    painter.rect_stroke(
        rect,
        Rounding::same(4.0),
        Stroke::new(1.0, Color32::from_rgb(0x1f, 0x28, 0x36)),
    );

    let stage = state.overall.stage;
    let label = stage.label();
    let color = stage_color(stage);

    if state.overall.is_determinate() {
        let frac = state.overall.fraction();
        let mut fill_rect = rect;
        fill_rect.set_width(rect.width() * frac);
        painter.rect_filled(fill_rect, Rounding::same(4.0), color);

        let mid = rect.center();
        let pct = (frac * 100.0).round() as u32;
        let total = state.overall.total;
        let done = state.overall.done;
        let eta = state
            .overall
            .eta_secs
            .map(format_eta)
            .unwrap_or_else(|| "—".into());
        let text = format!("{label}  ·  {done}/{total}  ·  {pct}%   ETA {eta}");
        painter.text(
            mid,
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(13.0),
            theme::TEXT_HI,
        );
    } else {
        // Indeterminate: sweeping pulse over the trough.
        let t = (Instant::now().elapsed().as_secs_f32() * 0.6) % 2.0;
        let wave_w = rect.width() * 0.35;
        let pos = ((t - 1.0).abs() * 0.5) * (rect.width() - wave_w) + rect.left();
        let mut pulse = egui::Rect::from_min_size(
            egui::pos2(pos, rect.top()),
            vec2(wave_w, rect.height()),
        );
        pulse = pulse.intersect(rect);
        painter.rect_filled(pulse, Rounding::same(4.0), color);

        let mid = rect.center();
        let text = if state.overall.done == 0 {
            label.to_string()
        } else {
            format!("{label}  ·  {} seen", state.overall.done)
        };
        painter.text(
            mid,
            egui::Align2::CENTER_CENTER,
            text,
            egui::FontId::proportional(13.0),
            theme::TEXT_HI,
        );
    }
}

fn stage_color(stage: OverallStage) -> Color32 {
    match stage {
        OverallStage::Idle => theme::ACCENT_DIM,
        OverallStage::Inventory => Color32::from_rgb(0x69, 0x9b, 0xff),
        OverallStage::SizeGroup => Color32::from_rgb(0x8a, 0xb4, 0xff),
        OverallStage::Layout => Color32::from_rgb(0xa0, 0xc6, 0xff),
        OverallStage::Hashing => theme::ACCENT,
        OverallStage::Finishing => Color32::from_rgb(0xfc, 0x53, 0x85),
    }
}

fn format_eta(secs: f32) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "—".into();
    }
    let total = secs.round() as u64;
    if total < 60 {
        return format!("{total}s");
    }
    let m = total / 60;
    let s = total % 60;
    if m < 60 {
        return format!("{m}m {s:02}s");
    }
    let h = m / 60;
    let m = m % 60;
    format!("{h}h {m:02}m")
}
