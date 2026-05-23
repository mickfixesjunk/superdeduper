//! "Scan complete" chime.
//!
//! Plays a brief, deliberately calm two-note tone when the scan
//! finishes — designed to be audible from the next room without
//! sounding like an alarm. The shape is a perfect fifth (C5 then
//! G5) with a soft attack and exponential decay so it reads as a
//! "ding" rather than a "BEEP". Total length ~700 ms; peak
//! amplitude held low so it doesn't startle.
//!
//! Synthesis is done in-process — no asset file embedded. That
//! keeps the binary lean and avoids any licensing concerns about
//! a bundled WAV.
//!
//! The whole thing runs on a detached thread; failures (no audio
//! device, locked WASAPI, etc.) are swallowed silently because a
//! missing dedup-done sound should never break the scan path.

use std::f32::consts::TAU;
use std::time::Duration;

const SAMPLE_RATE: u32 = 44_100;

/// Spawn a thread, synthesize the chime, play it through the
/// default audio device, return immediately. Drops the OutputStream
/// when the sound finishes (rodio requires the stream to outlive
/// the sink, but we sleep_until_end before letting it go).
pub fn play_done_chime() {
    std::thread::spawn(|| {
        // try_default returns Err if no audio device is available
        // (headless box, locked device, WSL2 without ALSA passthrough).
        // Silently bail — a missing chime isn't a failure mode worth
        // surfacing through the GUI.
        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            return;
        };

        let samples = synth_chime();
        sink.append(rodio::buffer::SamplesBuffer::new(1, SAMPLE_RATE, samples));
        // Lower the global volume on top of the per-sample envelope
        // — keeps the chime from being startling on systems where
        // the user already has output volume cranked.
        sink.set_volume(0.6);
        sink.sleep_until_end();
    });
}

/// Generate mono f32 PCM samples for a ~700 ms chime: 200 ms of C5
/// then 500 ms of G5 (perfect-fifth ascent — sounds open and
/// resolved, not alarming). Both notes use a soft cosine fade-in
/// and an exponential decay envelope to remove any harsh edges.
fn synth_chime() -> Vec<f32> {
    let mut out = Vec::with_capacity((SAMPLE_RATE as usize * 7) / 10);
    render_note(&mut out, 523.25, Duration::from_millis(200)); // C5
    render_note(&mut out, 783.99, Duration::from_millis(500)); // G5
    out
}

/// "Fast-forward begins" — deep dystopian synth swell that fires
/// once when the cache catches up to a paused resume. Aesthetic
/// reference: Brad Fiedel / Terminator 2 score — low detuned sines
/// + a quick high-frequency sweep + slight noise grit. Slow attack,
/// no melody, just atmospheric weight.
pub fn play_fastforward_start() {
    std::thread::spawn(|| {
        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            return;
        };
        let samples = synth_fastforward_swell();
        sink.append(rodio::buffer::SamplesBuffer::new(1, SAMPLE_RATE, samples));
        // Higher than the original 0.35 so the swell actually
        // registers over OS notification volume + browser-tab
        // background.
        sink.set_volume(0.7);
        sink.sleep_until_end();
    });
}

/// "Catch-up complete" — a single resonant metallic hit with
/// inharmonic partials and a long exponential decay. Reads as a
/// dystopian "thwomp" rather than a cheerful chime. Same aesthetic
/// family as the swell.
pub fn play_caught_up() {
    std::thread::spawn(|| {
        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            return;
        };
        let samples = synth_metallic_hit();
        sink.append(rodio::buffer::SamplesBuffer::new(1, SAMPLE_RATE, samples));
        sink.set_volume(0.85);
        sink.sleep_until_end();
    });
}

