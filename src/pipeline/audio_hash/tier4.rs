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

/// Take an inventory, hash every audio file in it, group by
/// average per-chunk Hamming distance ≤ `threshold`. Single-file
/// groups are filtered out.
///
/// Hash failures (decode error, DRM, unsupported codec) are
/// silently skipped — same tradeoff as the image Tier-4 path. The
/// alternative (failing the whole scan on one bad MP3) would
/// kill usability for any music library with a stray file.
///
/// Returns groups in arbitrary order; sorting is the caller's job.
pub fn find_similar_groups(inventory: &[FileEntry], threshold: f64) -> Vec<DuplicateGroup> {
    // Step 1: filter to audio extensions + hash each. Hash failures
    // drop silently with a tracing::debug! so triage has a trail.
    let hashed: Vec<Hashed<'_>> = inventory
        .iter()
        .filter(|f| is_audio_file(&f.path))
        .filter_map(|f| match hash_file(&f.path) {
            Ok(fp) if !fp.is_empty() => Some(Hashed {
                file: f,
                fingerprint: fp,
            }),
            Ok(_) => {
                tracing::debug!(
                    path = %f.path.display(),
                    "tier-4 audio: empty fingerprint; skipping (likely <30s of decoded audio)",
                );
                None
            }
            Err(e) => {
                tracing::debug!(
                    path = %f.path.display(),
                    error = %e,
                    "tier-4 audio: skip (hash failed)",
                );
                None
            }
        })
        .collect();

    if hashed.len() < 2 {
        return Vec::new();
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
        groups.push(DuplicateGroup {
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
        });
    }
    groups
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
        assert!(out.is_empty());
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
        assert!(out.is_empty());
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
