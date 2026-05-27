//! Tier-4 acoustic-audio similarity grouping (T1.3, #26).
//!
//! Sits AFTER the byte-identical T0–T3 pipeline. Takes the inventory,
//! filters to audio extensions, fingerprints each via
//! [`super::hash_file`] (symphonia decode → chromaprint), and groups
//! files whose chunk-sequences are within `threshold` average
//! Hamming distance per chunk.
//!
//! V1 scope (this commit):
//!   * Brute-force O(n²) similarity grouping over Vec<u32>
//!     fingerprint sequences. Fine up to a few hundred audio files;
//!     beyond that the BK-tree from spec §4.4 is the right structure
//!     (perf follow-up).
//!   * Extension allowlist per spec §3 (MP3, M4A/AAC, FLAC, WAV,
//!     OGG-Vorbis — OPUS + WMA deferred; symphonia core doesn't
//!     decode them yet).
//!   * Reports groups via the existing [`DuplicateGroup`] type with
//!     `similarity_kind = PerceptualImage` (TODO: add
//!     `SimilarityKind::PerceptualAudio` variant once design signs
//!     off on the wire shape — for v1 we reuse the image variant
//!     so the GUI groups table still renders the perceptual marker;
//!     the audio-specific variant lands alongside web's leaderboard
//!     schema bump).
//!
//! Not yet (deferred per spec):
//!   * BK-tree near-neighbour index (spec §4.4).
//!   * Cache integration — chromaprint fingerprints recomputed
//!     every run (spec §4.5).
//!   * DRM detection + `AudioFileEncrypted` event.

#![cfg(feature = "similar-audio")]

use crate::inventory::FileEntry;
use crate::pipeline::{DuplicateGroup, SimilarityKind};

use super::{average_hamming_distance, hash_file, is_audio_file, AudioFingerprint};

/// Default per-chunk average Hamming-distance threshold per spec
/// §3: "5 out of 32 bits per chunk — czkawka's calibrated default."
pub const DEFAULT_THRESHOLD: f64 = 5.0;

/// One (file, fingerprint) pair after Tier-4 hashing.
#[derive(Debug, Clone)]
struct Hashed<'a> {
    file: &'a FileEntry,
    fingerprint: AudioFingerprint,
}

/// #102 — Result of Tier-4 audio grouping. Carries the count of
/// files filtered out because their decoded duration was below
/// chromaprint's ~30s minimum so the scan-finish summary can
/// explain why short voice memos / sound effects didn't cluster.
/// These files are NOT lost from the dedup — Tier 0-3 byte-
/// identical matching ran independently first; this counter just
/// surfaces the perceptual-tier opt-out so the user understands
/// the behavior.
///
/// #119 — `decode_warnings` carries one entry per audio file
/// that hit a real decode error (corrupt header / mid-stream
/// flip / panic) — distinct from the short-skip counter which is
/// "valid file, just too short for chromaprint." The user sees
/// these as warning lines in CLI output and (future) badge
/// overlays in GUI dupe rows.
#[derive(Debug, Default)]
pub struct AudioTier4Result {
    pub groups: Vec<DuplicateGroup>,
    pub short_skipped_count: u64,
    pub decode_warnings: Vec<AudioDecodeWarning>,
}

/// #119 — one record per audio file whose decode failed or
/// panicked. Surfaced via the scan output JSON / text + (future)
/// the GUI badge widget. Captures enough context for the user to
/// understand WHY a file didn't fingerprint without being a hard
/// failure — silent skip is the anti-pattern this fixes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioDecodeWarning {
    pub path: std::path::PathBuf,
    /// One of: `"corrupt_header"`, `"mid_stream_corrupt"`,
    /// `"truncated"`, `"empty_file"`, `"decoder_panic"`,
    /// `"unknown"`. Stable wire shape — additions to the set
    /// require a #serde(default)-compatible bump and downstream
    /// consumers gracefully fall back to `"unknown"` for kinds
    /// they don't recognise.
    pub kind: String,
    /// Human-readable detail — the underlying decoder's error
    /// message, the panic payload, or a synthesised "X is 0 bytes"
    /// line for empty files. The CLI surface renders this verbatim;
    /// no consumer pattern-matches on its content.
    pub detail: String,
}

