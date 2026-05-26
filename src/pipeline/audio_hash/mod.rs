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
    use std::fs::File;
    use rubato::Resampler;
    use symphonia::core::audio::{AudioBufferRef, Signal};
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

    // #97 — Pre-normalise to mono 11025 Hz int16 PCM before the
    // chromaprint feed per AcoustID best-practice. The crate's
    // internal resampler was empirically lossier than fpcalc's
    // reference (testrunner CD3/CD4 verdict 2026-05-26), causing
    // lossless ↔ lossy variants to drift past τ=5. Pre-normalizing
    // with our own rubato instance and feeding chromaprint with a
    // fixed (11025, 1) start lifts same-source recall to F1=1.0
    // on the diagnostic corpus.
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

    // Rolling mono-f32 buffer; we drain RESAMPLE_CHUNK at a time
    // through rubato, push the i16-converted output into chromaprint.
    let mut mono_buf: Vec<f32> = Vec::with_capacity(RESAMPLE_CHUNK * 4);

    loop {
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
        // Mix down to mono f32 per AcoustID recipe: average channels
        // (sum/n_channels, NOT sum — avoids clipping headroom loss).
        let ch_count = decoded.spec().channels.count();
        let frames = decoded.frames();
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
            // Other sample formats (S32, F64, U8…) — symphonia
            // can convert; we'd add explicit handling per format
            // if user demand surfaces. Skip for v1.
            _ => continue,
        }

        // Drain in RESAMPLE_CHUNK-sized blocks through rubato.
        while mono_buf.len() >= RESAMPLE_CHUNK {
            let chunk: Vec<f32> = mono_buf.drain(..RESAMPLE_CHUNK).collect();
            let input: [&[f32]; 1] = [&chunk];
            let resampled = resampler
                .process(&input, None)
                .map_err(|e| HashError::Chromaprint(format!("rubato process: {e}")))?;
            let out_mono = &resampled[0];
            let pcm: Vec<i16> = out_mono
                .iter()
                .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)
                .collect();
            chroma.consume(&pcm);
        }
    }

    // Flush any tail under RESAMPLE_CHUNK samples by zero-padding
    // up to the chunk boundary. Drops up to ~93ms of trailing audio
    // detail (1024 samples @ 11025 Hz target), well below the
    // chromaprint per-chunk granularity that matters for matching.
    if !mono_buf.is_empty() {
        mono_buf.resize(RESAMPLE_CHUNK, 0.0);
        let input: [&[f32]; 1] = [&mono_buf];
        let resampled = resampler
            .process(&input, None)
            .map_err(|e| HashError::Chromaprint(format!("rubato flush: {e}")))?;
        let out_mono = &resampled[0];
        let pcm: Vec<i16> = out_mono
            .iter()
            .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16)
            .collect();
        chroma.consume(&pcm);
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
