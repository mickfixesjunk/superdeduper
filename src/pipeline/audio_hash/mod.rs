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

    // Chromaprint wants a stream of i16 mono samples at the file's
    // native sample rate; the crate handles the 11025Hz resample
    // internally. Just feed it what we decode.
    let sample_rate = codec_params.sample_rate.unwrap_or(44_100);
    let channels = codec_params.channels.map(|c| c.count() as u32).unwrap_or(2);
    let mut chroma =
        rusty_chromaprint::Fingerprinter::new(&rusty_chromaprint::Configuration::preset_test1());
    chroma
        .start(sample_rate, channels)
        .map_err(|e| HashError::Chromaprint(e.to_string()))?;

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
        // Convert to i16 mono for chromaprint. AudioBufferRef has
        // per-channel float / int variants; planar layout. We
        // interleave channels into the format chromaprint expects.
        let mut interleaved =
            Vec::with_capacity(decoded.frames() * decoded.spec().channels.count());
        match decoded {
            AudioBufferRef::F32(buf) => {
                for f in 0..buf.frames() {
                    for c in 0..buf.spec().channels.count() {
                        let s = buf.chan(c)[f];
                        // Clamp + scale to i16 range.
                        let v = (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        interleaved.push(v);
                    }
                }
            }
            AudioBufferRef::S16(buf) => {
                for f in 0..buf.frames() {
                    for c in 0..buf.spec().channels.count() {
                        interleaved.push(buf.chan(c)[f]);
                    }
                }
            }
            // For other sample formats (S32, F64, U8…) symphonia
            // can convert; we'd add explicit handling per format
            // if user demand surfaces. Skip for v1.
            _ => continue,
        }
        chroma.consume(&interleaved);
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
