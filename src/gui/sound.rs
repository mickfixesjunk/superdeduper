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

/// "Fast-forward in progress" — short rising chirp, ~120 ms,
/// square-wave timbre. Fires once when the engine starts fast-
/// forwarding through cached hashes. Computerised feel rather than
/// musical: square waves + fast frequency sweep evoke a "data
/// transfer" sound from early-90s software.
pub fn play_fastforward_start() {
    std::thread::spawn(|| {
        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            return;
        };
        let samples = synth_fastforward_start();
        sink.append(rodio::buffer::SamplesBuffer::new(1, SAMPLE_RATE, samples));
        sink.set_volume(0.4);
        sink.sleep_until_end();
    });
}

/// "Caught up" — three ascending notes (C5 → E5 → G5, major triad
/// arpeggio) with the square-wave timbre that matches the fast-
/// forward start chirp. Reads as "transfer complete." ~450 ms.
pub fn play_caught_up() {
    std::thread::spawn(|| {
        let Ok((_stream, handle)) = rodio::OutputStream::try_default() else {
            return;
        };
        let Ok(sink) = rodio::Sink::try_new(&handle) else {
            return;
        };
        let samples = synth_caught_up();
        sink.append(rodio::buffer::SamplesBuffer::new(1, SAMPLE_RATE, samples));
        sink.set_volume(0.5);
        sink.sleep_until_end();
    });
}

/// Render a square-wave note with the same fade-in/decay envelope as
/// `render_note` but a fundamentally chiptune timbre — the harmonics
/// of a square wave read as "computerised" rather than "tonal".
fn render_square_note(out: &mut Vec<f32>, freq_hz: f32, dur: Duration) {
    let n = (SAMPLE_RATE as f32 * dur.as_secs_f32()) as usize;
    let fade_in = (SAMPLE_RATE as f32 * 0.010) as usize;
    let decay = 0.18;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let phase = TAU * freq_hz * t;
        // Square wave = sign of the sine. Adds the odd-harmonic
        // overtones that give the "8-bit" character. Slight low-
        // pass via 0.7 multiplier on the alternating-amplitude so
        // it's not piercingly bright.
        let sq = if phase.sin() >= 0.0 { 1.0 } else { -1.0 };
        let attack = if i < fade_in {
            0.5 - 0.5 * ((i as f32 / fade_in as f32) * std::f32::consts::PI).cos()
        } else {
            1.0
        };
        let env = attack * (-t / decay).exp();
        out.push(0.18 * env * sq * 0.7);
    }
}

fn synth_fastforward_start() -> Vec<f32> {
    // Frequency sweep from A4 → A5 over 120 ms. Quick rise reads as
    // "engaging" without taking up enough time to be annoying when
    // it fires.
    let n = (SAMPLE_RATE as f32 * 0.120) as usize;
    let mut out = Vec::with_capacity(n);
    let start_freq = 440.0;
    let end_freq = 880.0;
    let fade_in = (SAMPLE_RATE as f32 * 0.005) as usize;
    let fade_out = (SAMPLE_RATE as f32 * 0.040) as usize;
    let mut phase_accum = 0.0;
    for i in 0..n {
        let t = i as f32 / SAMPLE_RATE as f32;
        let freq = start_freq + (end_freq - start_freq) * (t / 0.120).min(1.0);
        phase_accum += TAU * freq / SAMPLE_RATE as f32;
        let sq = if phase_accum.sin() >= 0.0 { 1.0 } else { -1.0 };
        let attack = if i < fade_in {
            (i as f32 / fade_in as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        let release = if i + fade_out > n {
            ((n - i) as f32 / fade_out as f32).clamp(0.0, 1.0)
        } else {
            1.0
        };
        out.push(0.2 * attack * release * sq);
    }
    out
}

fn synth_caught_up() -> Vec<f32> {
    let mut out = Vec::with_capacity((SAMPLE_RATE as usize * 5) / 10);
    render_square_note(&mut out, 523.25, Duration::from_millis(110)); // C5
    render_square_note(&mut out, 659.25, Duration::from_millis(110)); // E5
    render_square_note(&mut out, 783.99, Duration::from_millis(220)); // G5
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
