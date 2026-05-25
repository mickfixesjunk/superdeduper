//! Tier-4 perceptual-image similarity grouping (T1.2, #25).
//!
//! Sits AFTER the byte-identical T0–T3 pipeline. Takes the inventory
//! (or whatever's left of it), filters to image extensions, hashes
//! each via [`super::hash_file`], and groups files whose hashes are
//! within `threshold` Hamming distance of one another.
//!
//! V2 scope (this commit):
//!   * Brute-force O(n²) similarity grouping. Fine up to a few
//!     thousand images; beyond that the BK-tree from spec §3.2 is
//!     the right structure (v3 perf follow-up).
//!   * Extension allowlist per spec §3.3 step 1, minus HEIC which
//!     the `image` crate doesn't decode (spec §3.4 defers).
//!   * Reports groups via the existing [`DuplicateGroup`] type with
//!     `similarity_kind = PerceptualImage`.
//!
//! Not yet (deferred per spec):
//!   * BK-tree near-neighbour index (spec §3.2).
//!   * Cache integration — perceptual hashes recomputed every run
//!     (spec §3.6).
//!   * Smart-keep extension for perceptual groups (spec §3.9).
//!   * GUI scan-mode dropdown (spec §3.8).

#![cfg(feature = "similar-images")]

use std::path::Path;

use crate::inventory::FileEntry;
use crate::pipeline::{DuplicateGroup, SimilarityKind};

use super::{hamming_distance, hash_file, Algorithm, ImageFingerprint};

/// Image extensions Tier-4 will hash. Matches spec §3.3 step 1
/// minus HEIC (the `image` crate doesn't decode it; spec §3.4
/// defers HEIC). Compared lowercase so `.JPG` matches.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "tif", "ico",
];

/// Default Hamming-distance threshold per spec §2: "≤5 bits
/// different (~92% bit-similarity) = similar." Configurable via
/// CLI `--image-similarity-threshold N`.
pub const DEFAULT_THRESHOLD: u32 = 5;

/// True if `path` ends with one of the [`IMAGE_EXTENSIONS`].
/// Case-insensitive.
pub fn is_image_file(path: &Path) -> bool {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => {
            let lower = ext.to_ascii_lowercase();
            IMAGE_EXTENSIONS.contains(&lower.as_str())
        }
        None => false,
    }
}

/// One (path, fingerprint) pair after Tier-4 hashing.
#[derive(Debug, Clone)]
struct Hashed<'a> {
    file: &'a FileEntry,
    fingerprint: ImageFingerprint,
}