/// 500ms swell. A noise-burst transient + mid-bass metallic ring at
/// 110Hz / 220Hz / 165Hz with detune-induced beating, plus a brief
/// pitch-bent ring on top. Mid-frequency-heavy so it actually
/// reaches laptop and desktop speakers without sounding like a
/// distant chirp.
fn synth_fastforward_swell() -> Vec<f32> {
    let dur_s = 0.50;
    let n = (SAMPLE_RATE as f32 * dur_s) as usize;
    let mut out = Vec::with_capacity(n);
    let fade_in = (SAMPLE_RATE as f32 * 0.012) as usize;

    // Mid-bass to low-mid frequencies. Audible on every speaker.
    // The intervals are not quite a perfect chord — slight detuning
    // creates the "industrial / sci-fi" beating instead of a tonal
    // arpeggio.
    let f_a = 110.0; // A2 fundamental
    let f_b = 165.5; // ≈ E3 — perfect-fifth above, slight detune
    let f_c = 221.5; // ≈ A3 — octave + ~1.5 Hz beat against f_a*2
    let decay_a = 0.32;
    let decay_b = 0.22;
    let decay_c = 0.16;

    let mut noise_seed: u32 = 0x1357_9bdf;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let ring_a = (TAU * f_a * t).sin() * (-t / decay_a).exp();
        let ring_b = (TAU * f_b * t).sin() * (-t / decay_b).exp();
        let ring_c = (TAU * f_c * t).sin() * (-t / decay_c).exp();
        let ring = 0.55 * ring_a + 0.32 * ring_b + 0.22 * ring_c;

        // Sharp opening noise burst — gone in ~30ms. This is the
        // "thunk" that makes the swell feel like something
        // mechanical engaging rather than a tone fading in.
        noise_seed ^= noise_seed << 13;
        noise_seed ^= noise_seed >> 17;
        noise_seed ^= noise_seed << 5;
        let n_sample = (noise_seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let transient = 0.32 * n_sample * (1.0 - (t / 0.030).min(1.0));

        let attack = if i < fade_in {
            (i as f32 / fade_in as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        out.push(attack * (ring * 0.85 + transient));
    }
    out
}

/// 700ms metallic resonant hit. Mid-fundamental at A3 (220Hz) with
/// inharmonic partials at 2.756×, 5.404×, 8.933× — same ratios as
/// before but pitched up an octave so the hit punches through any
/// speaker. Sharp transient + long decay tail.
fn synth_metallic_hit() -> Vec<f32> {
    let dur_s = 0.70;
    let n = (SAMPLE_RATE as f32 * dur_s) as usize;
    let mut out = Vec::with_capacity(n);
    let fade_in = (SAMPLE_RATE as f32 * 0.003) as usize;

    // A3 fundamental + Helmholtz-style inharmonic partials. Each
    // mode has its own decay so higher modes die off first.
    let modes: &[(f32, f32, f32)] = &[
        (220.0, 0.55, 0.48),     // A3 fundamental
        (606.32, 0.30, 0.22),    // 2.756 × first inharmonic
        (1188.88, 0.18, 0.14),   // 5.404 ×
        (1965.26, 0.10, 0.08),   // 8.933 ×
    ];

    let mut noise_seed: u32 = 0xbadd_cafe;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let mut s = 0.0;
        for &(freq, amp, decay) in modes {
            s += amp * (TAU * freq * t).sin() * (-t / decay).exp();
        }
        // Initial impulse — sharper and louder than the swell so
        // the moment of catch-up reads as a HIT, not a hum.
        noise_seed ^= noise_seed << 13;
        noise_seed ^= noise_seed >> 17;
        noise_seed ^= noise_seed << 5;
        let n_sample = (noise_seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let transient = 0.40 * n_sample * (1.0 - (t / 0.020).min(1.0));

        let attack = if i < fade_in {
            (i as f32 / fade_in as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        out.push(attack * (s * 0.55 + transient));
    }
    out
}

fn render_note(out: &mut Vec<f32>, freq_hz: f32, dur: Duration) {
    let n = (SAMPLE_RATE as f32 * dur.as_secs_f32()) as usize;
    // 20 ms fade-in is long enough to avoid an audible click on
    // start but short enough that the note feels intentional, not
    // sluggish.
    let fade_in = (SAMPLE_RATE as f32 * 0.020) as usize;
    // Exponential decay constant; smaller = faster fade-out. 0.25 s
    // half-life gives a bell-like tail without dragging.
    let decay = 0.25;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let phase = TAU * freq_hz * t;
        // Soft attack: cosine ease-in over fade_in samples.
        let attack = if i < fade_in {
            0.5 - 0.5 * ((i as f32 / fade_in as f32) * std::f32::consts::PI).cos()
        } else {
            1.0
        };
        let env = attack * (-t / decay).exp();
        // 0.25 peak amplitude — comfortable at most system volumes.
        out.push(0.25 * env * phase.sin());
    }
}
