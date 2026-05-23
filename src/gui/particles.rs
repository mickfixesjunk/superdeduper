//! Sparkle particle system for the cache-fast-forward "magical"
//! moment. When the engine is replaying cached hashes on resume,
//! `OverallProgress.done` jumps hundreds or thousands of files per
//! frame. We detect that rate and spawn a burst of fading dots that
//! drift outward — visual confirmation that the cache is doing its
//! job.

use std::time::Instant;

use egui::{Color32, Pos2, Rect, Stroke, Ui};

/// One animated dot.
#[derive(Clone, Copy)]
struct Particle {
    pos: Pos2,
    vel: egui::Vec2,
    /// Seconds since the particle was spawned. Lifetime is implicit
    /// (drop when alpha hits 0).
    age: f32,
    /// Total seconds the particle should be visible. Beyond this, it
    /// gets dropped on the next update.
    lifetime: f32,
    /// Tint applied to the dot. The alpha channel is multiplied by
    /// the fade curve every frame.
    color: Color32,
    /// Pixel radius at age 0. Shrinks linearly to 0 at end-of-life.
    radius: f32,
}

pub struct Sparkles {
    particles: Vec<Particle>,
    last_files: u64,
    last_frame: Option<Instant>,
    /// Rolling rate of files-per-frame. Smoothed so a single big tick
    /// doesn't flicker the sparkles for one frame and stop.
    rate_ewma: f32,
    /// True while we're in the "cache fast-forward" regime. Used to
    /// change particle palette + detect the transition out so the
    /// caller can play the catch-up chime.
    fast_forwarding: bool,
}

/// Events emitted by Sparkles::tick that the caller may want to act
/// on — currently only the catch-up transition, which is the cue to
/// play the "you're synced" chime.
#[derive(Debug, Clone, Copy, Default)]
pub struct SparkleSignals {
    pub entered_fast_forward: bool,
    pub left_fast_forward: bool,
}

impl Default for Sparkles {
    fn default() -> Self {
        Self {
            particles: Vec::with_capacity(256),
            last_files: 0,
            last_frame: None,
            rate_ewma: 0.0,
            fast_forwarding: false,
        }
    }
}

impl Sparkles {
    /// Update on every frame. Feeds the current `files_done` counter
    /// (engine's `OverallProgress.done` while in Hashing). When the
    /// per-frame delta crosses the fast-forward threshold, emits a
    /// burst centred near `anchor`. Returns transition signals so
    /// the caller can play the catch-up chime exactly once when the
    /// cache fast-forward resolves.
    pub fn tick(&mut self, files_done: u64, anchor: Option<Rect>) -> SparkleSignals {
        let now = Instant::now();
        let dt = match self.last_frame {
            Some(prev) => now.saturating_duration_since(prev).as_secs_f32().max(0.001),
            None => 0.016,
        };
        self.last_frame = Some(now);

        let delta = files_done.saturating_sub(self.last_files);
        self.last_files = files_done;
        let rate = delta as f32 / dt;
        // EWMA so the sparkles stay lit across the brief gaps between
        // file-progress events (events fire every 100 files, not every
        // frame, so raw rate would flicker at low frame counts).
        self.rate_ewma = 0.6 * self.rate_ewma + 0.4 * rate;

        // Hysteresis: enter fast-forward at 2000 files/sec, exit at
        // 500. Real hashing rate is tens-to-hundreds of files/sec so
        // the 500 lower bound stays comfortably above normal scans.
        let was_ff = self.fast_forwarding;
        if !self.fast_forwarding && self.rate_ewma > 2_000.0 {
            self.fast_forwarding = true;
        } else if self.fast_forwarding && self.rate_ewma < 500.0 {
            self.fast_forwarding = false;
        }
        let signals = SparkleSignals {
            entered_fast_forward: !was_ff && self.fast_forwarding,
            left_fast_forward: was_ff && !self.fast_forwarding,
        };

        if self.fast_forwarding {
            if let Some(rect) = anchor {
                self.emit_burst(rect, dt);
            }
        }

        // Tick existing particles.
        for p in self.particles.iter_mut() {
            p.pos += p.vel * dt;
            // Slow drift down (gravity-lite) so they fall away from
            // the stat once they've drifted up.
            p.vel.y += 20.0 * dt;
            p.age += dt;
        }
        self.particles.retain(|p| p.age < p.lifetime);

        signals
    }

    pub fn is_fast_forwarding(&self) -> bool {
        self.fast_forwarding
    }

    fn emit_burst(&mut self, anchor: Rect, dt: f32) {
        // Particles-per-second proportional to rate, capped so we
        // don't drown the screen on a 100K-file cache fast-forward.
        let pps = (self.rate_ewma / 200.0).clamp(20.0, 240.0);
        let target_emit = (pps * dt) as u32;
        // PRNG: cheap xorshift seeded from the running rate +
        // particle count. Plenty of jitter for this purpose.
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
        for _ in 0..target_emit.max(1) {
            // Spawn anywhere across the anchor's width, near its top
            // edge — particles drift up and to the side from there.
            let x = anchor.min.x + rand() * anchor.width();
            let y = anchor.min.y + anchor.height() * (0.6 + 0.4 * rand());
            // Initial velocity: mostly upward, slight horizontal jitter.
            let vx = (rand() - 0.5) * 80.0;
            let vy = -40.0 - rand() * 80.0;
            let lifetime = 0.6 + rand() * 0.7;
            // Fast-forward palette: gold + cyan + white. Bright,
            // "data-stream" coded — distinct from the normal scan
            // colours so the user sees "this is the cache catching
            // up" instead of just generic activity.
            let color = match (rand() * 4.0) as u32 {
                0 => Color32::from_rgb(0xff, 0xd7, 0x3a), // gold
                1 => Color32::from_rgb(0x4f, 0xd1, 0xc5), // cyan/teal
                2 => Color32::from_rgb(0x69, 0x9b, 0xff), // electric blue
                _ => Color32::from_rgb(0xff, 0xfa, 0xe6), // bright white
            };
            self.particles.push(Particle {
                pos: egui::pos2(x, y),
                vel: egui::vec2(vx, vy),
                age: 0.0,
                lifetime,
                color,
                radius: 2.5 + rand() * 2.5,
            });
        }
    }

    /// Render every live particle as a fading dot. Call at the end
    /// of the render pass so sparkles sit on top of the header /
    /// stats / progress bar.
    pub fn paint(&self, ui: &Ui) {
        if self.particles.is_empty() {
            return;
        }
        let painter = ui.ctx().layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("sd-sparkles"),
        ));
        for p in &self.particles {
            let t = (p.age / p.lifetime).clamp(0.0, 1.0);
            let alpha = ((1.0 - t).powf(1.5) * 220.0) as u8;
            let radius = p.radius * (1.0 - t).max(0.0);
            if radius <= 0.0 {
                continue;
            }
            let c = Color32::from_rgba_unmultiplied(p.color.r(), p.color.g(), p.color.b(), alpha);
            // Soft halo + bright core for the "shimmer" feel.
            painter.circle_filled(
                p.pos,
                radius * 1.7,
                Color32::from_rgba_unmultiplied(p.color.r(), p.color.g(), p.color.b(), alpha / 3),
            );
            painter.circle(p.pos, radius, c, Stroke::NONE);
        }
    }

    /// True while there are particles still on screen — used by the
    /// caller to schedule a repaint so the animation runs smoothly
    /// even when no other event is firing.
    pub fn active(&self) -> bool {
        !self.particles.is_empty() || self.rate_ewma > 200.0
    }
}
