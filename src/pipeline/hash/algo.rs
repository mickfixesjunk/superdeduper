//! Pluggable content-hash algorithm.
//!
//! Two backends today: cryptographic-grade [`HashAlgo::Blake3`] (32-byte
//! output, SIMD + parallel chunk-tree internals) and the new
//! [`HashAlgo::Ddh128`] which produces a 16-byte output and exists as
//! a stub today wrapping xxhash3-128 — the AES-NI core lands in
//! 0.2.0.
//!
//! Engine code never touches a `blake3::Hasher` or `ddh128::Hasher`
//! directly; instead it constructs a [`ContentHasher`] from the
//! algo chosen in [`ScanSettings::hash_algo`] and uses the same
//! `new` / `update` / `finalize` shape regardless of which backend
//! is selected. Outputs are returned as `Vec<u8>` because the two
//! algorithms have different sizes (32 vs 16 bytes).

use serde::{Deserialize, Serialize};

/// Which content-hash algorithm a scan should use. Persisted via
/// `ScanSettings` and recorded in the cache row so warm rescans
/// don't mix Blake3 and Ddh128 outputs by accident.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HashAlgo {
    Blake3,
    Ddh128,
}

impl Default for HashAlgo {
    fn default() -> Self {
        HashAlgo::Blake3
    }
}

impl HashAlgo {
    /// Fixed output byte count for this algorithm.
    pub fn output_len(self) -> usize {
        match self {
            HashAlgo::Blake3 => 32,
            HashAlgo::Ddh128 => 16,
        }
    }

    /// Lowercase tag stored in the cache schema's `hash_algo` column
    /// and printed in the diagnostics report. Stable across versions
    /// so old cache rows stay queryable.
    pub fn tag(self) -> &'static str {
        match self {
            HashAlgo::Blake3 => "blake3",
            HashAlgo::Ddh128 => "ddh128",
        }
    }

    pub fn from_tag(s: &str) -> Option<Self> {
        match s {
            "blake3" => Some(HashAlgo::Blake3),
            "ddh128" => Some(HashAlgo::Ddh128),
            _ => None,
        }
    }
}

/// Backend dispatch for streaming-hashed content. Sized for stack
/// allocation; both variants fit in a few hundred bytes.
pub enum ContentHasher {
    Blake3(blake3::Hasher),
    Ddh128(ddh128::Hasher),
}

impl ContentHasher {
    pub fn new(algo: HashAlgo) -> Self {
        match algo {
            HashAlgo::Blake3 => ContentHasher::Blake3(blake3::Hasher::new()),
            HashAlgo::Ddh128 => ContentHasher::Ddh128(ddh128::Hasher::new()),
        }
    }

    pub fn update(&mut self, data: &[u8]) {
        match self {
            ContentHasher::Blake3(h) => {
                h.update(data);
            }
            ContentHasher::Ddh128(h) => {
                h.update(data);
            }
        }
    }

    /// Consume the hasher and return its output. Width depends on the
    /// algorithm — see [`HashAlgo::output_len`].
    pub fn finalize(self) -> Vec<u8> {
        match self {
            ContentHasher::Blake3(h) => h.finalize().as_bytes().to_vec(),
            ContentHasher::Ddh128(h) => h.finalize().to_vec(),
        }
    }
}

/// One-shot convenience for hashing a slice. Identical to
/// `ContentHasher::new(algo).update(data).finalize()` but doesn't
/// allocate the wrapper.
pub fn hash_oneshot(algo: HashAlgo, data: &[u8]) -> Vec<u8> {
    match algo {
        HashAlgo::Blake3 => blake3::hash(data).as_bytes().to_vec(),
        HashAlgo::Ddh128 => ddh128::hash(data).to_vec(),
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
    fn ddh128_output_is_16_bytes() {
        let h = hash_oneshot(HashAlgo::Ddh128, b"superdupe");
        assert_eq!(h.len(), 16);
    }

    #[test]
    fn streaming_matches_oneshot() {
        for algo in [HashAlgo::Blake3, HashAlgo::Ddh128] {
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
        let b = hash_oneshot(HashAlgo::Ddh128, b"x");
        assert_ne!(a, b);
    }

    #[test]
    fn tag_roundtrips() {
        for algo in [HashAlgo::Blake3, HashAlgo::Ddh128] {
            assert_eq!(HashAlgo::from_tag(algo.tag()), Some(algo));
        }
    }
}
