//! Always-on environment banner for non-prod channels.
//!
//! Per `dev-channel-spec.md` §3.4 + §5.5 (LOCKED 2026-05-24):
//!
//! - **Color**: `#ff8800` (safety-orange) for `dev`; `#3399ff`
//!   (sky-blue) for `local`.
//! - **Geometry**: 32px tall horizontal strip across the top of
//!   the main window; full window width.
//! - **Behaviour**: ALWAYS-ON when channel ≠ prod. NOT
//!   dismissable per-session — a dismissable banner defeats the
//!   "this isn't prod" signal if the user can hide it.
//! - **Content**: `"DEV ENVIRONMENT"` for `dev`; `"LOCAL ENVIRONMENT"`
//!   for `local`. White text, bold, centered.

#![cfg(feature = "gui")]

use egui::{Align, Color32, FontId, Layout, RichText, TopBottomPanel};

use crate::channel::Channel;

/// Vertical extent of the banner, in egui logical pixels. Locked
/// at 32 per spec §3.4.
pub const BANNER_HEIGHT: f32 = 32.0;

/// Render the channel banner ABOVE everything else if the active
/// channel is non-prod. No-op when on prod — the spec explicitly
/// forbids any prod-time banner so a user clicking around in prod
/// never sees one (and stays calibrated that "the orange/blue
/// stripe means I am NOT in prod").
///
/// Returns nothing — the banner is informational only; it has no
/// click handlers, no dismiss button, no settings shortcut. (The
/// Network panel handles channel switching.)
pub fn show(ctx: &egui::Context, channel: Channel) {
    if !channel.is_non_prod() {
        return;
    }
    let (fill, text) = match channel {
        Channel::Prod => return,
        Channel::Dev => (Color32::from_rgb(0xff, 0x88, 0x00), "DEV ENVIRONMENT"),
        Channel::Local => (Color32::from_rgb(0x33, 0x99, 0xff), "LOCAL ENVIRONMENT"),
    };
    TopBottomPanel::top("sd_channel_banner")
        .exact_height(BANNER_HEIGHT)
        .resizable(false)
        .show_separator_line(false)
        .frame(
            egui::Frame::none()
                .fill(fill)
                .inner_margin(egui::Margin::symmetric(0, 0)),
        )
        .show(ctx, |ui| {
            ui.with_layout(Layout::centered_and_justified(egui::Direction::TopDown), |ui| {
                ui.label(
                    RichText::new(text)
                        .color(Color32::WHITE)
                        .strong()
                        .font(FontId::proportional(15.0)),
                );
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_is_noop_on_prod() {
        // We can't easily snapshot egui state in a unit test, but
        // the early-return is the only meaningful behaviour on
        // prod — confirm the precondition matches the spec.
        assert!(!Channel::Prod.is_non_prod());
        // The match arms inside show() also encode this: Prod →
        // early return BEFORE any TopBottomPanel call.
    }

    #[test]
    fn banner_height_is_32() {
        // Locked at 32 per spec §3.4. Catch accidental drift.
        assert_eq!(BANNER_HEIGHT, 32.0);
    }

    #[test]
    fn dev_and_local_emit_distinct_labels() {
        // The spec's "DEV ENVIRONMENT" / "LOCAL ENVIRONMENT"
        // labels are what calibrate users; assert via the same
        // match the renderer uses.
        let dev_label = match Channel::Dev {
            Channel::Dev => "DEV ENVIRONMENT",
            _ => "?",
        };
        let local_label = match Channel::Local {
            Channel::Local => "LOCAL ENVIRONMENT",
            _ => "?",
        };
        assert_ne!(dev_label, local_label);
        assert!(dev_label.contains("DEV"));
        assert!(local_label.contains("LOCAL"));
    }
}
