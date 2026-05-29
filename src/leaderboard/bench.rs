//! T-BENCH-ME canonical-bench primitives — the FROZEN byte-exact cores
//! (design 2026-05-29, research-locked). MUST match web's #160 verifier
//! byte-for-byte; the primitives here are covered by self-checking tests
//! (determinism, random-access==sequential, tree-structure / odd-node) and
//! will be cross-checked against research's golden vectors before any dev
//! binary submits to a ranked board.
//!
//! FROZEN spec:
//! - corpus content: RFC-8439 ChaCha20 keystream, O(1) random access.
//!   `content(c) = ChaCha20(K_content, nonce = u96le(c))`; the chunk at byte
//!   `offset` is the keystream seeked to `offset` (block = offset/64).
//! - subkeys: `K_content = BLAKE3(seed‖0x01)`, `K_control = BLAKE3(seed‖0x02)`.
//! - Merkle leaf = `BLAKE3(0x00 ‖ u32le(path_len) ‖ path_utf8_nfc ‖
//!   u64le(offset) ‖ u64le(len) ‖ chunk[≤1MiB])`.
//! - Merkle node = `BLAKE3(0x01 ‖ left ‖ right)` (32B children, no len prefix).
//! - tree: RFC-6962, PROMOTE-LAST odd handling (NEVER duplicate a lone node —
//!   CVE-2012-2459 guard); split at the largest power of two < n; 1-leaf root
//!   = the leaf hash itself.
//! - root wire form = std-base64 (RFC4648, WITH padding).
//! - all in-hash integers = little-endian, fixed width.
#![cfg(feature = "telemetry")]

use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::ChaCha20;

/// 1 MiB Merkle-leaf chunk size (FROZEN). The final chunk of a content may be
/// shorter; its leaf `len` reflects the actual bytes.
pub const CHUNK_SIZE: u64 = 1 << 20;

const TAG_LEAF: u8 = 0x00;
const TAG_NODE: u8 = 0x01;

/// Derive the corpus subkeys from the 32-byte seed (FROZEN):
/// `K_content = BLAKE3(seed‖0x01)`, `K_control = BLAKE3(seed‖0x02)`.
pub fn corpus_keys(seed: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let derive = |tag: u8| {
        let mut h = blake3::Hasher::new();
        h.update(seed);
        h.update(&[tag]);
        *h.finalize().as_bytes()
    };
    (derive(0x01), derive(0x02))
}

/// 12-byte (u96 little-endian) ChaCha20 nonce for `content_id`. content_id is
/// a u64 in corpus-v1: the low 8 bytes are `content_id.to_le_bytes()`, the
/// high 4 bytes are zero (u96le of a u64). NOTE: confirm the content_id width
/// + nonce packing against research's golden vector before shipping — this is
/// exactly the byte-exact detail a vector pins.
fn content_nonce(content_id: u64) -> [u8; 12] {
    let mut n = [0u8; 12];
    n[..8].copy_from_slice(&content_id.to_le_bytes());
    n
}

/// Fill `buf` with `content(content_id)` keystream bytes starting at byte
/// `offset`. O(1) random access (StreamCipherSeek → block offset/64) — the
/// property web relies on to verify a sampled leaf without the full corpus.
pub fn content_bytes_at(k_content: &[u8; 32], content_id: u64, offset: u64, buf: &mut [u8]) {
    let key = chacha20::Key::from(*k_content);
    let nonce = chacha20::Nonce::from(content_nonce(content_id));
    let mut cipher = ChaCha20::new(&key, &nonce);
    cipher.seek(offset);
    for b in buf.iter_mut() {
        *b = 0;
    }
    cipher.apply_keystream(buf);
}

/// Merkle leaf hash (FROZEN). `path` must be the canonical relative path in
/// UTF-8 NFC with '/' separators. corpus-v1 paths (`f{index:010}.bin`) are
/// ASCII, so NFC is identity; a non-ASCII path would require an NFC step
/// (not added — corpus paths are ASCII by spec).
pub fn leaf_hash(path: &str, offset: u64, len: u64, chunk: &[u8]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[TAG_LEAF]);
    h.update(&(path.len() as u32).to_le_bytes());
    h.update(path.as_bytes());
    h.update(&offset.to_le_bytes());
    h.update(&len.to_le_bytes());
    h.update(chunk);
    *h.finalize().as_bytes()
}

