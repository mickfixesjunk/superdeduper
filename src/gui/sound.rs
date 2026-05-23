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
        sink.set_volume(0.35);
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
        sink.set_volume(0.45);
        sink.sleep_until_end();
    });
}

/// 350ms swell. Two near-detuned bass sines (slow beating) plus a
/// pitch-falling high "whoosh" overlay. Slow cosine attack, longer
/// exponential decay. The combination evokes a metallic hatch
/// closing / power-system engaging — Fiedel-coded.
fn synth_fastforward_swell() -> Vec<f32> {
    let dur_s = 0.35;
    let n = (SAMPLE_RATE as f32 * dur_s) as usize;
    let mut out = Vec::with_capacity(n);
    let fade_in_s = 0.06;
    let fade_in = (SAMPLE_RATE as f32 * fade_in_s) as usize;
    let decay = 0.22;

    // Two slightly detuned low fundamentals → slow phase beating,
    // ~3 Hz wobble. Sub-bass weight without being muddy.
    let f_a = 55.0; // A1
    let f_b = 58.27; // A1 + ~100 cents detune (slight)

    // High sweep: 6 kHz → 1.5 kHz over the swell. Falling pitch
    // reads as "rushing in" / "locking on" rather than "rising up".
    let sweep_start = 6_000.0;
    let sweep_end = 1_500.0;
    let mut sweep_phase = 0.0_f32;

    let mut noise_seed: u32 = 0x1357_9bdf;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let phase_a = TAU * f_a * t;
        let phase_b = TAU * f_b * t;
        let bass = 0.55 * phase_a.sin() + 0.45 * phase_b.sin();

        let sweep_freq = sweep_start + (sweep_end - sweep_start) * (t / dur_s).min(1.0);
        sweep_phase += TAU * sweep_freq / SAMPLE_RATE as f32;
        // High sine fades faster than the bass so the tail is
        // mostly low-end.
        let sweep_env = (1.0 - (t / 0.18).min(1.0)).powf(1.8);
        let sweep = 0.18 * sweep_env * sweep_phase.sin();

        // Tiny white-noise grit, low-pass-ish via averaging two
        // samples. Adds metallic "air" without being audible as
        // hiss.
        noise_seed ^= noise_seed << 13;
        noise_seed ^= noise_seed >> 17;
        noise_seed ^= noise_seed << 5;
        let n1 = (noise_seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        noise_seed ^= noise_seed << 7;
        let n2 = (noise_seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let grit = 0.04 * (n1 + n2) * 0.5 * (1.0 - (t / 0.10).min(1.0));

        let attack = if i < fade_in {
            0.5 - 0.5 * ((i as f32 / fade_in as f32) * std::f32::consts::PI).cos()
        } else {
            1.0
        };
        let env = attack * (-t / decay).exp();
        out.push(env * (bass * 0.55 + sweep + grit));
    }
    out
}

/// 650ms metallic resonant hit. Fundamental + inharmonic partials
/// (2.756x, 5.404x, 8.933x — non-integer ratios are what makes a
/// hit sound "metallic" rather than "tonal"). Sharp 4ms attack +
/// long exponential decay. A muffled hammer on a bulkhead.
fn synth_metallic_hit() -> Vec<f32> {
    let dur_s = 0.65;
    let n = (SAMPLE_RATE as f32 * dur_s) as usize;
    let mut out = Vec::with_capacity(n);
    let fade_in = (SAMPLE_RATE as f32 * 0.004) as usize;

    // Fundamental + Helmholtz-style inharmonic partials, each with
    // its own decay so the higher modes die off first (matches a
    // real metal-plate impulse response).
    let modes: &[(f32, f32, f32)] = &[
        (130.81, 0.55, 0.42), // C3 fundamental, longest decay
        (360.65, 0.28, 0.20), // 2.756 × — first inharmonic
        (706.93, 0.15, 0.12), // 5.404 ×
        (1168.51, 0.08, 0.06), // 8.933 ×
    ];

    let mut noise_seed: u32 = 0xbadd_cafe;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let mut s = 0.0;
        for &(freq, amp, decay) in modes {
            s += amp * (TAU * freq * t).sin() * (-t / decay).exp();
        }
        // Initial noise burst (transient hammer-strike), gone in
        // the first ~25ms.
        noise_seed ^= noise_seed << 13;
        noise_seed ^= noise_seed >> 17;
        noise_seed ^= noise_seed << 5;
        let n_sample = (noise_seed as f32 / u32::MAX as f32) * 2.0 - 1.0;
        let transient = 0.20 * n_sample * (1.0 - (t / 0.025).min(1.0));

        let attack = if i < fade_in {
            (i as f32 / fade_in as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        out.push(attack * (s * 0.45 + transient));
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
