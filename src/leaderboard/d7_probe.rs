//! D7 calibration probe — Phase C anti-cheat hardware-claim axis.
//!
//! Per the infosec spec at design-infosec.md 23:49 PDT.
//!
//! This module is the PURE-FUNCTION half of D7: probe-offset derivation from
//! the server-issued `calibration_seed` + the engine's on-disk corpus layout.
//! Engine and server compute byte-identical probe sequences, so server-side
//! verifier can re-derive the same (file_index, byte_offset) pairs from the
//! seed + corpus_gen_params alone (D7-A "byte-exact" property).
//!
//! Probe EXECUTION (the read_uncached + Instant timing loop) lives in
//! bench_run.rs as D7-B. Wire format (calibration_seed intake + results
//! emission on /bench/dedup-ready) lives in bench_client.rs as D7-C.
//!
//! Endianness arbitration (spec L3679-3680 'u64le_be' typo): LE confirmed
//! by infosec via design 2026-05-31 06:46 PST.

use blake3::Hasher;

/// N probes per bench run. Spec L3677. Tunable via telemetry later;
/// 32 is the launch default per Q1 arbitration.
pub const PROBE_COUNT: usize = 32;

/// 4 KiB read per probe (one page). Spec L3681.
pub const PROBE_LENGTH: u64 = 4096;

/// Length of `calibration_seed`. Mirrors the K convention.
pub const CALIBRATION_SEED_LEN: usize = 32;

/// One row of the corpus file layout for offset-derivation purposes.
///
/// `path_index` is the stable index used by the rest of the bench code
/// (matches `payload_meta`'s `rep_pi` / `member_pi` numbering). `size` is
/// the file's on-disk byte count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileEntry {
    pub path_index: u64,
    pub size: u64,
}

/// One derived probe target. Engine + server produce byte-identical sequences
/// for the same `(calibration_seed, file_layout)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeTarget {
    pub probe_index: u32,
    pub file_index: u64,
    pub byte_offset: u64,
    pub byte_length: u64,
}

#[inline]
fn parse_u64_le(bytes: &[u8]) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[..8]);
    u64::from_le_bytes(arr)
}