/// Merkle node hash (FROZEN): `BLAKE3(0x01 ‖ left ‖ right)`.
pub fn node_hash(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    h.update(&[TAG_NODE]);
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

/// The RFC-6962 split point for `n` leaves: the largest power of two strictly
/// less than `n` (`n >= 2`). The left subtree takes `[..k]`, the right `[k..]`.
fn split_point(n: usize) -> usize {
    let mut k = 1usize;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// RFC-6962 Merkle root over `leaves` with PROMOTE-LAST odd handling.
/// `[]` → None; `[h]` → `h` (1-leaf root is the leaf itself, no node wrap);
/// else split at the largest power of two strictly less than `n`. Never
/// duplicates a lone node (CVE-2012-2459 guard).
pub fn merkle_root(leaves: &[[u8; 32]]) -> Option<[u8; 32]> {
    match leaves.len() {
        0 => None,
        1 => Some(leaves[0]),
        n => {
            let k = split_point(n);
            let left = merkle_root(&leaves[..k]).expect("non-empty left split");
            let right = merkle_root(&leaves[k..]).expect("non-empty right split");
            Some(node_hash(&left, &right))
        }
    }
}

/// RFC-6962 audit (inclusion) path for the leaf at index `m` among `leaves`,
/// deepest sibling FIRST. Pairs with [`root_from_path`]: a verifier holding
/// only the sampled leaf + this path can reconstruct the root without the rest
/// of the corpus — the property the bench challenge relies on. Same
/// promote-last split as [`merkle_root`]; byte-exact with research's reference.
pub fn audit_path(m: usize, leaves: &[[u8; 32]]) -> Vec<[u8; 32]> {
    let n = leaves.len();
    if n <= 1 {
        return vec![];
    }
    let k = split_point(n);
    if m < k {
        let mut p = audit_path(m, &leaves[..k]);
        p.push(merkle_root(&leaves[k..]).expect("non-empty right split"));
        p
    } else {
        let mut p = audit_path(m - k, &leaves[k..]);
        p.push(merkle_root(&leaves[..k]).expect("non-empty left split"));
        p
    }
}

/// Reconstruct the Merkle root from a sampled `leaf`, its [`audit_path`], its
/// leaf index `m`, and the total `leaf_count` `n`. The verifier's half of the
/// inclusion proof: equals [`merkle_root`] iff the leaf is genuinely at `m`.
pub fn root_from_path(leaf: [u8; 32], path: &[[u8; 32]], m: usize, n: usize) -> [u8; 32] {
    let rev: Vec<[u8; 32]> = path.iter().rev().copied().collect();
    fn rec(m: usize, n: usize, leaf: [u8; 32], rev: &[[u8; 32]], i: usize) -> [u8; 32] {
        if n == 1 {
            return leaf;
        }
        let k = split_point(n);
        let sib = rev[i];
        if m < k {
            node_hash(&rec(m, k, leaf, rev, i + 1), &sib)
        } else {
            node_hash(&sib, &rec(m - k, n - k, leaf, rev, i + 1))
        }
    }
    rec(m, n, leaf, &rev, 0)
}

/// Split one corpus file into its 1MiB-chunk Merkle leaves, in offset order
/// (path-lex/offset order). The final chunk may be shorter; its leaf `len`
/// reflects the actual bytes. Content is generated on the fly via the O(1)
/// random-access keystream — no disk read. Shared by the golden-vector test
/// and the corpus generator (`super::bench_corpus`). `size == 0` → no leaves.
pub fn file_leaves(k_content: &[u8; 32], path: &str, content_id: u64, size: u64) -> Vec<[u8; 32]> {
    let mut leaves = Vec::new();
    let mut off = 0u64;
    while off < size {
        let len = (size - off).min(CHUNK_SIZE);
        let mut chunk = vec![0u8; len as usize];
        content_bytes_at(k_content, content_id, off, &mut chunk);
        leaves.push(leaf_hash(path, off, len, &chunk));
        off += len;
    }
    leaves
}

/// std-base64 (RFC4648, WITH padding) of the 32-byte Merkle root — the FROZEN
/// wire form (44 chars; NOT base64url).
pub fn root_base64(root: &[u8; 32]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(root)
}

/// Single-round challenge positions (FROZEN): for k = 0,1,2,…
/// `h_k = BLAKE3(ascii(bench_challenge_id) ‖ u32le(k))`; `pos =
/// u64le(h_k[0..8]) mod leaf_count`; collect distinct positions until `n`
/// (32 for v1) or until every leaf is covered.
pub fn challenge_positions(bench_challenge_id: &str, leaf_count: u64, n: usize) -> Vec<u64> {
    let target = n.min(leaf_count as usize);
    let mut out = Vec::with_capacity(target);
    let mut seen = std::collections::HashSet::with_capacity(target);
    let mut k: u32 = 0;
    while out.len() < target {
        let mut h = blake3::Hasher::new();
        h.update(bench_challenge_id.as_bytes());
        h.update(&k.to_le_bytes());
        let d = h.finalize();
        let pos = u64::from_le_bytes(d.as_bytes()[0..8].try_into().unwrap()) % leaf_count;
        if seen.insert(pos) {
            out.push(pos);
        }
        k = k.wrapping_add(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_keys_deterministic_and_distinct() {
        let seed = [7u8; 32];
        let (kc1, kk1) = corpus_keys(&seed);
        let (kc2, kk2) = corpus_keys(&seed);
        assert_eq!(kc1, kc2, "K_content deterministic");
        assert_eq!(kk1, kk2, "K_control deterministic");
        assert_ne!(kc1, kk1, "K_content != K_control (different tag)");
        assert_ne!(corpus_keys(&[8u8; 32]).0, kc1, "different seed -> different key");
    }

    #[test]
    fn content_is_deterministic() {
        let (kc, _) = corpus_keys(&[1u8; 32]);
        let mut a = [0u8; 4096];
        let mut b = [0u8; 4096];
        content_bytes_at(&kc, 42, 0, &mut a);
        content_bytes_at(&kc, 42, 0, &mut b);
        assert_eq!(a, b, "same (key, content_id, offset) -> same bytes");
        let mut other = [0u8; 4096];
        content_bytes_at(&kc, 43, 0, &mut other);
        assert_ne!(a, other, "different content_id -> different bytes");
    }

    #[test]
    fn random_access_equals_sequential() {
        // The load-bearing property web depends on: a chunk read at an
        // arbitrary offset == the same window of the full sequential stream.
        let (kc, _) = corpus_keys(&[9u8; 32]);
        let mut full = [0u8; 8192];
        content_bytes_at(&kc, 5, 0, &mut full);
        // read a window straddling ChaCha block boundaries (block = 64B).
        for &(off, len) in &[(0usize, 100usize), (64, 64), (100, 300), (4096, 1000), (8000, 192)] {
            let mut window = vec![0u8; len];
            content_bytes_at(&kc, 5, off as u64, &mut window);
            assert_eq!(
                &window[..],
                &full[off..off + len],
                "random-access chunk at offset {off} must equal the sequential window"
            );
        }
    }

    #[test]
    fn leaf_hash_field_sensitive() {
        let base = leaf_hash("f0000000001.bin", 0, 16, b"hello-chunk-data");
        // every field independently changes the hash (no field collision).
        assert_ne!(base, leaf_hash("f0000000002.bin", 0, 16, b"hello-chunk-data"));
        assert_ne!(base, leaf_hash("f0000000001.bin", 1048576, 16, b"hello-chunk-data"));
        assert_ne!(base, leaf_hash("f0000000001.bin", 0, 17, b"hello-chunk-data"));
        assert_ne!(base, leaf_hash("f0000000001.bin", 0, 16, b"HELLO-chunk-data"));
        // stable across calls.
        assert_eq!(base, leaf_hash("f0000000001.bin", 0, 16, b"hello-chunk-data"));
        // domain separation: a leaf is never equal to a node of the same bytes.
        assert_ne!(leaf_hash("", 0, 0, b""), node_hash(&[0u8; 32], &[0u8; 32]));
    }

    #[test]
    fn merkle_promote_last_structure() {
        let l: Vec<[u8; 32]> = (0u8..5).map(|i| [i; 32]).collect();
        assert_eq!(merkle_root(&[]), None, "empty -> None");
        assert_eq!(merkle_root(&l[..1]), Some(l[0]), "1-leaf root = the leaf, no node wrap");
        // 2 leaves -> node(l0,l1).
        assert_eq!(merkle_root(&l[..2]), Some(node_hash(&l[0], &l[1])));
        // 3 leaves: split at largest pow2<3 = 2 -> node(node(l0,l1), l2);
        // the lone l2 is PROMOTED, never duplicated.
        let expect3 = node_hash(&node_hash(&l[0], &l[1]), &l[2]);
        assert_eq!(merkle_root(&l[..3]), Some(expect3));
        // 5 leaves: split at 4 -> node( root([0..4]), l4 ).
        let left4 = node_hash(&node_hash(&l[0], &l[1]), &node_hash(&l[2], &l[3]));
        assert_eq!(merkle_root(&l[..5]), Some(node_hash(&left4, &l[4])));
        // promote-last guard: a 3-leaf root must NOT equal the duplicate-last form.
        let dup_last = node_hash(&node_hash(&l[0], &l[1]), &node_hash(&l[2], &l[2]));
        assert_ne!(merkle_root(&l[..3]).unwrap(), dup_last, "must not duplicate the lone node");
    }

    #[test]
    fn root_base64_is_padded_standard_44_chars() {
        let s = root_base64(&[0xABu8; 32]);
        assert_eq!(s.len(), 44, "32B -> 44 chars std-base64 with padding");
        assert!(s.ends_with('='), "std-base64 padding present");
        assert!(!s.contains('-') && !s.contains('_'), "std alphabet, not base64url");
    }

    // CROSS-IMPL byte-match against web's #160 verifier reference vector
    // (web msg 2026-05-29). web's leaf = BLAKE3(0x00 || preimage) over raw
    // ascii preimages; this locks the TREE half (node_hash + merkle_root +
    // promote-last) byte-for-byte with web. (Our real-corpus leaf wraps a
    // STRUCTURED preimage — see leaf_hash — but the node/tree fold is identical.)
    #[test]
    fn merkle_matches_web_cross_impl_vector() {
        let web_leaf = |s: &str| {
            let mut h = blake3::Hasher::new();
            h.update(&[TAG_LEAF]);
            h.update(s.as_bytes());
            *h.finalize().as_bytes()
        };
        let leaves: Vec<[u8; 32]> = (0..5).map(|i| web_leaf(&format!("leaf-{i}"))).collect();
        // per-leaf hashes agree with web.
        assert_eq!(root_base64(&leaves[0]), "E+22yZg+AO9liNscI7PB0yA4Jr6oJT9J29d2Nvo1Yaw=");
        assert_eq!(root_base64(&leaves[1]), "i3iIxRiNVZRjoKxJgYfHK6bUj3t7vWjU8QtIKZAt2pU=");
        assert_eq!(root_base64(&leaves[4]), "6qFHPJwbwGVOwTLH06FNqw0zXybw62KFcUQNGe/Ncd4=");
        // the 5-leaf root matches web byte-for-byte -> tree/node half LOCKED.
        let root = merkle_root(&leaves).expect("5 leaves");
        assert_eq!(
            root_base64(&root),
            "5Tvt8Ma0OZJK521yNAjsfovZgJxS+3yEUrsmrKHgDbc=",
            "merkle_root must match web's #160 reference vector byte-for-byte"
        );
        // web's intermediate node(l0,l1) appears in the path_index=3 proof.
        assert_eq!(
            root_base64(&node_hash(&leaves[0], &leaves[1])),
            "RGnpwcmaZ0F6Vn5+PLJSrAOBqSuGo7IvzymH2etAFmk="
        );
    }

    // Emits a reciprocal cross-impl vector for web (#160): a tiny real corpus
    // (ChaCha20 content -> STRUCTURED leaf -> promote-last root) web can verify
    // on dev. Run: cargo test --features telemetry -- --nocapture
    //   print_reciprocal_vector_for_web
    // Asserts internal determinism; the printed values go to web-superdeduper.
    #[test]
    fn print_reciprocal_vector_for_web() {
        use base64::Engine;
        let b64 = |h: &[u8; 32]| base64::engine::general_purpose::STANDARD.encode(h);
        let seed = [0x42u8; 32];
        let (kc, _kk) = corpus_keys(&seed);
        // 3 files, 1 chunk each (size <= 1MiB). f2 shares content_id 0 with f0
        // => exact-dup ground truth. path-lex order == index.
        let files = [
            ("f0000000000.bin", 0u64, 100u64),
            ("f0000000001.bin", 1u64, 100u64),
            ("f0000000002.bin", 0u64, 100u64),
        ];
        let mut leaves = Vec::new();
        eprintln!("--- RECIPROCAL VECTOR (engine -> web #160) ---");
        eprintln!("seed_hex: {}", hex32(&seed));
        eprintln!("kdf: K_content=BLAKE3(seed||0x01); content(c)=ChaCha20(K_content,nonce=u96le(c)), nonce=c.to_le_bytes()(8)||[0;4]");
        eprintln!("leaf=BLAKE3(0x00||u32le(path_len)||path_utf8||u64le(off)||u64le(len)||chunk); node=BLAKE3(0x01||L||R); RFC-6962 promote-last");
        for (path, cid, size) in files {
            let mut chunk = vec![0u8; size as usize];
            content_bytes_at(&kc, cid, 0, &mut chunk);
            let lh = leaf_hash(path, 0, size, &chunk);
            eprintln!("file path={path} content_id={cid} off=0 len={size} chunk_b64_first16={} leaf={}",
                base64::engine::general_purpose::STANDARD.encode(&chunk[..16.min(chunk.len())]), b64(&lh));
            leaves.push(lh);
        }
        let root = merkle_root(&leaves).unwrap();
        eprintln!("merkle_root: {}", b64(&root));
        // sample proof for path_index=1 (leaf_count=3, promote-last):
        // tree = node(node(l0,l1), l2); leaf1 sibling=l0, then sibling=l2.
        eprintln!("sample_proof path_index=1 leaf_count=3 merkle_path=[{}, {}]", b64(&leaves[0]), b64(&leaves[2]));
        // determinism self-check.
        let (kc2, _) = corpus_keys(&seed);
        let mut c2 = vec![0u8; 100];
        content_bytes_at(&kc2, 0, 0, &mut c2);
        let mut c1 = vec![0u8; 100];
        content_bytes_at(&kc, 0, 0, &mut c1);
        assert_eq!(c1, c2, "reciprocal vector content is deterministic");
        assert_eq!(merkle_root(&leaves).unwrap(), root, "root reproducible");
    }

    fn hex32(b: &[u8; 32]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    // RESEARCH GOLDEN-VECTOR cross-check (/tmp/tcorpus-goldenvec, design
    // 2026-05-29). Reproduces research's reference impl byte-for-byte:
    // seed=000102…1f, K_content, the 8-file/9-leaf fixture (f7 spans 2 chunks),
    // and the merkle_root. Triple-locks the engine cores (self + web #160 +
    // research golden). The >1MiB f7 also exercises seek-based random-access
    // at a real chunk boundary (offset 1MiB) matching the sequential stream.
    #[test]
    fn matches_research_golden_vector() {
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        let (kc, _) = corpus_keys(&seed);
        assert_eq!(
            hex32(&kc),
            "358c10779b09eea6a01af531bdc0ea2b6e465f575b7856b8df608cb6e3fc86c0",
            "K_content = BLAKE3(seed||0x01) must match research golden"
        );
        // golden 8-file fixture: (content_id, size). content_id 100 repeats
        // (f1/f4/f6) -> dupset; f7 is 1MiB+64 -> 2 leaves.
        let files: [(u64, u64); 8] = [
            (0, 16),
            (100, 512),
            (2, 256),
            (3, 512),
            (100, 512),
            (5, 1000),
            (100, 512),
            (7, 1_048_640),
        ];
        let mut leaves = Vec::new();
        for (i, (cid, size)) in files.iter().enumerate() {
            let path = format!("f{i:010}.bin");
            leaves.extend(file_leaves(&kc, &path, *cid, *size));
        }
        assert_eq!(leaves.len(), 9, "8 files, f7 spans 2 chunks -> 9 leaves");
        // a few golden leaf hashes (incl f7's two chunks -> exercises seek).
        use base64::Engine;
        let b64 = |h: &[u8; 32]| base64::engine::general_purpose::STANDARD.encode(h);
        assert_eq!(b64(&leaves[0]), "ggmH46W4Dp9u3N5AXG0iERpd/GHfMeUo4bocn2iLt34=", "f0 leaf");
        assert_eq!(b64(&leaves[7]), "pxqy/xV+P02qOGxqWVeFSLx+mDBR5alN9eFLMZsqvcg=", "f7 chunk0 leaf");
        assert_eq!(b64(&leaves[8]), "qm4pHNZuL06BZV6C4OQgV4JS1REYIRfuS+0x7pfTqAE=", "f7 chunk1 leaf (offset 1MiB, seek)");
        // THE golden root, byte-for-byte.
        assert_eq!(
            root_base64(&merkle_root(&leaves).unwrap()),
            "z6vzmw41RQT0EdEUE+zDHXlPBddgpSmx9OUopWvTl2w=",
            "merkle_root must match research golden vector byte-for-byte"
        );
    }

    #[test]
    fn audit_path_reconstructs_root_for_every_position() {
        // Rebuild the research golden 9-leaf tree; every leaf's audit path must
        // reconstruct the golden root byte-for-byte (the web challenge's exact
        // verification path). Locks the inclusion-proof half cross-impl.
        let mut seed = [0u8; 32];
        for (i, b) in seed.iter_mut().enumerate() {
            *b = i as u8;
        }
        let (kc, _) = corpus_keys(&seed);
        let files: [(u64, u64); 8] = [
            (0, 16), (100, 512), (2, 256), (3, 512),
            (100, 512), (5, 1000), (100, 512), (7, 1_048_640),
        ];
        let mut leaves = Vec::new();
        for (i, (cid, size)) in files.iter().enumerate() {
            leaves.extend(file_leaves(&kc, &format!("f{i:010}.bin"), *cid, *size));
        }
        assert_eq!(leaves.len(), 9);
        let root = merkle_root(&leaves).unwrap();
        assert_eq!(root_base64(&root), "z6vzmw41RQT0EdEUE+zDHXlPBddgpSmx9OUopWvTl2w=");
        for m in 0..leaves.len() {
            let path = audit_path(m, &leaves);
            let recon = root_from_path(leaves[m], &path, m, leaves.len());
            assert_eq!(recon, root, "audit path for leaf {m} must reconstruct the golden root");
        }
        // a wrong leaf must NOT reconstruct the root (proof is sound).
        let bad = root_from_path([0xFFu8; 32], &audit_path(3, &leaves), 3, leaves.len());
        assert_ne!(bad, root, "a forged leaf must not reconstruct the root");
    }

    #[test]
    fn audit_path_matches_reciprocal_vector_structure() {
        // 3-leaf promote-last tree: leaf 1's path = [l0, l2] (deepest sibling
        // first) — exactly the sample_proof emitted to web in the reciprocal
        // vector (print_reciprocal_vector_for_web).
        let l: Vec<[u8; 32]> = (0u8..3).map(|i| [i; 32]).collect();
        assert_eq!(audit_path(1, &l), vec![l[0], l[2]]);
        assert_eq!(audit_path(0, &l), vec![l[1], l[2]]);
        assert_eq!(audit_path(2, &l), vec![node_hash(&l[0], &l[1])]);
        for m in 0..3 {
            assert_eq!(root_from_path(l[m], &audit_path(m, &l), m, 3), merkle_root(&l).unwrap());
        }
    }

    #[test]
    fn challenge_positions_distinct_deterministic_in_range() {
        let a = challenge_positions("bench-chal-xyz", 1000, 32);
        let b = challenge_positions("bench-chal-xyz", 1000, 32);
        assert_eq!(a, b, "deterministic for the same challenge_id");
        assert_eq!(a.len(), 32, "32 positions for a large corpus");
        assert!(a.iter().all(|&p| p < 1000), "all positions in range");
        let set: std::collections::HashSet<_> = a.iter().collect();
        assert_eq!(set.len(), a.len(), "positions are distinct");
        assert_ne!(a, challenge_positions("different-chal", 1000, 32), "different challenge -> different positions");
        // tiny corpus: can't exceed leaf_count distinct positions.
        assert_eq!(challenge_positions("c", 5, 32).len(), 5);
    }
}