/// Take an inventory, hash every audio file in it, group by
/// average per-chunk Hamming distance ≤ `threshold`. Single-file
/// groups are filtered out.
///
/// Hash failures (decode error, DRM, unsupported codec) are
/// silently skipped — same tradeoff as the image Tier-4 path. The
/// alternative (failing the whole scan on one bad MP3) would
/// kill usability for any music library with a stray file.
///
/// Returns groups (in arbitrary order; sorting is the caller's
/// job) plus the count of files filtered for being too short to
/// fingerprint per #102.
pub fn find_similar_groups(inventory: &[FileEntry], threshold: f64) -> AudioTier4Result {
    // Step 1: filter to audio extensions + hash each. Decode
    // failures + panics get recorded as AudioDecodeWarning (#119)
    // instead of silent-dropping; empty fingerprints (file shorter
    // than chromaprint's ~30s window) stay on the short_skipped
    // counter — that's a valid file, not a decode failure.
    let mut short_skipped_count: u64 = 0;
    let mut decode_warnings: Vec<AudioDecodeWarning> = Vec::new();
    let hashed: Vec<Hashed<'_>> = inventory
        .iter()
        .filter(|f| is_audio_file(&f.path))
        .filter_map(|f| {
            // Pre-decode edge case: zero-byte files (extension-
            // valid but no bytes). Surface as a warning rather
            // than hand it to symphonia, which would error
            // mid-probe with an unhelpful message.
            if let Ok(meta) = std::fs::metadata(&f.path) {
                if meta.len() == 0 {
                    decode_warnings.push(AudioDecodeWarning {
                        path: f.path.clone(),
                        kind: "empty_file".to_string(),
                        detail: "file is 0 bytes".to_string(),
                    });
                    return None;
                }
            }
            // `catch_unwind` shields the scan from upstream decoder
            // panics (notably the symphonia-codec-aac 0.5.5 panic
            // at `aac/ics/mod.rs:242:17`).
            let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hash_file(&f.path)));
            match res {
                Ok(Ok(fp)) if !fp.is_empty() => Some(Hashed {
                    file: f,
                    fingerprint: fp,
                }),
                Ok(Ok(_)) => {
                    tracing::debug!(
                        path = %f.path.display(),
                        "tier-4 audio: empty fingerprint; skipping (likely <30s of decoded audio)",
                    );
                    short_skipped_count = short_skipped_count.saturating_add(1);
                    None
                }
                Ok(Err(e)) => {
                    let detail = e.to_string();
                    // Heuristic kind classification from the
                    // decoder's error message. The substring
                    // matches are case-insensitive against
                    // common symphonia error phrasings; the
                    // `unknown` fallback prevents silent
                    // miscategorisation when symphonia adds a
                    // new error class we haven't seen.
                    let lower = detail.to_lowercase();
                    let kind = if lower.contains("eof") || lower.contains("truncat") {
                        "truncated"
                    } else if lower.contains("header") || lower.contains("streaminfo") {
                        "corrupt_header"
                    } else if lower.contains("frame") || lower.contains("crc") {
                        "mid_stream_corrupt"
                    } else {
                        "unknown"
                    };
                    tracing::warn!(
                        path = %f.path.display(),
                        kind,
                        error = %detail,
                        "tier-4 audio: decode failed",
                    );
                    decode_warnings.push(AudioDecodeWarning {
                        path: f.path.clone(),
                        kind: kind.to_string(),
                        detail,
                    });
                    None
                }
                Err(payload) => {
                    let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                        (*s).to_string()
                    } else if let Some(s) = payload.downcast_ref::<String>() {
                        s.clone()
                    } else {
                        "unknown panic payload".to_string()
                    };
                    tracing::warn!(
                        path = %f.path.display(),
                        panic = %msg,
                        "tier-4 audio: decoder panic — likely symphonia upstream bug",
                    );
                    decode_warnings.push(AudioDecodeWarning {
                        path: f.path.clone(),
                        kind: "decoder_panic".to_string(),
                        detail: msg,
                    });
                    None
                }
            }
        })
        .collect();

    if hashed.len() < 2 {
        return AudioTier4Result {
            groups: Vec::new(),
            short_skipped_count,
            decode_warnings,
        };
    }

    // Step 2: brute-force union-find on average chunk-Hamming
    // distance ≤ threshold. O(n²) over fingerprint sequences;
    // per-pair cost is O(min(len_a, len_b)) chunks × popcount,
    // so a 5-minute song with ~2400 chunks costs ~2400 popcounts
    // per comparison. For a library of <500 audio files, the
    // brute-force completes in seconds; bigger libraries want
    // the BK-tree (v3 perf follow-up).
    let n = hashed.len();
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] == i {
            return i;
        }
        let root = find(parent, parent[i]);
        parent[i] = root;
        root
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if average_hamming_distance(&hashed[i].fingerprint, &hashed[j].fingerprint) <= threshold
            {
                union(&mut parent, i, j);
            }
        }
    }

    // Step 3: collect into clusters, drop singletons, build groups.
    let mut clusters: std::collections::HashMap<usize, Vec<usize>> =
        std::collections::HashMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        clusters.entry(root).or_default().push(i);
    }

    let mut groups = Vec::new();
    for (_root, indices) in clusters {
        if indices.len() < 2 {
            continue;
        }
        // Pick the largest file as the size representative —
        // typically the highest-bitrate / least-compressed version.
        let max_size = indices
            .iter()
            .map(|&i| hashed[i].file.size)
            .max()
            .unwrap_or(0);
        // Group identity: synthetic token derived from the
        // lex-min member's fingerprint. Stable across runs IFF
        // the canonical-first-member sorts first.
        let canonical_idx = *indices
            .iter()
            .min_by_key(|&&i| hashed[i].file.path.as_os_str())
            .unwrap_or(&indices[0]);
        // First chunk as a token (chromaprint fingerprints can be
        // hundreds of u32s; use the first 8 bytes as identity).
        let canonical_fp_token: u64 = hashed[canonical_idx]
            .fingerprint
            .first()
            .copied()
            .unwrap_or(0) as u64;
        let mut files: Vec<_> = indices
            .iter()
            .map(|&i| hashed[i].file.path.clone())
            .collect();
        files.sort();
        let g = DuplicateGroup {
            size: max_size,
            content_hash: format!("perceptual-audio-{canonical_fp_token:016x}"),
            files,
            link_equivalent: false,
            unique_inodes: indices.len() as u64,
            // GH #54 — was `PerceptualImage` in the v1 placeholder
            // (so the GUI marker still surfaced "review carefully");
            // testrunner's AT6 caught the inconsistency between
            // content_hash's `perceptual-audio-` prefix + the kind
            // field. Audio groups now correctly emit
            // PerceptualAudio.
            similarity_kind: SimilarityKind::PerceptualAudio,
        };
        crate::pipeline::assert_unique_paths(&g);
        groups.push(g);
    }
    AudioTier4Result {
        groups,
        short_skipped_count,
        decode_warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn entry(path: PathBuf, size: u64) -> FileEntry {
        FileEntry {
            path,
            size,
            mtime: 0,
            file_ref: 0,
            parent_ref: 0,
            usn: 0,
            attributes: 0,
            volume_guid: None,
            placeholder: crate::inventory::placeholder::PlaceholderState::default(),
        }
    }

    #[test]
    fn empty_inventory_yields_no_groups() {
        let out = find_similar_groups(&[], 5.0);
        assert!(out.groups.is_empty());
        assert_eq!(out.short_skipped_count, 0);
    }

    #[test]
    fn non_audio_files_ignored() {
        // Touch a non-audio file and pass it through; with no
        // audio in the inventory the result is empty (nothing to
        // hash) — proves the extension filter does its job.
        let td = TempDir::new().unwrap();
        let p = td.path().join("doc.txt");
        std::fs::write(&p, b"not audio").unwrap();
        let inv = vec![entry(p, 12)];
        let out = find_similar_groups(&inv, 5.0);
        assert!(out.groups.is_empty());
        assert_eq!(out.short_skipped_count, 0);
        assert!(out.decode_warnings.is_empty());
    }

    /// #119 W6 — zero-byte audio file produces an
    /// `empty_file` decode warning (not a silent skip, not a
    /// short_skipped count).
    #[test]
    fn zero_byte_audio_file_yields_empty_file_warning() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("zero.flac");
        std::fs::write(&p, b"").unwrap();
        let inv = vec![entry(p.clone(), 0)];
        let out = find_similar_groups(&inv, 5.0);
        assert!(out.groups.is_empty());
        assert_eq!(out.short_skipped_count, 0);
        assert_eq!(out.decode_warnings.len(), 1);
        let w = &out.decode_warnings[0];
        assert_eq!(w.path, p);
        assert_eq!(w.kind, "empty_file");
        assert!(
            w.detail.contains("0 bytes"),
            "detail should mention 0 bytes, got: {}",
            w.detail
        );
    }

    /// #119 W3 — garbage-bytes file with .flac extension hits
    /// symphonia's decode path + produces a decode_warning. The
    /// kind heuristic maps the symphonia error message to one of
    /// the stable wire strings.
    #[test]
    fn corrupt_audio_file_yields_decode_warning() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("garbage.flac");
        // Some non-empty garbage bytes — symphonia will reject
        // these at probe time. The exact error message varies by
        // symphonia version; we don't pin a specific kind, just
        // that ONE of the AudioDecodeWarning slots fires.
        std::fs::write(&p, vec![0xAB; 256]).unwrap();
        let inv = vec![entry(p.clone(), 256)];
        let out = find_similar_groups(&inv, 5.0);
        assert!(out.groups.is_empty());
        assert_eq!(out.short_skipped_count, 0);
        assert_eq!(out.decode_warnings.len(), 1);
        let w = &out.decode_warnings[0];
        assert_eq!(w.path, p);
        // Kind must be one of the stable wire strings — see the
        // AudioDecodeWarning docstring.
        assert!(
            matches!(
                w.kind.as_str(),
                "corrupt_header" | "truncated" | "mid_stream_corrupt" | "unknown"
            ),
            "unexpected kind: {}",
            w.kind,
        );
        assert!(!w.detail.is_empty(), "detail should not be empty");
    }

    // Synthesising real audio files in unit tests is heavy + slow
    // (need to write a valid WAV header + decoded PCM that
    // chromaprint can process). Coverage of the actual fingerprint
    // matching belongs in an integration test with a small audio
    // fixture corpus; testdesign owns that surface per design's
    // 08:46Z routing ("testrunner has audio corpus generator pre-
    // built"). This file's unit tests stay focused on the
    // grouping shape that doesn't need real audio.
}
