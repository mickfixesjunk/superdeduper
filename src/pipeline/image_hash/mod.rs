//! Perceptual image hashing — first slice of GH #25 (T1.2).
//!
//! V1 scope (this commit):
//!   * `Algorithm` enum + thin wrapper around `image_hasher`'s
//!     dHash / aHash / pHash implementations.
//!   * `hash_file(path, algorithm)` entry point — decode + hash one
//!     image, return a 64-bit fingerprint.
//!   * `hamming_distance(a, b)` for similarity scoring.
//!   * Unit tests against synthetic pixel buffers.
//!
//! V1 explicitly does NOT include:
//!   * BK-tree near-neighbour index (spec §3.2)
//!   * Pipeline integration (Tier 4 stage per spec §3.3)
//!   * Cache integration (spec §3.6)
//!   * Smart-keep extension (spec §3.9)
//!   * GUI mode dropdown (spec §3.8)
//!   * `--mode image` actually doing anything beyond CLI parse
//!
//! Behind the `similar-images` cargo feature so the always-on
//! dedup pipeline doesn't pull image-codec transitive deps it
//! doesn't need. Release matrix flips it on alongside Tier-4
//! integration in a later sub-deliverable.

#![cfg(feature = "similar-images")]

pub mod tier4;

use std::path::Path;

use image_hasher::HashAlg;

/// Perceptual hash family per spec §2.
///
/// Defaults to [`DifferenceHash`](Self::DifferenceHash) for parity
/// with czkawka's UX (good accuracy + ~10× faster than DCT-based
/// pHash for typical phone-photo sizes).
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum Algorithm {
    /// Average hash. Fastest, lowest robustness — primarily useful
    /// as a prefilter before a heavier algorithm. Spec table §2.
    AverageHash,
    /// Difference hash. Default per spec §2 "Recommendation:
    /// ship dHash as default."
    #[default]
    DifferenceHash,
    /// Double-gradient hash from `image_hasher::HashAlg::DoubleGradient`.
    /// Despite the historical slug `"phash"`, this is NOT a
    /// Discrete-Cosine-Transform pHash — it's the double-gradient
    /// extension of dHash that captures gradient transitions in both
    /// horizontal AND vertical directions. The slug stays "phash"
    /// for back-compat with persisted scan JSON + cache rows; the
    /// docstring + variant name now match what the code actually
    /// does. If true DCT-based pHash ever lands, it gets a new
    /// variant + slug; the existing "phash" wire encoding belongs
    /// to DoubleGradient permanently.
    DoubleGradient,
}

impl Algorithm {
    /// Stable lowercase slug for CLI + JSON serialization. Matches
    /// the values the `--mode` enum + the future Tier-4 schema
    /// will surface. `"phash"` is back-compat shorthand for
    /// [`Algorithm::DoubleGradient`]; see that variant's docstring.
    pub fn as_slug(self) -> &'static str {
        match self {
            Self::AverageHash => "ahash",
            Self::DifferenceHash => "dhash",
            Self::DoubleGradient => "phash",
        }
    }

    fn to_image_hasher_alg(self) -> HashAlg {
        match self {
            Self::AverageHash => HashAlg::Mean,
            Self::DifferenceHash => HashAlg::Gradient,
            Self::DoubleGradient => HashAlg::DoubleGradient,
        }
    }
}

/// A 64-bit perceptual fingerprint. Stored as raw `u64` so the
/// downstream BK-tree can XOR + popcount efficiently (Hamming
/// distance = `(a ^ b).count_ones()`).
///
/// `image_hasher` returns an opaque `ImageHash` value of variable
/// length depending on the algorithm. We normalise to a 64-bit
/// fingerprint by reading the first 8 bytes of its byte buffer —
/// every algorithm in our enum produces at least 64 bits (dHash =
/// 64, aHash = 64, pHash double-gradient = 128 → use the upper
/// 8). This keeps the BK-tree single-typed; if v2 wants
/// wider hashes we bump the schema.
pub type ImageFingerprint = u64;