/// Take an inventory, hash every image file in it, group by
/// Hamming distance ≤ `threshold`. Single-image groups are filtered
/// out (no point reporting "1 image, no similar files"); only
/// groups with ≥2 members come back.
///
/// Hash failures (decode error, IO error) are silently skipped —
/// they don't fail the whole scan. The tradeoff: a corrupt JPEG in
/// the corpus doesn't take down the scan; the user just doesn't see
/// it surfaced as a similarity candidate.
///
/// Returns groups in arbitrary order; sorting is the caller's job.
pub fn find_similar_groups(
    inventory: &[FileEntry],
    algorithm: Algorithm,
    threshold: u32,
) -> Vec<DuplicateGroup> {
    // Step 1: filter to image extensions + hash each. Hash failures
    // (decode + io) drop silently — they don't kill the scan.
    let hashed: Vec<Hashed<'_>> = inventory
        .iter()
        .filter(|f| is_image_file(&f.path))
        .filter_map(|f| match hash_file(&f.path, algorithm) {
            Ok(fp) => Some(Hashed {
                file: f,
                fingerprint: fp,
            }),
            Err(e) => {
                tracing::debug!(
                    path = %f.path.display(),
                    error = %e,
                    "tier-4: skip image (hash failed)",
                );
                None
            }
        })
        .collect();

    if hashed.len() < 2 {
        return Vec::new();
    }

    // Step 2: brute-force union-find on Hamming distance ≤ threshold.
    // O(n²); BK-tree replacement is v3. For n < ~5000 the popcount
    // inner loop is cache-warm enough that the constant factor stays
    // tiny — measured ~1µs per pair on a modern x86, so 1000 images =
    // ~500ms total. Beyond that, BK-tree shrinks the search radius.
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
            if hamming_distance(hashed[i].fingerprint, hashed[j].fingerprint) <= threshold {
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
        // Pick the largest file as the size representative — matches
        // the spec §3.9 smart-keep default ("highest resolution /
        // larger file") + makes the path-aware "reclaim" estimate
        // intuitive (delete N-1 copies of the biggest version).
        let max_size = indices
            .iter()
            .map(|&i| hashed[i].file.size)
            .max()
            .unwrap_or(0);
        // Group identity: synthetic token derived from the first
        // member's fingerprint. Stable across runs for the same
        // cluster IFF the same canonical-first-member sorts first.
        let canonical_idx = *indices
            .iter()
            .min_by_key(|&&i| hashed[i].file.path.as_os_str())
            .unwrap_or(&indices[0]);
        let canonical_fp = hashed[canonical_idx].fingerprint;
        let mut files: Vec<_> = indices
            .iter()
            .map(|&i| hashed[i].file.path.clone())
            .collect();
        files.sort();
        let g = DuplicateGroup {
            size: max_size,
            content_hash: format!("perceptual-{canonical_fp:016x}"),
            files,
            link_equivalent: false,
            unique_inodes: indices.len() as u64,
            similarity_kind: SimilarityKind::PerceptualImage,
        };
        crate::pipeline::assert_unique_paths(&g);
        groups.push(g);
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::FileEntry;
    use image::{DynamicImage, ImageBuffer, Rgb};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn synth_image(rgb: [u8; 3], size: u32) -> DynamicImage {
        let img = ImageBuffer::from_fn(size, size, |_, _| Rgb(rgb));
        DynamicImage::ImageRgb8(img)
    }

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
    fn is_image_file_recognises_common_extensions() {
        assert!(is_image_file(&PathBuf::from("foo.jpg")));
        assert!(is_image_file(&PathBuf::from("FOO.JPG")));
        assert!(is_image_file(&PathBuf::from("foo.png")));
        assert!(is_image_file(&PathBuf::from("foo.tiff")));
        assert!(is_image_file(&PathBuf::from("foo.tif")));
        assert!(!is_image_file(&PathBuf::from("foo.txt")));
        assert!(!is_image_file(&PathBuf::from("foo.heic"))); // not yet
        assert!(!is_image_file(&PathBuf::from("noext")));
    }

    #[test]
    fn empty_inventory_yields_no_groups() {
        let out = find_similar_groups(&[], Algorithm::DifferenceHash, 5);
        assert!(out.is_empty());
    }

    #[test]
    fn single_image_yields_no_group() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("a.png");
        synth_image([10, 20, 30], 32).save(&p).unwrap();
        let inv = vec![entry(p, 0)];
        let out = find_similar_groups(&inv, Algorithm::DifferenceHash, 5);
        assert!(out.is_empty(), "single image must not form a group");
    }

    #[test]
    fn identical_images_form_one_group() {
        let td = TempDir::new().unwrap();
        let img = synth_image([100, 100, 200], 32);
        let a = td.path().join("a.png");
        let b = td.path().join("b.png");
        img.save(&a).unwrap();
        img.save(&b).unwrap();
        let inv = vec![entry(a, 0), entry(b, 0)];
        let out = find_similar_groups(&inv, Algorithm::DifferenceHash, 5);
        assert_eq!(out.len(), 1, "two identical images must form one group");
        assert_eq!(out[0].files.len(), 2);
        assert_eq!(out[0].similarity_kind, SimilarityKind::PerceptualImage);
        assert!(out[0].content_hash.starts_with("perceptual-"));
    }

    #[test]
    fn very_different_images_do_not_group_at_strict_threshold() {
        // Two visually-distinct constant-color images.
        // At threshold=1 (very strict), dHash on uniform colors
        // tends to collapse to near-zero, so all-uniform images
        // can score very close even with different colors.
        // Use a uniform color + a checker-pattern image to get
        // meaningfully different dHashes.
        let td = TempDir::new().unwrap();
        let uniform = synth_image([200, 200, 200], 64);
        let checker = {
            let buf = ImageBuffer::from_fn(64, 64, |x, y| {
                if (x / 8 + y / 8) % 2 == 0 {
                    Rgb([0, 0, 0])
                } else {
                    Rgb([255, 255, 255])
                }
            });
            DynamicImage::ImageRgb8(buf)
        };
        let a = td.path().join("uniform.png");
        let b = td.path().join("checker.png");
        uniform.save(&a).unwrap();
        checker.save(&b).unwrap();
        let inv = vec![entry(a, 0), entry(b, 0)];
        // Threshold 0 — must be EXACTLY equal. uniform vs checker
        // won't be exactly equal.
        let out = find_similar_groups(&inv, Algorithm::DifferenceHash, 0);
        assert!(
            out.is_empty(),
            "distinct images must not group at threshold=0; got {} group(s)",
            out.len()
        );
    }

    #[test]
    fn non_image_files_ignored() {
        let td = TempDir::new().unwrap();
        let p = td.path().join("doc.txt");
        std::fs::write(&p, b"not an image").unwrap();
        let inv = vec![entry(p, 12)];
        let out = find_similar_groups(&inv, Algorithm::DifferenceHash, 5);
        assert!(
            out.is_empty(),
            "non-image files must not be hashed or grouped"
        );
    }

    #[test]
    fn three_clones_form_one_group_of_three() {
        let td = TempDir::new().unwrap();
        let img = synth_image([50, 100, 150], 32);
        let mut inv = Vec::new();
        for i in 0..3 {
            let p = td.path().join(format!("img{i}.png"));
            img.save(&p).unwrap();
            inv.push(entry(p, 0));
        }
        let out = find_similar_groups(&inv, Algorithm::DifferenceHash, 5);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].files.len(), 3);
    }
}
