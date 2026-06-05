//! Acoustic audio fingerprinting — first slice of GH #26 (T1.3).
//!
//! V1 scope (this commit):
//!   * `hash_file(path) -> Result<AudioFingerprint, HashError>`
//!     — decode via symphonia, fingerprint via rusty-chromaprint.
//!   * `hamming_distance(a, b) -> u32` for similarity scoring.
//!   * Audio-extension allowlist constant per spec §3.
//!   * Unit tests against synthetic PCM data (no codec dep) +
//!     extension-detection.
//!
//! V1 explicitly does NOT include:
//!   * Tier-4 pipeline integration (next sub-deliverable)
//!   * BK-tree near-neighbour index (spec §4.4)
//!   * Cache integration (spec §4.5)
//!   * GUI scan-mode dropdown wiring (already exists from #25 v2.5)
//!
//! Behind the `similar-audio` cargo feature so the always-on
//! dedup pipeline doesn't pull the audio-codec dep tree it
//! doesn't need (symphonia + transitive deps are ~2MB).
//!
//! ## Algorithm shape
//!
//! Chromaprint produces a sequence of 32-bit fingerprint chunks
//! (one every ~0.124s of audio at 11025Hz). The spec's matching
//! is Hamming distance over the chunk sequences — i.e. align two
//! chunk arrays and count bit-flips per pair. For v1 we expose the
//! raw fingerprint (Vec<u32>) + a per-chunk hamming helper; the
//! caller can implement whichever matching strategy (full-sequence,
//! aligned-30s-prefix, etc.) the Tier-4 integration needs.
//!
//! ## Audio format support
//!
//! Per spec §3, v1 supports MP3, M4A/AAC (via ISO-MP4 container +
//! AAC codec), FLAC, WAV, OGG-Vorbis. OPUS + WMA listed in the
//! spec but symphonia's pure-Rust support for those is less mature
//! and may need symphonia-adapter-libopus / fdk-aac — left for a
//! follow-up if user demand surfaces.

#![cfg(feature = "similar-audio")]

pub mod tier4;

use std::path::Path;

/// Profiling instrumentation for the audio Tier-4 hot loop. Behind
/// a runtime check on env var SUPERDEDUPER_AUDIO_PROFILE so it adds
/// zero meaningful cost when off (one branch-not-taken per phase
/// per chunk). Thread-local accumulators so it's correct under
/// rayon par_iter — Tier-4's normal call shape.
///
/// Phases tracked:
///   decode   — Symphonia next_packet + decoder.decode()
///   mixdown  — sample-format match arm + mono mix-down
///   resample — rubato process_into_buffer / process_partial_into_buffer
///   pcm      — f32 → i16 clamp+round map into pcm_buf
///   chroma   — chromaprint consume()
///
/// Snapshot via [`profile::snapshot`] after a batch of `hash_file`
/// calls on a single thread; reset between batches with
/// [`profile::reset`]. See `examples/audio_profile.rs` for a working
/// driver.
///
/// Why this exists: the 2026-05-26 audio-resample-opt arc proved
/// that "rubato is the bottleneck" assumptions were wrong — profile
/// data showed Symphonia decode at 60-65% on MP3/OGG, with rubato
/// always #3 at most. Permanent instrumentation makes the next
/// "where does audio dedup spend its time?" question a 30-second
/// answer instead of a 30-minute reinstrument cycle.
pub mod profile {
    use std::cell::Cell;
    use std::sync::OnceLock;

    /// Returns true iff SUPERDEDUPER_AUDIO_PROFILE is set in the
    /// process environment. Cached after first read.
    pub fn enabled() -> bool {
        static FLAG: OnceLock<bool> = OnceLock::new();
        *FLAG.get_or_init(|| std::env::var("SUPERDEDUPER_AUDIO_PROFILE").is_ok())
    }

    thread_local! {
        pub static T_DECODE: Cell<u64> = const { Cell::new(0) };
        pub static T_MIXDOWN: Cell<u64> = const { Cell::new(0) };
        pub static T_RESAMPLE: Cell<u64> = const { Cell::new(0) };
        pub static T_PCM: Cell<u64> = const { Cell::new(0) };
        pub static T_CHROMA: Cell<u64> = const { Cell::new(0) };
    }