/// #78 / E3 — Resolve the Hamming threshold from a default + the
/// corpus size `n` (count of decoded image fingerprints).
///
/// Formula: `τ_eff = max(3, default_tau - floor(log10(n / 100)))`.
/// Intuition: the expected random-collision FP rate scales with
/// `n²`; dropping `τ` by 1 bit each order of magnitude past 100
/// keeps the expected-FP-rate bounded as the corpus grows. The
/// floor at 3 prevents τ from collapsing to zero on huge corpora
/// where dHash's bit-precision tops out (5 bits per chunk per
/// spec §2 calibration).
///
/// Anchor points (with default_tau = 5):
/// - n ≤ 99   → τ_eff = 5 (no scaling)
/// - n = 100  → τ_eff = 5 (log10(1.0) = 0)
/// - n = 1000 → τ_eff = 4 (log10(10) = 1)
/// - n = 10k  → τ_eff = 3 (log10(100) = 2; floored)
/// - n = 100k → τ_eff = 3 (floored)
///
/// Behind `--image-similarity-threshold auto` per the spec. Ship-
/// as-opt-in first; promotion to default gated on field results.
pub fn tau_for_n(default_tau: u32, n: u64) -> u32 {
    if n < 100 {
        return default_tau.max(3);
    }
    // floor(log10(n / 100)) = number of orders of magnitude past
    // 100. integer-arithmetic version avoids floats + matches
    // the exact-integer boundary the docstring claims.
    let scale = ((n / 100) as f64).log10().floor() as u32;
    default_tau.saturating_sub(scale).max(3)
}

/// Compute a perceptual fingerprint for one image file.
///
/// Decodes via the `image` crate (supports JPEG / PNG / WebP / GIF
/// / BMP / TIFF / ICO out of the box; HEIC is deferred per spec
/// §3.4). The decoded buffer is downsampled to 32×32 grayscale
/// internally by `image_hasher` before the algorithm runs — no
/// caller knobs needed for that today, since the spec locks the
/// downsample dimensions at the default 8×8 (image_hasher's
/// `hash_size`).
///
/// Errors:
/// * IO failures from reading the file.
/// * Decode failures from `image::ImageReader` (unsupported format,
///   truncated file, corrupt header).
pub fn hash_file(path: &Path, algorithm: Algorithm) -> Result<ImageFingerprint, HashError> {
    let img = image::ImageReader::open(path)
        .map_err(HashError::Io)?
        .with_guessed_format()
        .map_err(HashError::Io)?
        .decode()
        .map_err(HashError::Decode)?;
    Ok(hash_image(&img, algorithm))
}

/// Compute a perceptual fingerprint from an already-decoded image.
/// Public-ish (pub-in-crate) so future Tier-4 pipeline integration
/// can pass the decoded buffer directly without round-tripping
/// through disk.
pub fn hash_image(img: &image::DynamicImage, algorithm: Algorithm) -> ImageFingerprint {
    let hasher = image_hasher::HasherConfig::new()
        .hash_alg(algorithm.to_image_hasher_alg())
        .hash_size(8, 8)
        .to_hasher();
    let hash = hasher.hash_image(img);
    bytes_to_u64(hash.as_bytes())
}

/// Hamming distance between two fingerprints — count of differing
/// bits. Lower = more perceptually similar. Spec §2 default
/// threshold: ≤5 bits → "similar."
pub fn hamming_distance(a: ImageFingerprint, b: ImageFingerprint) -> u32 {
    (a ^ b).count_ones()
}

/// Errors from [`hash_file`].
///
/// Kept separate from the crate-wide `Error` enum to keep the
/// always-on engine paths free of `image` / `image_hasher` types.
/// Promote to the crate `Error` when Tier-4 integration lands.
#[derive(Debug)]
pub enum HashError {
    Io(std::io::Error),
    Decode(image::ImageError),
}

