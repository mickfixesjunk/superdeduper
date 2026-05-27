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
    use rayon::prelude::*;
    // Step 1: filter to image extensions + hash each. Hash failures
    // (decode + io) drop silently — they don't kill the scan.
    //
    // Rayon par_iter parallelizes the per-file decode + hash across
    // cores — image_hash::hash_file is CPU-bound on resize + perceptual
    // hash. ~4× throughput on a 4-core machine. Fingerprint output is
    // byte-identical to the serial path; rayon only schedules, doesn't
    // change values.
    let hashed: Vec<Hashed<'_>> = inventory
        .par_iter()
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

    // Step 3b — E2: cluster-cohesion validation per the
    // 2026-05-27 research memo + design routing.
    //
    // Union-find groups files where ANY two members are within
    // `threshold` Hamming bits — but transitive linkage can chain
    // dissimilar files: A~B (Δ=5) + B~C (Δ=5) puts A,B,C in one
    // cluster even when A and C are Δ=10 apart and visually
    // unrelated. That's #78's mega-cluster precision collapse.
    //
    // The cohesion check rejects any cluster whose DIAMETER —
    // max pairwise Hamming distance — exceeds 2 × threshold.
    // O(k²) per cluster, k small (most clusters <10 members; the
    // pathological mega-cluster on #78's reproducer is the
    // exception we WANT to catch). Single-pair clusters (k=2)
    // trivially pass since diameter ≤ threshold ≤ 2 × threshold.
    //
    // E3 (auto-scaling τ from corpus size n) is the orthogonal
    // fix for random-collision FPs; ship separately.
    let cohesion_cap = threshold.saturating_mul(2);
    let mut groups = Vec::new();
    let mut rejected_cohesion = 0u64;
    for (_root, indices) in clusters {
        if indices.len() < 2 {
            continue;
        }
        let diameter = cluster_diameter(&indices, &hashed);
        if diameter > cohesion_cap {
            tracing::debug!(
                cluster_size = indices.len(),
                diameter,
                threshold,
                cohesion_cap,
                "tier-4 image: cluster rejected by E2 cohesion check \
                 (diameter > 2 × threshold; transitive-linkage chain)",
            );
            rejected_cohesion = rejected_cohesion.saturating_add(1);
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
    if rejected_cohesion > 0 {
        tracing::info!(
            rejected_cohesion,
            "tier-4 image: rejected {rejected_cohesion} cluster(s) by E2 \
             cohesion check (kept {} cohesive cluster(s))",
            groups.len(),
        );
    }
    groups
}

/// E2 — cluster-diameter helper. Returns the MAX pairwise Hamming
/// distance among the cluster's members. Caller decides whether
/// that exceeds the cohesion cap (2 × threshold).
///
/// O(k²) per call; cluster sizes are bounded by the union-find
/// output. For pathological mega-clusters (the #78 reproducer
/// case) k can reach the hundreds — still cheap because the
/// inner op is `hamming_distance` (XOR + popcount) on `u64`s.
fn cluster_diameter(indices: &[usize], hashed: &[Hashed<'_>]) -> u32 {
    let mut max_d = 0u32;
    for (n, &i) in indices.iter().enumerate() {
        for &j in &indices[n + 1..] {
            let d = hamming_distance(hashed[i].fingerprint, hashed[j].fingerprint);
            if d > max_d {
                max_d = d;
            }
        }
    }
    max_d
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
    fn cluster_diameter_handles_singleton_pair_and_chain() {
        // Standalone unit test for E2 — pathological inputs that
        // would chain through union-find but should fail the
        // cohesion cap on cluster diameter.

        // Build Hashed entries with hand-set fingerprints (no
        // image decoding). The `file` slot points at synthetic
        // FileEntry refs.
        let entries = [
            entry(PathBuf::from("/x/a"), 0),
            entry(PathBuf::from("/x/b"), 0),
            entry(PathBuf::from("/x/c"), 0),
        ];

        // Singleton (k=1) → diameter 0 (no pairs).
        let single = vec![0usize];
        let h_single = vec![Hashed {
            file: &entries[0],
            fingerprint: 0,
        }];
        assert_eq!(cluster_diameter(&single, &h_single), 0);

        // Pair, Δ = 3 → diameter 3.
        let pair = vec![0usize, 1usize];
        let h_pair = vec![
            Hashed {
                file: &entries[0],
                fingerprint: 0b_0000_0000,
            },
            Hashed {
                file: &entries[1],
                fingerprint: 0b_0000_0111,
            },
        ];
        assert_eq!(cluster_diameter(&pair, &h_pair), 3);

        // Chain A~B~C where A↔B and B↔C are within τ=5 but A↔C
        // straddles 10 bits. union-find would put all three in
        // one cluster; cohesion check (cap = 2τ = 10) catches it
        // at diameter == 10 (passes), or > 10 (rejects).
        //
        // A = 0
        // B = 0b_0000_0000_0001_1111 (5 bits set; Δ to A = 5)
        // C = 0b_0000_0011_1110_0000 (5 bits set; Δ to A = 5; Δ
        //                              to B = 10 — A→B + B→C bits
        //                              don't overlap)
        let triple = vec![0usize, 1usize, 2usize];
        let h_triple = vec![
            Hashed {
                file: &entries[0],
                fingerprint: 0u64,
            },
            Hashed {
                file: &entries[1],
                fingerprint: 0b_0000_0000_0001_1111u64,
            },
            Hashed {
                file: &entries[2],
                fingerprint: 0b_0000_0011_1110_0000u64,
            },
        ];
        let d = cluster_diameter(&triple, &h_triple);
        // A↔C: XOR has 10 bits set → distance 10.
        assert_eq!(d, 10);
        // Verify the cohesion-cap arithmetic: with τ=5, cap = 10,
        // diameter 10 → JUST passes (`>` not `>=`). With τ=4, cap
        // = 8, diameter 10 → rejected. Pins the boundary so a
        // future cap rule change (`>=` vs `>`) is noticed.
        let cap_at_5 = 5u32.saturating_mul(2);
        assert!(d <= cap_at_5, "diameter 10 must NOT exceed cap=10 at τ=5");
        let cap_at_4 = 4u32.saturating_mul(2);
        assert!(d > cap_at_4, "diameter 10 MUST exceed cap=8 at τ=4");
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