    #[inline]
    pub fn add(slot: &'static std::thread::LocalKey<Cell<u64>>, micros: u64) {
        slot.with(|c| c.set(c.get() + micros));
    }

    /// Snapshot current thread's accumulators (micros) as
    /// (decode, mixdown, resample, pcm, chroma).
    pub fn snapshot() -> (u64, u64, u64, u64, u64) {
        (
            T_DECODE.with(|c| c.get()),
            T_MIXDOWN.with(|c| c.get()),
            T_RESAMPLE.with(|c| c.get()),
            T_PCM.with(|c| c.get()),
            T_CHROMA.with(|c| c.get()),
        )
    }

    /// Zero all phase accumulators on the current thread.
    pub fn reset() {
        T_DECODE.with(|c| c.set(0));
        T_MIXDOWN.with(|c| c.set(0));
        T_RESAMPLE.with(|c| c.set(0));
        T_PCM.with(|c| c.set(0));
        T_CHROMA.with(|c| c.set(0));
    }
}

/// Audio extensions Tier-4 will fingerprint. Matches spec §3 v1
/// scope, minus OPUS + WMA (deferred — symphonia core doesn't
/// decode them; needs FDK-AAC / libopus adapters that we haven't
/// adopted yet). Compared lowercase so `.MP3` matches.
pub const AUDIO_EXTENSIONS: &[&str] = &["mp3", "m4a", "aac", "flac", "wav", "ogg"];

/// Default Hamming-distance threshold per spec §3: "5 out of 32
/// bits per fingerprint chunk — czkawka's calibrated default."
pub const DEFAULT_THRESHOLD: u32 = 5;

/// A Chromaprint fingerprint — sequence of 32-bit chunks. Stored
/// raw so the caller can implement sequence-alignment matching.
/// One chunk represents ~0.124s of audio at chromaprint's
/// canonical 11025Hz mono sample rate.
pub type AudioFingerprint = Vec<u32>;

/// True if `path` ends with one of the [`AUDIO_EXTENSIONS`].
/// Case-insensitive.
pub fn is_audio_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let lower = ext.to_ascii_lowercase();
            AUDIO_EXTENSIONS.contains(&lower.as_str())
        }
        None => false,
    }
}

/// Hamming distance between two fingerprint chunks. Sequence-level
/// matching (over Vec<u32>) is the caller's responsibility — there
/// are multiple ways to align (full overlap, longest common
/// subchunk, sliding window) and Tier-4 will pick the right one
/// per the v1 audio-pipeline spec.
pub fn hamming_distance_chunk(a: u32, b: u32) -> u32 {
    (a ^ b).count_ones()
}

/// Average chunk-Hamming distance between two fingerprints. v1
/// matching helper — divides the bit-flip count by the number of
/// aligned chunks. Caller passes fingerprints; the shorter one's
/// length determines the alignment window. Returns 0 if either
/// fingerprint is empty.
pub fn average_hamming_distance(a: &AudioFingerprint, b: &AudioFingerprint) -> f64 {
    let n = a.len().min(b.len());
    if n == 0 {
        return 0.0;
    }
    let total_bits: u32 = a[..n]
        .iter()
        .zip(b[..n].iter())
        .map(|(x, y)| hamming_distance_chunk(*x, *y))
        .sum();
    total_bits as f64 / n as f64
}

