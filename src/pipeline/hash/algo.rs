//! Pluggable content-hash algorithm.
//!
//! Two backends today: cryptographic-grade [`HashAlgo::Blake3`] (32-byte
//! output, SIMD + parallel chunk-tree internals) and
//! [`HashAlgo::River5`] (formerly DDH-128) — a 16-byte hash with an
//! AES-NI core, maintained out of `../ddh-128` (the crate has been
//! renamed to `river128` but the directory still carries the old
//! name).
//!
//! Engine code never touches a `blake3::Hasher` or `river5::Hasher`
//! directly; instead it constructs a [`ContentHasher`] from the
//! algo chosen in [`ScanSettings::hash_algo`] and uses the same
//! `new` / `update` / `finalize` shape regardless of which backend
//! is selected. Outputs are returned as `Vec<u8>` because the two
//! algorithms have different sizes (32 vs 16 bytes).

use serde::{Deserialize, Serialize};

/// Which content-hash algorithm a scan should use. Persisted via
/// `ScanSettings` and recorded in the cache row so warm rescans
/// don't mix Blake3 and River5 outputs by accident.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HashAlgo {
    #[default]
    Blake3,
    /// 16-byte hash from the `river5` crate (was `ddh128`, then
    /// briefly `river128` before the latest rename). Old persisted
    /// settings carrying any of those names are accepted via serde
    /// aliases so old checkpoints load cleanly.
    #[serde(alias = "Ddh128", alias = "River128")]
    River5,
}

impl HashAlgo {
    /// Fixed output byte count for this algorithm.
    pub fn output_len(self) -> usize {
        match self {
            HashAlgo::Blake3 => 32,
            HashAlgo::River5 => 16,
        }
    }

    /// Lowercase tag stored in the cache schema's `hash_algo` column
    /// and printed in the diagnostics report.
    pub fn tag(self) -> &'static str {
        match self {
            HashAlgo::Blake3 => "blake3",
            HashAlgo::River5 => "river5",
        }
    }

    /// Parse the tag from cache/disk. Accepts the legacy `"ddh128"`
    /// string so existing schema-v2 rows can still be queried even
    /// though new rows go in as `"river5"`. Cache schema bumps to
    /// v3 below to invalidate the old rows entirely if the user
    /// wants a clean slate — but the alias is here in case anyone
    /// downgrades or rolls forward without a schema reset.
    pub fn from_tag(s: &str) -> Option<Self> {
        match s {
            "blake3" => Some(HashAlgo::Blake3),
            "river5" | "ddh128" => Some(HashAlgo::River5),
            _ => None,
        }
    }
}

/// Backend dispatch for streaming-hashed content. The two variants
/// have very different sizes (`blake3::Hasher` is ~2 KiB while
/// `river5::Hasher` is small) — we don't `Box` to dodge the
/// `large_enum_variant` lint because the hasher is constructed on
/// every per-file hash call and an extra heap allocation per file
/// would more than wipe out any size saving on this hot path.
#[allow(clippy::large_enum_variant)]
pub enum ContentHasher {
    Blake3(blake3::Hasher),
    River5(river5::Hasher),
}

impl ContentHasher {
    pub fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Blake3 => ContentHasher::Blake3(blake3::Hasher::new()),
            HashAlgo::River5 => ContentHasher::River5(river5::Hasher::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            ContentHasher::Blake3(h) => {
                h.update(data);
            }
            ContentHasher::River5(h) => {
                h.update(data);
            }
        }
    }

    /// Consume the hasher and return its output. Width depends on the
    /// algorithm — see [`HashAlgo::output_len`].
    pub fn finalize(self) -> Vec<u8> {
        match self {
            ContentHasher::Blake3(h) => h.finalize().as_bytes().to_vec(),
            ContentHasher::River5(h) => h.finalize().to_vec(),
        }
    }
}

/// One-shot convenience for hashing a slice. Identical to
/// `ContentHasher::new(algo).update(data).finalize()` but doesn't
/// allocate the wrapper.
pub fn hash_oneshot(algo: HashAlgo, data: &[u8]) -> Vec<u8> {
    match algo {
        HashAlgo::Blake3 => blake3::hash(data).as_bytes().to_vec(),
        HashAlgo::River5 => river5::hash(data).to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake3_output_is_32_bytes() {
        let h = hash_oneshot(HashAlgo::Blake3, b"superdupe");
        assert_eq!(h.len(), 32);
    }

    #[test]
    fn river128_output_is_16_bytes() {
        let h = hash_oneshot(HashAlgo::River5, b"superdupe");
        assert_eq!(h.len(), 16);
    }

    #[test]
    fn streaming_matches_oneshot() {
        for algo in [HashAlgo::Blake3, HashAlgo::River5] {
            let mut h = ContentHasher::new(algo);
            h.update(b"super");
            h.update(b"dupe");
            let streamed = h.finalize();
            let one = hash_oneshot(algo, b"superdupe");
            assert_eq!(streamed, one, "streaming != one-shot for {algo:?}");
        }
    }

    #[test]
    fn algos_produce_different_outputs() {
        let a = hash_oneshot(HashAlgo::Blake3, b"x");
        let b = hash_oneshot(HashAlgo::River5, b"x");
        assert_ne!(a, b);
    }

    #[test]
    fn tag_roundtrips() {
        for algo in [HashAlgo::Blake3, HashAlgo::River5] {
            assert_eq!(HashAlgo::from_tag(algo.tag()), Some(algo));
        }
    }

    #[test]
    fn from_tag_accepts_legacy_ddh128() {
        assert_eq!(HashAlgo::from_tag("ddh128"), Some(HashAlgo::River5));
    }
}
