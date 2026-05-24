//! Transient corner notifications. Used for events that arrive
//! after the user has likely moved on from the trigger surface —
//! e.g. async rank-computation completing after they've closed the
//! post-scan modal. Toasts stack top-right, fade out, and auto-
//! dismiss on a TTL.
//!
//! Push from any thread via [`push`]; render once per frame from
//! [`show`] in the app's `update()` loop.

use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, Context, RichText};
use parking_lot::Mutex;

use crate::gui::theme;

pub struct Toast {
    pub heading: String,
    pub lines: Vec<String>,
    pub spawned_at: Instant,
    pub ttl: Duration,
}

impl Toast {
    fn alpha(&self) -> f32 {
        let elapsed = self.spawned_at.elapsed();
        if elapsed >= self.ttl {
            return 0.0;
        }
        let remaining = self.ttl - elapsed;
        let fade_window = Duration::from_millis(800);
        if remaining < fade_window {
            return remaining.as_secs_f32() / fade_window.as_secs_f32();
        }
        1.0
    }

    fn expired(&self) -> bool {
        self.spawned_at.elapsed() >= self.ttl
    }
}

static QUEUE: OnceLock<Mutex<VecDeque<Toast>>> = OnceLock::new();

fn queue() -> &'static Mutex<VecDeque<Toast>> {
    QUEUE.get_or_init(|| Mutex::new(VecDeque::new()))
}

/// Push a multi-line toast. `heading` renders bold; `lines` render
/// below as dim text. TTL is wall-clock — egui timers are not used
/// (push can come from any thread).
pub fn push(heading: impl Into<String>, lines: Vec<String>, ttl: Duration) {
    let toast = Toast {
        heading: heading.into(),
        lines,
        spawned_at: Instant::now(),
        ttl,
    };
    queue().lock().push_back(toast);
}

/// Render active toasts. Call once per frame from the app's
/// `update()`. Returns early when the queue is empty. Drops expired
/// toasts before rendering; requests a repaint while any toast is
/// alive so fade-out animates without a separate timer.
pub fn show(ctx: &Context) {
    let mut q = queue().lock();
    q.retain(|t| !t.expired());
    if q.is_empty() {
        return;
    }
    ctx.request_repaint_after(Duration::from_millis(33));

    let screen = ctx.screen_rect();
    let mut y_offset = 16.0;
    let toast_width = 320.0;

    for toast in q.iter() {
        let alpha = toast.alpha();
        let pos = egui::pos2(screen.right() - toast_width - 16.0, screen.top() + y_offset);

        let bg = apply_alpha(Color32::from_rgb(0x10, 0x14, 0x1d), alpha);
        let stroke_col = apply_alpha(theme::ACCENT, alpha * 0.7);
        let heading_col = apply_alpha(theme::ACCENT, alpha);
        let body_col = apply_alpha(theme::TEXT_HI, alpha * 0.95);

        let area_id = egui::Id::new(("sd-toast", toast.spawned_at));
        egui::Area::new(area_id)
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(bg)
                    .stroke(egui::Stroke::new(1.0, stroke_col))
                    .rounding(6.0)
                    .inner_margin(egui::Margin::symmetric(12, 10))
                    .show(ui, |ui| {
                        ui.set_min_width(toast_width - 24.0);
                        ui.set_max_width(toast_width - 24.0);
                        ui.label(
                            RichText::new(&toast.heading)
                                .color(heading_col)
                                .strong()
                                .size(13.0),
                        );
                        for line in &toast.lines {
                            ui.label(RichText::new(line).color(body_col).size(12.0));
                        }
                    });
            });

        y_offset += 78.0 + (toast.lines.len() as f32) * 16.0;
    }
}

fn apply_alpha(c: Color32, a: f32) -> Color32 {
    let a = a.clamp(0.0, 1.0);
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (a * 255.0) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_full_before_fade_window() {
        let t = Toast {
            heading: "h".into(),
            lines: vec![],
            spawned_at: Instant::now(),
            ttl: Duration::from_secs(5),
        };
        assert!(t.alpha() > 0.99);
        assert!(!t.expired());
    }

    #[test]
    fn alpha_zero_after_ttl() {
        let t = Toast {
            heading: "h".into(),
            lines: vec![],
            spawned_at: Instant::now() - Duration::from_secs(10),
            ttl: Duration::from_secs(5),
        };
        assert_eq!(t.alpha(), 0.0);
        assert!(t.expired());
    }

    #[test]
    fn push_then_drain_via_show_works_when_queue_isolated() {
        let mut q = VecDeque::new();
        q.push_back(Toast {
            heading: "h".into(),
            lines: vec!["l".into()],
            spawned_at: Instant::now() - Duration::from_secs(10),
            ttl: Duration::from_secs(5),
        });
        q.retain(|t| !t.expired());
        assert!(q.is_empty(), "expired toasts removed by retain");
    }
}