/// Compute a chromaprint fingerprint for one audio file.
///
/// Decodes via `symphonia` (cross-format), resamples to 11025Hz
/// mono internally, feeds to `rusty-chromaprint`, returns the
/// fingerprint chunk array.
///
/// Errors:
/// * IO failure reading the file.
/// * Symphonia decode failure (unsupported codec, truncated /
///   corrupt file, DRM-locked container).
/// * Chromaprint internal failure (rare).
pub fn hash_file(path: &Path) -> Result<AudioFingerprint, HashError> {
    use rubato::Resampler;
    use std::fs::File;
    use symphonia::core::audio::{AudioBufferRef, SampleBuffer, Signal};
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::errors::Error as SymphoniaError;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let file = File::open(path).map_err(HashError::Io)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(HashError::Decode)?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or(HashError::Decode(SymphoniaError::Unsupported(
            "no default track",
        )))?;
    let track_id = track.id;
    let codec_params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&codec_params, &DecoderOptions::default())
        .map_err(HashError::Decode)?;

    // #97 v2 — Pre-normalize to mono 11025 Hz int16 PCM before the
    // chromaprint feed per AcoustID best-practice. v1 (reverted
    // dc3dfe9 → a480120) had two bugs caught by testrunner's PCM-diff
    // at 2026-05-26T14:50Z + 14:55Z: (a) the `_ => continue` arm
    // silently dropped any sample format other than F32/S16 — 24-bit
    // FLAC decodes to S32, so lossless variants never reached
    // chromaprint, and (b) the symphonia `pcm` codec was missing from
    // Cargo features so WAV files failed `decoder.make()` upstream
    // of any of this code. v2 fixes both: `pcm` is now in features
    // (Cargo.toml); and the match arms below cover F32/S16 zero-copy
    // fast paths plus a SampleBuffer<f32> fallback for every other
    // sample format symphonia can produce.
    const TARGET_RATE: u32 = 11_025;
    const RESAMPLE_CHUNK: usize = 1024;
    let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
    let resample_ratio = TARGET_RATE as f64 / sample_rate as f64;

    let mut resampler = rubato::FastFixedIn::<f32>::new(
        resample_ratio,
        1.0,
        rubato::PolynomialDegree::Septic,
        RESAMPLE_CHUNK,
        1,
    )
    .map_err(|e| HashError::Chromaprint(format!("rubato init: {e}")))?;

    let mut chroma =
        rusty_chromaprint::Fingerprinter::new(&rusty_chromaprint::Configuration::preset_test1());
    chroma
        .start(TARGET_RATE, 1)
        .map_err(|e| HashError::Chromaprint(e.to_string()))?;

    // Rolling mono-f32 buffer; drain RESAMPLE_CHUNK at a time
    // through rubato + push the i16-converted output into chromaprint.
    let mut mono_buf: Vec<f32> = Vec::with_capacity(RESAMPLE_CHUNK * 4);

    // Profiling: see the `profile` module above. One env-var check
    // once per file; zero meaningful cost when off.
    let profile_on = profile::enabled();

    // feat/audio-resample-opt — pre-allocate rubato input + output
    // buffers + i16 PCM staging so the hot loop has zero per-chunk
    // heap allocs. v0.2.12 v2 allocated 2-3 Vecs per RESAMPLE_CHUNK
    // (input drain.collect, rubato process output, pcm i16 collect)
    // — for a 4-minute 44.1kHz file that's 20-30K small allocs per
    // file. AT5 N=500 hits ~10-15M small allocs total, which showed
    // up as the +37% wall-clock regression vs dc3dfe9.
    //
    // Pattern: input_buffer_allocate(true) + output_buffer_allocate(true)
    // give us correctly-sized Vec<Vec<f32>> (1 channel × N frames).
    // process_into_buffer reads from input slice + writes to output
    // slice with no allocation. pcm_buf is grown once to the max
    // output frame count then reused via clear()+extend.
    let mut input_buf: Vec<Vec<f32>> = resampler.input_buffer_allocate(true);
    let mut output_buf: Vec<Vec<f32>> = resampler.output_buffer_allocate(true);
    let mut pcm_buf: Vec<i16> = Vec::with_capacity(resampler.output_frames_max());

    loop {
        let t_decode_start = if profile_on {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(HashError::Decode(e)),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymphoniaError::DecodeError(_)) => continue, // tolerate single-packet corruption
            Err(e) => return Err(HashError::Decode(e)),
        };
        if let Some(t) = t_decode_start {
            profile::add(&profile::T_DECODE, t.elapsed().as_micros() as u64);
        }
        // Mix down to mono f32 per AcoustID recipe: average channels
        // (sum/n_channels, NOT sum — avoids clipping headroom loss).
        let ch_count = decoded.spec().channels.count();
        let frames = decoded.frames();
        let t_mix_start = if profile_on {
            Some(std::time::Instant::now())
        } else {
            None
        };
        match decoded {
            AudioBufferRef::F32(buf) => {
                if ch_count == 1 {
                    mono_buf.extend_from_slice(&buf.chan(0)[..frames]);
                } else {
                    for f in 0..frames {
                        let mut sum = 0.0_f32;
                        for c in 0..ch_count {
                            sum += buf.chan(c)[f];
                        }
                        mono_buf.push(sum / ch_count as f32);
                    }
                }
            }
            AudioBufferRef::S16(buf) => {
                let scale = 1.0_f32 / (i16::MAX as f32);
                if ch_count == 1 {
                    for &s in &buf.chan(0)[..frames] {
                        mono_buf.push(s as f32 * scale);
                    }
                } else {
                    for f in 0..frames {
                        let mut sum = 0.0_f32;
                        for c in 0..ch_count {
                            sum += buf.chan(c)[f] as f32;
                        }
                        mono_buf.push(sum * scale / ch_count as f32);
                    }
                }
            }
            // #97 v2 — SampleBuffer<f32> fallback handles S32 (24-bit
            // FLAC + 24-bit WAV), S24, S8, U8, U16, F64. One extra
            // allocation per packet (~kBs); audio tier hashes
            // thousands of files per scan at most so the overhead is
            // negligible. v1's `_ => continue` silent drop is the
            // bug this arm closes.
            other => {
                let spec = *other.spec();
                let mut sb = SampleBuffer::<f32>::new(frames as u64, spec);
                sb.copy_interleaved_ref(other);
                let samples = sb.samples();
                if ch_count == 1 {
                    mono_buf.extend_from_slice(&samples[..frames]);
                } else {
                    let mut idx = 0;
                    for _ in 0..frames {
                        let mut sum = 0.0_f32;
                        for _ in 0..ch_count {
                            sum += samples[idx];
                            idx += 1;
                        }
                        mono_buf.push(sum / ch_count as f32);
                    }
                }
            }
        }
        if let Some(t) = t_mix_start {
            profile::add(&profile::T_MIXDOWN, t.elapsed().as_micros() as u64);
        }

        // Drain in RESAMPLE_CHUNK-sized blocks through rubato.
        // Zero-alloc per chunk: copy into pre-sized input_buf[0],
        // call process_into_buffer, refill pcm_buf in place.
        while mono_buf.len() >= RESAMPLE_CHUNK {
            // input_buf[0] was sized to input_frames_max() by
            // input_buffer_allocate; we use the first RESAMPLE_CHUNK
            // slots. drain() is O(n) but doesn't allocate.
            input_buf[0].clear();
            input_buf[0].extend(mono_buf.drain(..RESAMPLE_CHUNK));
            let t_resample_start = if profile_on {
                Some(std::time::Instant::now())
            } else {
                None
            };
            let (_in_consumed, out_written) = resampler
                .process_into_buffer(&input_buf, &mut output_buf, None)
                .map_err(|e| HashError::Chromaprint(format!("rubato process: {e}")))?;
            if let Some(t) = t_resample_start {
                profile::add(&profile::T_RESAMPLE, t.elapsed().as_micros() as u64);
            }
            let t_pcm_start = if profile_on {
                Some(std::time::Instant::now())
            } else {
                None
            };
            pcm_buf.clear();
            pcm_buf.extend(
                output_buf[0][..out_written]
                    .iter()
                    .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16),
            );
            if let Some(t) = t_pcm_start {
                profile::add(&profile::T_PCM, t.elapsed().as_micros() as u64);
            }
            let t_chroma_start = if profile_on {
                Some(std::time::Instant::now())
            } else {
                None
            };
            chroma.consume(&pcm_buf);
            if let Some(t) = t_chroma_start {
                profile::add(&profile::T_CHROMA, t.elapsed().as_micros() as u64);
            }
        }
    }

    // Flush any tail under RESAMPLE_CHUNK samples by zero-padding
    // up to the chunk boundary. Drops up to ~93ms of trailing audio
    // detail (1024 samples @ 11025 Hz target), well below the
    // chromaprint per-chunk granularity that matters for matching.
    // Uses process_partial_into_buffer which handles padding
    // internally + reuses output_buf (one small temp alloc for the
    // padded input — one-off per file, not per chunk).
    if !mono_buf.is_empty() {
        let tail: &[f32] = &mono_buf[..];
        let tail_in: [&[f32]; 1] = [tail];
        let (_in_consumed, out_written) = resampler
            .process_partial_into_buffer(Some(&tail_in), &mut output_buf, None)
            .map_err(|e| HashError::Chromaprint(format!("rubato flush: {e}")))?;
        pcm_buf.clear();
        pcm_buf.extend(
            output_buf[0][..out_written]
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16),
        );
        chroma.consume(&pcm_buf);
    }

    chroma.finish();
    Ok(chroma.fingerprint().to_vec())
}