impl std::fmt::Display for HashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::Decode(e) => write!(f, "image decode error: {e}"),
        }
    }
}

impl std::error::Error for HashError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            Self::Decode(e) => Some(e),
        }
    }
}

/// Read the first 8 bytes of a hash buffer into a big-endian u64.
/// Big-endian so the bit-ordering matches popcount semantics on
/// `u64::count_ones` (architecture-independent).
fn bytes_to_u64(b: &[u8]) -> u64 {
    let mut out = [0u8; 8];
    let n = b.len().min(8);
    out[..n].copy_from_slice(&b[..n]);
    u64::from_be_bytes(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageBuffer, Rgb};

    /// Build a synthetic test image filled with a uniform color.
    /// `image_hasher` reduces this to a constant fingerprint, which
    /// is enough to exercise the wrapper.
    fn solid(rgb: [u8; 3]) -> DynamicImage {
        let img = ImageBuffer::from_fn(64, 64, |_, _| Rgb(rgb));
        DynamicImage::ImageRgb8(img)
    }

    /// Diagonal split: top-left half one color, bottom-right half
    /// another. Produces a non-trivial fingerprint that's stable
    /// for that fixed pattern.
    fn diagonal() -> DynamicImage {
        let img = ImageBuffer::from_fn(64, 64, |x, y| {
            if x + y < 64 {
                Rgb([200, 50, 50])
            } else {
                Rgb([50, 50, 200])
            }
        });
        DynamicImage::ImageRgb8(img)
    }

    #[test]
    fn algorithm_default_is_dhash() {
        assert_eq!(Algorithm::default(), Algorithm::DifferenceHash);
    }

    #[test]
    fn tau_for_n_no_scaling_under_100() {
        // n < 100 → no scaling; default returned (or floor at 3).
        assert_eq!(tau_for_n(5, 0), 5);
        assert_eq!(tau_for_n(5, 1), 5);
        assert_eq!(tau_for_n(5, 50), 5);
        assert_eq!(tau_for_n(5, 99), 5);
    }

    #[test]
    fn tau_for_n_anchor_points_match_docstring() {
        // n=100 → floor(log10(1.0)) = 0 → no scaling.
        assert_eq!(tau_for_n(5, 100), 5);
        // n=999 → floor(log10(9.99)) = 0 → still no scaling.
        assert_eq!(tau_for_n(5, 999), 5);
        // n=1000 → floor(log10(10.0)) = 1 → -1 from default.
        assert_eq!(tau_for_n(5, 1000), 4);
        // n=10000 → floor(log10(100.0)) = 2 → -2 from default = 3.
        assert_eq!(tau_for_n(5, 10_000), 3);
        // n=100k → floor(log10(1000.0)) = 3; floored at 3.
        assert_eq!(tau_for_n(5, 100_000), 3);
        // n=10M → floored at 3 regardless.
        assert_eq!(tau_for_n(5, 10_000_000), 3);
    }

    #[test]
    fn tau_for_n_respects_default_when_default_below_3() {
        // floor-at-3 logic: a default below 3 (pathological) still
        // produces 3. Defends against future API misuse.
        assert_eq!(tau_for_n(0, 0), 3);
        assert_eq!(tau_for_n(2, 99), 3);
        assert_eq!(tau_for_n(2, 100_000), 3);
    }

    #[test]
    fn tau_for_n_with_larger_default() {
        // A user-supplied higher default (say 10 for legacy phash
        // recall) scales down by the same exponent.
        assert_eq!(tau_for_n(10, 50), 10);
        assert_eq!(tau_for_n(10, 100), 10);
        assert_eq!(tau_for_n(10, 1_000), 9);
        assert_eq!(tau_for_n(10, 10_000), 8);
        assert_eq!(tau_for_n(10, 100_000), 7);
        assert_eq!(tau_for_n(10, 1_000_000), 6);
        assert_eq!(tau_for_n(10, 10_000_000), 5);
        assert_eq!(tau_for_n(10, 100_000_000), 4);
        // Eventually floors at 3 when the subtraction would push
        // below 3 (scale=7 → 10-7=3; scale=8+ → still 3).
        assert_eq!(tau_for_n(10, 1_000_000_000), 3);
        assert_eq!(tau_for_n(10, 10_000_000_000), 3);
    }

    #[test]
    fn algorithm_slugs_are_stable() {
        // These slugs land in the future Tier-4 JSON schema +
        // --image-similarity-algorithm CLI flag. Don't churn them.
        assert_eq!(Algorithm::AverageHash.as_slug(), "ahash");
        assert_eq!(Algorithm::DifferenceHash.as_slug(), "dhash");
        assert_eq!(Algorithm::DoubleGradient.as_slug(), "phash");
    }

    #[test]
    fn solid_image_hashes_consistently_across_calls() {
        let img = solid([100, 150, 200]);
        let h1 = hash_image(&img, Algorithm::DifferenceHash);
        let h2 = hash_image(&img, Algorithm::DifferenceHash);
        assert_eq!(h1, h2, "dHash must be deterministic for same input");
    }

    #[test]
    fn different_images_yield_different_hashes_with_dhash() {
        // Distinct visual content → different dHash. A constant
        // and a diagonal split shouldn't collide.
        let solid_h = hash_image(&solid([100, 100, 100]), Algorithm::DifferenceHash);
        let diag_h = hash_image(&diagonal(), Algorithm::DifferenceHash);
        assert_ne!(solid_h, diag_h);
    }

    #[test]
    fn identical_images_have_zero_hamming_distance() {
        let img = diagonal();
        let h = hash_image(&img, Algorithm::DifferenceHash);
        assert_eq!(hamming_distance(h, h), 0);
    }

    #[test]
    fn hamming_distance_for_inverted_bits_is_64() {
        let a: u64 = 0;
        let b: u64 = !0;
        assert_eq!(hamming_distance(a, b), 64);
    }

    #[test]
    fn hamming_distance_is_symmetric() {
        let img1 = solid([100, 100, 100]);
        let img2 = diagonal();
        let h1 = hash_image(&img1, Algorithm::DifferenceHash);
        let h2 = hash_image(&img2, Algorithm::DifferenceHash);
        assert_eq!(hamming_distance(h1, h2), hamming_distance(h2, h1));
    }

    /// Round-trip via PNG on disk to exercise the file-decode path.
    /// Same image → same fingerprint regardless of encode/decode
    /// cycle, which is the basic property the algorithm must hold.
    #[test]
    fn hash_file_round_trip_via_png() {
        let img = diagonal();
        let tmp = tempfile::Builder::new()
            .prefix("sd-image-hash-test-")
            .suffix(".png")
            .tempfile()
            .unwrap();
        img.save(tmp.path()).unwrap();
        let h_disk = hash_file(tmp.path(), Algorithm::DifferenceHash).unwrap();
        let h_mem = hash_image(&img, Algorithm::DifferenceHash);
        assert_eq!(h_disk, h_mem);
    }

    /// All three algorithms produce a fingerprint without panicking
    /// on the synthetic inputs. This is a smoke check, not a
    /// quality check; the real benchmark lives in spec §6.
    #[test]
    fn all_algorithms_produce_a_fingerprint() {
        let img = diagonal();
        for algo in [
            Algorithm::AverageHash,
            Algorithm::DifferenceHash,
            Algorithm::DoubleGradient,
        ] {
            let h = hash_image(&img, algo);
            // Not asserting specific value — just that it's
            // non-zero on a non-trivial input.
            assert_ne!(
                h, 0,
                "{algo:?} should not produce all-zero hash for diagonal"
            );
        }
    }
}
