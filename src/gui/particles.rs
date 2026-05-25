//! Resume cache-fast-forward visual effect.
//!
//! Strictly gated on:
//! 1. The current scan is a RESUME (set when accept_resume launches the engine).
//! 2. The OverallProgress rate exceeds the cache-fast-forward threshold.
//! 3. We have a bar fill rect to anchor to.
//!
//! Particles are clipped to the filled portion of the progress bar
//! and use the app's accent palette (no gold, no rainbow). When
//! catch-up completes (rate drops below the lower hysteresis bound),
//! the effect stops entirely and the bar returns to its normal
//! render until end of scan.

use std::time::Instant;

use egui::{Color32, Pos2, Rect, Ui};

use crate::gui::theme;

#[derive(Clone, Copy)]
struct Particle {
    pos: Pos2,
    vel: egui::Vec2,
    age: f32,
    lifetime: f32,
    color: Color32,
    radius: f32,
}

pub struct Sparkles {
    particles: Vec<Particle>,
    last_files: u64,
    last_frame: Option<Instant>,
    rate_ewma: f32,
    fast_forwarding: bool,
    /// Set on the first tick that flips fast_forwarding to true.
    /// Used to enforce a hard ceiling on how long the effect runs
    /// — real hashing on small files can sustain rates above any
    /// reasonable rate threshold, so the rate alone isn't a reliable
    /// exit signal. Cap at FF_MAX_DURATION seconds.
    started_at: Option<Instant>,
}

/// Hard ceiling on resume catch-up effect duration. The actual
/// cache fast-forward is usually well under a second; after this
/// long we KNOW the cache phase is done even if some other signal
/// keeps the rate elevated.
const FF_MAX_DURATION_SECS: f32 = 3.0;

#[derive(Debug, Clone, Copy, Default)]
pub struct SparkleSignals {
    pub entered_fast_forward: bool,
    pub left_fast_forward: bool,
}

impl Default for Sparkles {
    fn default() -> Self {
        Self {
            particles: Vec::with_capacity(128),
            last_files: 0,
            last_frame: None,
            rate_ewma: 0.0,
            fast_forwarding: false,
            started_at: None,
        }
    }
}

impl Sparkles {
    /// Reset between scans so the rate counter doesn't carry stale
    /// state across a launch/cancel/relaunch cycle.
    pub fn reset(&mut self) {
        self.particles.clear();
        self.last_files = 0;
        self.last_frame = None;
        self.rate_ewma = 0.0;
        self.fast_forwarding = false;
        self.started_at = None;
    }