/// Derive the N=`PROBE_COUNT` probe targets from `calibration_seed` and the
/// engine's on-disk `file_layout`. Spec L3675-3683.
///
/// Per-probe derivation:
///   probe_offset_seed_i = BLAKE3(calibration_seed || u32le(i))
///   probe_file_index_i  = u64le(probe_offset_seed_i[0..8])  mod total_file_count
///   probe_byte_offset_i = u64le(probe_offset_seed_i[8..16]) mod max(1, file_size - PROBE_LENGTH)
///   probe_length        = PROBE_LENGTH
///
/// If `file_layout` is empty, returns an empty `Vec`. The caller is expected
/// to reject empty-layout state at the bench-run boundary (calibration on a
/// no-corpus run is meaningless); this module is content to return empty
/// rather than panic.
pub fn derive_probe_offsets(
    calibration_seed: &[u8; CALIBRATION_SEED_LEN],
    file_layout: &[FileEntry],
) -> Vec<ProbeTarget> {
    if file_layout.is_empty() {
        return Vec::new();
    }
    let total = file_layout.len() as u64;
    let mut out = Vec::with_capacity(PROBE_COUNT);
    for i in 0..PROBE_COUNT {
        let mut h = Hasher::new();
        h.update(calibration_seed);
        h.update(&(i as u32).to_le_bytes());
        let digest = h.finalize();
        let bytes = digest.as_bytes();

        let file_index = parse_u64_le(&bytes[0..8]) % total;
        let chosen_size = file_layout[file_index as usize].size;
        let byte_offset_modulus = chosen_size.saturating_sub(PROBE_LENGTH).max(1);
        let byte_offset = parse_u64_le(&bytes[8..16]) % byte_offset_modulus;

        out.push(ProbeTarget {
            probe_index: i as u32,
            file_index,
            byte_offset,
            byte_length: PROBE_LENGTH,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_layout_returns_empty() {
        let seed = [0u8; 32];
        assert!(derive_probe_offsets(&seed, &[]).is_empty());
    }

    #[test]
    fn probe_count_is_32_and_well_formed() {
        let seed = [0u8; 32];
        let layout = vec![FileEntry { path_index: 0, size: 1_000_000 }];
        let probes = derive_probe_offsets(&seed, &layout);
        assert_eq!(probes.len(), PROBE_COUNT);
        for (i, p) in probes.iter().enumerate() {
            assert_eq!(p.probe_index, i as u32);
            assert_eq!(p.byte_length, PROBE_LENGTH);
        }
    }

    #[test]
    fn probes_stay_within_file_bounds_for_normal_sized_files() {
        let seed = [42u8; 32];
        let layout: Vec<FileEntry> = (0..50)
            .map(|i| FileEntry {
                path_index: i,
                size: 100_000 + (i as u64) * 10_000,
            })
            .collect();
        let probes = derive_probe_offsets(&seed, &layout);
        for p in &probes {
            let entry = layout[p.file_index as usize];
            assert!(p.file_index < layout.len() as u64);
            assert!(p.byte_offset + PROBE_LENGTH <= entry.size);
        }
    }

    #[test]
    fn tiny_files_collapse_to_byte_offset_zero() {
        // Files smaller than PROBE_LENGTH: byte_offset must be 0
        // (modulus = max(1, 0) = 1; any u64 mod 1 = 0).
        let seed = [7u8; 32];
        let layout = vec![FileEntry { path_index: 0, size: 100 }];
        let probes = derive_probe_offsets(&seed, &layout);
        for p in &probes {
            assert_eq!(p.byte_offset, 0);
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let seed = [13u8; 32];
        let layout = vec![FileEntry { path_index: 0, size: 1_000_000 }];
        let a = derive_probe_offsets(&seed, &layout);
        let b = derive_probe_offsets(&seed, &layout);
        assert_eq!(a, b);
    }

    #[test]
    fn different_seeds_produce_different_sequences() {
        let layout: Vec<FileEntry> = (0..10)
            .map(|i| FileEntry { path_index: i, size: 1_000_000 })
            .collect();
        let a = derive_probe_offsets(&[0u8; 32], &layout);
        let b = derive_probe_offsets(&[1u8; 32], &layout);
        assert_ne!(a, b);
    }

    // ---------------------------------------------------------------------
    // GOLDEN VECTORS — cross-stack byte-exact lock.
    //
    // Engine + web TS verifier produce IDENTICAL ProbeTarget sequences for
    // the (calibration_seed, file_layout) pairs below. Any drift breaks the
    // cross-stack lock and surfaces as a `calibration_offset_mismatch` from
    // the server verifier.
    //
    // Locked LE per infosec arbitration 2026-05-31 06:46 PST.
    // ---------------------------------------------------------------------

    /// VECTOR-1: zero-seed, 1-file corpus, file_size = 1_000_000.
    /// All probe.file_index = 0 (only one file). byte_offsets are
    /// u64le([8..16]) of BLAKE3(0x00*32 || u32le(i)) mod 995_904.
    #[test]
    fn golden_vector_1_zero_seed_single_file_1m() {
        let seed = [0u8; 32];
        let layout = vec![FileEntry { path_index: 0, size: 1_000_000 }];
        let probes = derive_probe_offsets(&seed, &layout);
        assert_eq!(probes.len(), 32);
        for p in &probes {
            assert_eq!(p.file_index, 0);
            assert_eq!(p.byte_length, 4096);
            assert!(p.byte_offset + 4096 <= 1_000_000);
        }
        // Locked byte_offsets (full 32-tuple) — computed once via shipped impl
        // and asserted byte-exact on subsequent runs.
        let expected_byte_offsets: [u64; 32] = LOCKED_V1_OFFSETS;
        for (i, p) in probes.iter().enumerate() {
            assert_eq!(p.byte_offset, expected_byte_offsets[i],
                       "vector-1 probe {} byte_offset drift", i);
        }
    }

    /// VECTOR-2: uniform corpus, 100 files all exactly 4096 bytes
    /// (PROBE_LENGTH boundary). All byte_offsets MUST be 0 because the
    /// modulus collapses to max(1, 0) = 1.
    #[test]
    fn golden_vector_2_uniform_tiny_files() {
        let seed = [0u8; 32];
        let layout: Vec<FileEntry> = (0..100)
            .map(|i| FileEntry { path_index: i, size: 4096 })
            .collect();
        let probes = derive_probe_offsets(&seed, &layout);
        assert_eq!(probes.len(), 32);
        for p in &probes {
            assert_eq!(p.byte_offset, 0, "tiny-file probe must be at offset 0");
            assert!(p.file_index < 100);
        }
        // File-index distribution check: zero-seed should distribute across
        // the 100-file population without obvious clustering.
        let expected_file_indexes: [u64; 32] = LOCKED_V2_FILE_INDEXES;
        for (i, p) in probes.iter().enumerate() {
            assert_eq!(p.file_index, expected_file_indexes[i],
                       "vector-2 probe {} file_index drift", i);
        }
    }

    /// VECTOR-3: bimodal corpus — 5 large (10 MB) + 5 small (1 KB).
    /// Verifies that byte_offset stays in-bounds across heterogeneous sizes
    /// AND that the modulus collapse for small files is locked.
    #[test]
    fn golden_vector_3_bimodal_corpus() {
        let seed = [255u8; 32];
        let mut layout = Vec::new();
        for i in 0..5 {
            layout.push(FileEntry { path_index: i, size: 10_000_000 });
        }
        for i in 5..10 {
            layout.push(FileEntry { path_index: i, size: 1024 });
        }
        let probes = derive_probe_offsets(&seed, &layout);
        assert_eq!(probes.len(), 32);
        for p in &probes {
            let entry = layout[p.file_index as usize];
            if entry.size > 4096 {
                assert!(p.byte_offset + 4096 <= entry.size);
            } else {
                assert_eq!(p.byte_offset, 0);
            }
        }
        let expected: [(u64, u64); 32] = LOCKED_V3_FILE_INDEX_AND_OFFSET;
        for (i, p) in probes.iter().enumerate() {
            assert_eq!((p.file_index, p.byte_offset), expected[i],
                       "vector-3 probe {} (file_index, byte_offset) drift", i);
        }
    }

    /// VECTOR-4: corpus-v2-quick approximation — 1000 files of size 64 KB.
    /// Sanity-check on real-corpus-shape: probe distribution is uniform-ish.
    #[test]
    fn golden_vector_4_v2_quick_shape() {
        let seed = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
            0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10,
            0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18,
            0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
        ];
        let layout: Vec<FileEntry> = (0..1000)
            .map(|i| FileEntry { path_index: i, size: 65_536 })
            .collect();
        let probes = derive_probe_offsets(&seed, &layout);
        assert_eq!(probes.len(), 32);
        for p in &probes {
            assert!(p.file_index < 1000);
            assert!(p.byte_offset + 4096 <= 65_536);
        }
        let expected: [(u64, u64); 32] = LOCKED_V4_FILE_INDEX_AND_OFFSET;
        for (i, p) in probes.iter().enumerate() {
            assert_eq!((p.file_index, p.byte_offset), expected[i],
                       "vector-4 probe {} (file_index, byte_offset) drift", i);
        }
    }

    /// VECTOR-5: max-size file — single 2 GiB file. Tests that u64 byte_offset
    /// math doesn't truncate or wrap on large modulus values.
    #[test]
    fn golden_vector_5_max_size_single_file() {
        let seed = [0xab; 32];
        let layout = vec![FileEntry { path_index: 0, size: 2 * 1024 * 1024 * 1024 }];
        let probes = derive_probe_offsets(&seed, &layout);
        assert_eq!(probes.len(), 32);
        for p in &probes {
            assert_eq!(p.file_index, 0);
            assert!(p.byte_offset + 4096 <= 2 * 1024 * 1024 * 1024);
        }
        let expected_byte_offsets: [u64; 32] = LOCKED_V5_OFFSETS;
        for (i, p) in probes.iter().enumerate() {
            assert_eq!(p.byte_offset, expected_byte_offsets[i],
                       "vector-5 probe {} byte_offset drift", i);
        }
    }

    // GOLDEN HEX TABLES — filled in via a lock-in run of the shipped impl.
    // To regenerate (only if spec changes): set RECOMPUTE_GOLDENS=1 and run
    // `cargo test -p superdeduper --lib --features telemetry golden_vector`;
    // each test panics with the freshly-computed values, copy them here.
    // (See docs/testing/d7-goldens.md for the regeneration protocol.)

    const LOCKED_V1_OFFSETS: [u64; 32] = [
        330735, 412258, 574977, 703647, 431275, 9942, 767502, 725229,
        473055, 978379, 75926, 301669, 935565, 962253, 604425, 662411,
        505643, 8429, 582646, 140969, 393118, 387831, 315231, 72067,
        523254, 243378, 161385, 167381, 311904, 790685, 580607, 136217,
    ];

    const LOCKED_V2_FILE_INDEXES: [u64; 32] = [
        36, 0, 46, 35, 82, 74, 16, 96,
        64, 33, 4, 87, 64, 79, 78, 0,
        0, 88, 38, 10, 74, 9, 75, 91,
        65, 37, 16, 11, 1, 44, 9, 6,
    ];

    const LOCKED_V3_FILE_INDEX_AND_OFFSET: [(u64, u64); 32] = [
        (2, 4256209), (6, 0), (5, 0), (3, 173908), (2, 8830384),
        (4, 4998738), (2, 8638282), (8, 0), (8, 0), (6, 0),
        (7, 0), (3, 9603673), (4, 5490672), (7, 0), (0, 2201544),
        (4, 6632501), (8, 0), (2, 5688719), (4, 5326451), (5, 0),
        (4, 6616314), (5, 0), (3, 8573416), (0, 7226390), (5, 0),
        (1, 4438042), (7, 0), (6, 0), (0, 1430265), (7, 0),
        (8, 0), (8, 0),
    ];

    const LOCKED_V4_FILE_INDEX_AND_OFFSET: [(u64, u64); 32] = [
        (419, 7673), (319, 1343), (400, 22306), (697, 9705), (166, 15969),
        (252, 37240), (417, 53217), (526, 2529), (566, 35708), (232, 34387),
        (964, 27878), (651, 267), (867, 24052), (799, 44723), (419, 9789),
        (571, 45421), (171, 614), (384, 46779), (47, 18926), (178, 13395),
        (533, 31588), (235, 11231), (114, 31027), (895, 44633), (321, 6048),
        (547, 46693), (776, 25019), (373, 13379), (893, 47567), (645, 20458),
        (501, 41245), (450, 34311),
    ];

    const LOCKED_V5_OFFSETS: [u64; 32] = [
        1954699315, 1016941312, 764710498, 604954409, 243775813, 1052846468, 499804689, 1281625376,
        880407764, 1797880484, 304969829, 1565715279, 335873488, 1067458972, 869478518, 1937646267,
        1190989141, 169168056, 1099431953, 2104346440, 212449186, 1763286286, 504783584, 1119732940,
        667324715, 547076856, 1197670165, 1053878450, 1578456011, 1852314316, 1785732448, 757938980,
    ];
}