/// Errors from [`hash_file`].
///
/// Kept separate from the crate-wide `Error` enum to keep the
/// always-on engine paths free of `symphonia` / `chromaprint`
/// types. Promote to the crate `Error` when Tier-4 audio
/// integration lands.
#[derive(Debug)]
pub enum HashError {
    Io(std::io::Error),
    Decode(symphonia::core::errors::Error),
    Chromaprint(String),
}

impl std::fmt::Display for HashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Decode(e) => write!(f, "audio decode error: {e}"),
            Self::Chromaprint(e) => write!(f, "chromaprint error: {e}"),
        }
    }
}

impl std::error::Error for HashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Decode(e) => Some(e),
            Self::Chromaprint(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn is_audio_file_recognises_common_extensions() {
        assert!(is_audio_file(&PathBuf::from("foo.mp3")));
        assert!(is_audio_file(&PathBuf::from("FOO.MP3")));
        assert!(is_audio_file(&PathBuf::from("foo.flac")));
        assert!(is_audio_file(&PathBuf::from("foo.wav")));
        assert!(is_audio_file(&PathBuf::from("foo.ogg")));
        assert!(is_audio_file(&PathBuf::from("foo.aac")));
        assert!(is_audio_file(&PathBuf::from("foo.m4a")));
        assert!(!is_audio_file(&PathBuf::from("foo.txt")));
        // OPUS + WMA deferred per spec §3 — symphonia core doesn't
        // decode them without adapter crates. These should NOT
        // surface as audio files today.
        assert!(!is_audio_file(&PathBuf::from("foo.opus")));
        assert!(!is_audio_file(&PathBuf::from("foo.wma")));
        assert!(!is_audio_file(&PathBuf::from("noext")));
    }

    #[test]
    fn hamming_distance_chunk_for_inverted_bits_is_32() {
        assert_eq!(hamming_distance_chunk(0, !0), 32);
        assert_eq!(hamming_distance_chunk(0, 0), 0);
        assert_eq!(hamming_distance_chunk(0b1010, 0b0101), 4);
    }

    #[test]
    fn average_hamming_distance_zero_on_identical() {
        let a = vec![0x1234_5678, 0xdead_beef, 0xcafe_babe];
        assert_eq!(average_hamming_distance(&a, &a), 0.0);
    }

    #[test]
    fn average_hamming_distance_handles_empty() {
        let empty: AudioFingerprint = vec![];
        let nonempty = vec![1u32, 2, 3];
        assert_eq!(average_hamming_distance(&empty, &nonempty), 0.0);
        assert_eq!(average_hamming_distance(&nonempty, &empty), 0.0);
        assert_eq!(average_hamming_distance(&empty, &empty), 0.0);
    }

    #[test]
    fn average_hamming_distance_aligns_on_shorter() {
        // a has 5 chunks; b has 2 chunks. Alignment is over 2 chunks.
        // Each chunk pair flips 32 bits → average = 32.0.
        let a: AudioFingerprint = vec![0; 5];
        let b: AudioFingerprint = vec![!0u32; 2];
        assert_eq!(average_hamming_distance(&a, &b), 32.0);
    }
}