    /// Update once per frame. `fill_rect` is the progress-bar fill
    /// rectangle (the gauge's coloured portion). Particles spawn
    /// inside it and are clipped to it on render.
    ///
    /// Returns transition signals — caller plays sound effects on
    /// `entered_fast_forward` / `left_fast_forward`.
    pub fn tick(&mut self, files_done: u64, fill_rect: Option<Rect>) -> SparkleSignals {
        let now = Instant::now();
        let dt = match self.last_frame {
            Some(prev) => now.saturating_duration_since(prev).as_secs_f32().max(0.001),
            None => 0.016,
        };
        self.last_frame = Some(now);

        let delta = files_done.saturating_sub(self.last_files);
        self.last_files = files_done;
        let rate = delta as f32 / dt;
        // Faster EWMA (more weight on new) so we exit fast-forward
        // quickly when real hashing starts — previously the 0.6/0.4
        // split kept the bar red for seconds past the actual
        // transition because the old rate decayed too slowly.
        self.rate_ewma = 0.35 * self.rate_ewma + 0.65 * rate;

        let was_ff = self.fast_forwarding;
        // Enter on rate spike. Exit on EITHER (a) hard duration cap
        // (FF_MAX_DURATION_SECS) OR (b) rate drop. Rate alone isn't
        // reliable as an exit signal — Tier 1 over many small files
        // with 32 rayon workers can sustain 5K+ files/sec, well
        // above any reasonable "not fast-forwarding" threshold. The
        // time cap guarantees the effect always terminates within a
        // few seconds.
        if !self.fast_forwarding && self.rate_ewma > 2_000.0 {
            self.fast_forwarding = true;
            self.started_at = Some(now);
        } else if self.fast_forwarding {
            let too_long = self
                .started_at
                .map(|t| now.saturating_duration_since(t).as_secs_f32() > FF_MAX_DURATION_SECS)
                .unwrap_or(false);
            if too_long || self.rate_ewma < 800.0 {
                self.fast_forwarding = false;
                self.started_at = None;
            }
        }
        let signals = SparkleSignals {
            entered_fast_forward: !was_ff && self.fast_forwarding,
            left_fast_forward: was_ff && !self.fast_forwarding,
        };

        if self.fast_forwarding {
            if let Some(rect) = fill_rect {
                if rect.width() > 4.0 {
                    self.emit_inside_bar(rect, dt);
                }
            }
        }
        // Catch-up moment: emit a final dense burst with a brighter
        // accent palette so the colour-flip away from red has a
        // visual exclamation mark. After this burst, no more
        // emission for the rest of the scan.
        if signals.left_fast_forward {
            if let Some(rect) = fill_rect {
                if rect.width() > 4.0 {
                    self.emit_catch_up_burst(rect);
                }
            }
        }

        // Tick particles. No gravity — keep them inside the bar.
        for p in self.particles.iter_mut() {
            p.pos += p.vel * dt;
            p.age += dt;
            // Slight velocity damping so particles stall and fade in
            // place instead of drifting out of bounds.
            p.vel *= (1.0 - 1.4 * dt).max(0.0);
        }
        self.particles.retain(|p| {
            if p.age >= p.lifetime {
                return false;
            }
            // Drop anything outside the bar fill rect on the most
            // recent frame. The caller passes the current fill rect
            // each tick, so this is up-to-date even as the bar
            // grows during fast-forward.
            if let Some(rect) = fill_rect {
                if p.pos.x < rect.min.x - 2.0
                    || p.pos.x > rect.max.x + 2.0
                    || p.pos.y < rect.min.y - 2.0
                    || p.pos.y > rect.max.y + 2.0
                {
                    return false;
                }
            }
            true
        });

        signals
    }

    pub fn is_fast_forwarding(&self) -> bool {
        self.fast_forwarding
    }

    /// Force a catch-up burst + flip out of fast-forward state.
    /// Used by the caller when the scan finishes while still in
    /// fast-forward (a corpus where every file was cache-cached —
    /// the rate stays high through the entire scan and never
    /// naturally drops below the exit threshold).
    pub fn force_catch_up(&mut self, fill_rect: Option<Rect>) {
        if !self.fast_forwarding {
            return;
        }
        self.fast_forwarding = false;
        if let Some(rect) = fill_rect {
            if rect.width() > 4.0 {
                self.emit_catch_up_burst(rect);
            }
        }
    }

    pub fn active(&self) -> bool {
        !self.particles.is_empty() || self.fast_forwarding
    }

    fn emit_inside_bar(&mut self, fill_rect: Rect, dt: f32) {
        // Modest emission — the effect should read as "the bar is
        // alive with data flowing inside it" not "a fireworks
        // display." Cap aggressively so 100K-file fast-forwards
        // don't carpet the screen.
        let pps = (self.rate_ewma / 1_500.0).clamp(15.0, 80.0);
        let target_emit = (pps * dt).ceil() as u32;

        let mut seed = (self.rate_ewma as u32)
            .wrapping_add(self.particles.len() as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(1);
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };

        // Spawn band: the rightmost ~12% of the fill, where the
        // bar's leading edge is. Visually "data is being processed
        // here, and trailing through the bar".
        let lead_x = fill_rect.max.x;
        let band_w = (fill_rect.width() * 0.12).max(8.0);
        let band_left = (lead_x - band_w).max(fill_rect.min.x);

        for _ in 0..target_emit.max(1) {
            // Bias horizontal spawn toward the leading edge.
            let r = rand();
            let bias = r * r; // squared bias = denser near lead_x
            let x = band_left + (1.0 - bias) * band_w;
            // Vertical: keep them clearly inside the bar.
            let y_inset = fill_rect.height() * 0.25;
            let y = fill_rect.min.y + y_inset + rand() * (fill_rect.height() - 2.0 * y_inset);

            // Small leftward drift + tiny vertical jitter. Damping
            // in tick() stalls them so they fade in place inside
            // the bar.
            let vx = -20.0 - rand() * 40.0;
            let vy = (rand() - 0.5) * 12.0;

            let lifetime = 0.35 + rand() * 0.45;

            // Theme palette only. Two cyan-family shades + one
            // electric-blue accent. No gold, no warn-amber, no
            // hot-pink. Occasional bright off-white for sparkle.
            let color = match (rand() * 6.0) as u32 {
                0 => theme::ACCENT,                       // primary teal
                1 => theme::ACCENT,                       // weighted
                2 => Color32::from_rgb(0x69, 0x9b, 0xff), // electric blue
                3 => Color32::from_rgb(0xa0, 0xc6, 0xff), // pale blue
                _ => Color32::from_rgb(0xe8, 0xee, 0xf5), // text-hi off-white
            };

            self.particles.push(Particle {
                pos: egui::pos2(x, y),
                vel: egui::vec2(vx, vy),
                age: 0.0,
                lifetime,
                color,
                radius: 1.3 + rand() * 1.6,
            });
        }
    }

    /// One-shot burst at the moment of catch-up. Brighter palette
    /// (off-white + cyan), faster radial spread, longer lifetimes
    /// so the transition reads as a clear "snap" away from the red
    /// fast-forward bar and back to normal teal.
    fn emit_catch_up_burst(&mut self, fill_rect: Rect) {
        let mut seed = (self.particles.len() as u32)
            .wrapping_mul(0x9E37_79B9)
            .wrapping_add(0xc0ff_eebb);
        let mut rand = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as f32 / u32::MAX as f32
        };
        let count = 60;
        for _ in 0..count {
            // Spawn anywhere along the full filled bar — celebratory
            // sweep across the catch-up extent, not just the lead.
            let x = fill_rect.min.x + rand() * fill_rect.width();
            let y = fill_rect.min.y + 0.15 * fill_rect.height() + rand() * 0.7 * fill_rect.height();
            // Radial outward velocity (gentle), no gravity.
            let angle = rand() * std::f32::consts::TAU;
            let speed = 30.0 + rand() * 60.0;
            let vx = angle.cos() * speed;
            let vy = angle.sin() * speed * 0.3; // squashed so it stays in-bar
            let color = match (rand() * 3.0) as u32 {
                0 => Color32::from_rgb(0xe8, 0xee, 0xf5), // bright off-white
                1 => theme::ACCENT,                       // teal "all clear"
                _ => Color32::from_rgb(0xa0, 0xc6, 0xff), // pale blue
            };
            self.particles.push(Particle {
                pos: egui::pos2(x, y),
                vel: egui::vec2(vx, vy),
                age: 0.0,
                lifetime: 0.55 + rand() * 0.4,
                color,
                radius: 1.8 + rand() * 1.8,
            });
        }
    }

    /// Render every particle. Caller is expected to provide a Ui
    /// (typically inside an Area on the Foreground layer). Particles
    /// outside `fill_rect` are skipped — defensive in case the bar
    /// shrank between tick and paint.
    pub fn paint(&self, ui: &Ui, fill_rect: Option<Rect>) {
        if self.particles.is_empty() {
            return;
        }
        let mut painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("sd-sparkles"),
        ));
        if let Some(rect) = fill_rect {
            painter.set_clip_rect(rect);
        }
        let _ = &mut painter;
        for p in &self.particles {
            let t = (p.age / p.lifetime).clamp(0.0, 1.0);
            // Quick fade-in (first 15% of life) then steady decay.
            let alpha_curve = if t < 0.15 {
                t / 0.15
            } else {
                ((1.0 - t) / 0.85).powf(1.2)
            };
            let alpha = (alpha_curve * 200.0).clamp(0.0, 220.0) as u8;
            let radius = p.radius * alpha_curve.max(0.2);
            if radius <= 0.2 {
                continue;
            }
            let c = Color32::from_rgba_unmultiplied(p.color.r(), p.color.g(), p.color.b(), alpha);
            // Soft halo (1/3 alpha, larger radius) + crisp core.
            painter.circle_filled(
                p.pos,
                radius * 1.5,
                Color32::from_rgba_unmultiplied(p.color.r(), p.color.g(), p.color.b(), alpha / 4),
            );
            painter.circle_filled(p.pos, radius, c);
        }
    }
}
