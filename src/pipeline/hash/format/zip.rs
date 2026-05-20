//! ZIP / DOCX / XLSX / EPUB Tier 0 fingerprint.
//!
//! ZIP archives end with an "End of Central Directory" (EOCD) record
//! whose layout includes the offset and size of the Central Directory.
//! The CD lists every file in the archive with its name, uncompressed
//! size, and CRC32 — perfect fingerprint material that's a tiny
//! fraction of the archive's bytes.
//!
//! Fingerprint = BLAKE3 of the sorted-by-name list of
//! `(filename, uncompressed_size, crc32)` tuples from the CD.

use std::io::{Read, Seek, SeekFrom};


/// Maximum bytes to scan backwards from EOF when locating the EOCD.
/// The EOCD comment field can be up to 65 535 bytes, plus the 22-byte
/// fixed header. Add a small slack so we don't miss the boundary.
const EOCD_SEARCH: usize = 22 + 0xFFFF + 64;
const EOCD_SIG: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];
const CD_SIG: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
/// ZIP64 EOCD locator. Not parsed for fingerprinting; we rely on the
/// regular EOCD pointer or skip if archive is too big.
const _Z64_LOCATOR_SIG: [u8; 4] = [0x50, 0x4b, 0x06, 0x07];

pub fn fingerprint<R: Read + Seek>(r: &mut R, size: u64) -> std::io::Result<Vec<u8>> {
    let search_len = EOCD_SEARCH.min(size as usize);
    if search_len < 22 {
        return Err(io_err("file too small for a ZIP"));
    }
    let start = size - search_len as u64;
    r.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; search_len];
    r.read_exact(&mut buf)?;

    let eocd_off_in_buf = find_signature_back(&buf, &EOCD_SIG).ok_or_else(|| io_err("no EOCD"))?;
    let eocd = &buf[eocd_off_in_buf..];
    if eocd.len() < 22 {
        return Err(io_err("truncated EOCD"));
    }
    let cd_entries = u16::from_le_bytes([eocd[10], eocd[11]]) as usize;
    let cd_size = u32::from_le_bytes([eocd[12], eocd[13], eocd[14], eocd[15]]) as u64;
    let cd_off = u32::from_le_bytes([eocd[16], eocd[17], eocd[18], eocd[19]]) as u64;

    // ZIP64 signal: cd_off == 0xFFFFFFFF or cd_size == 0xFFFFFFFF means
    // the archive uses ZIP64 extensions. We don't parse those; fall
    // back to byte sampling for those (rare) archives.
    if cd_off == 0xFFFFFFFF || cd_size == 0xFFFFFFFF {
        return Err(io_err("ZIP64 unsupported in fingerprint"));
    }
    if cd_off + cd_size > size {
        return Err(io_err("CD pointer out of range"));
    }

    r.seek(SeekFrom::Start(cd_off))?;
    let mut cd = vec![0u8; cd_size as usize];
    r.read_exact(&mut cd)?;

    let mut entries: Vec<(Vec<u8>, u32, u32)> = Vec::with_capacity(cd_entries);
    let mut pos = 0usize;
    while pos + 46 <= cd.len() {
        if cd[pos..pos + 4] != CD_SIG {
            return Err(io_err("bad CD entry signature"));
        }
        let crc32 = u32::from_le_bytes([cd[pos + 16], cd[pos + 17], cd[pos + 18], cd[pos + 19]]);
        let uncompressed =
            u32::from_le_bytes([cd[pos + 24], cd[pos + 25], cd[pos + 26], cd[pos + 27]]);
        let name_len = u16::from_le_bytes([cd[pos + 28], cd[pos + 29]]) as usize;
        let extra_len = u16::from_le_bytes([cd[pos + 30], cd[pos + 31]]) as usize;
        let comment_len = u16::from_le_bytes([cd[pos + 32], cd[pos + 33]]) as usize;
        let name_start = pos + 46;
        let name_end = name_start + name_len;
        if name_end > cd.len() {
            return Err(io_err("CD name overflow"));
        }
        entries.push((cd[name_start..name_end].to_vec(), uncompressed, crc32));
        pos = name_end + extra_len + comment_len;
    }

    // Determinism: sort by filename so archive members listed in
    // different orders still match.
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"ZIP|");
    hasher.update(&(entries.len() as u32).to_le_bytes());
    for (name, sz, crc) in &entries {
        hasher.update(&(name.len() as u32).to_le_bytes());
        hasher.update(name);
        hasher.update(&sz.to_le_bytes());
        hasher.update(&crc.to_le_bytes());
    }
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn find_signature_back(haystack: &[u8], sig: &[u8; 4]) -> Option<usize> {
    if haystack.len() < 4 {
        return None;
    }
    let mut i = haystack.len() - 4;
    loop {
        if &haystack[i..i + 4] == sig {
            return Some(i);
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Hand-roll a minimal ZIP and verify a round-trip fingerprint.
    /// Files: `hello.txt` (uncompressed) and `world.txt` (uncompressed).
    fn build_minimal_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        // crc32 isn't strictly needed for parsing; use IEEE polynomial.
        let crc32 = |b: &[u8]| -> u32 {
            let mut crc: u32 = 0xFFFFFFFF;
            for &byte in b {
                crc ^= byte as u32;
                for _ in 0..8 {
                    let mask = if crc & 1 != 0 { 0xEDB88320u32 } else { 0 };
                    crc = (crc >> 1) ^ mask;
                }
            }
            !crc
        };

        let mut out = Vec::new();
        let mut local_offsets = Vec::with_capacity(entries.len());

        for (name, data) in entries {
            local_offsets.push(out.len() as u32);
            out.extend_from_slice(&[0x50, 0x4b, 0x03, 0x04]); // local header sig
            out.extend_from_slice(&[20, 0]); // version
            out.extend_from_slice(&[0, 0]); // gp flag
            out.extend_from_slice(&[0, 0]); // method = store
            out.extend_from_slice(&[0, 0, 0, 0]); // mtime/date
            out.extend_from_slice(&crc32(data).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed
            out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes()); // extra
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(data);
        }

        let cd_start = out.len() as u32;
        for ((name, data), local_off) in entries.iter().zip(&local_offsets) {
            out.extend_from_slice(&CD_SIG);
            out.extend_from_slice(&[20, 0]); // version made by
            out.extend_from_slice(&[20, 0]); // version needed
            out.extend_from_slice(&[0, 0]); // gp flag
            out.extend_from_slice(&[0, 0]); // method
            out.extend_from_slice(&[0, 0, 0, 0]); // mtime/date
            out.extend_from_slice(&crc32(data).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&local_off.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
        }
        let cd_size = out.len() as u32 - cd_start;

        out.extend_from_slice(&EOCD_SIG);
        out.extend_from_slice(&[0, 0]); // disk number
        out.extend_from_slice(&[0, 0]); // disk with CD
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_start.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment length
        out
    }

    #[test]
    fn identical_zips_share_fingerprint() {
        let z = build_minimal_zip(&[("a.txt", b"hello"), ("b.txt", b"world")]);
        let mut c1 = Cursor::new(z.clone());
        let mut c2 = Cursor::new(z.clone());
        let fp1 = fingerprint(&mut c1, z.len() as u64).unwrap();
        let fp2 = fingerprint(&mut c2, z.len() as u64).unwrap();
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn entry_order_does_not_matter() {
        // Same logical content, different physical entry order.
        let a = build_minimal_zip(&[("x", b"1"), ("y", b"2")]);
        let b = build_minimal_zip(&[("y", b"2"), ("x", b"1")]);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        let fa = fingerprint(&mut ca, a.len() as u64).unwrap();
        let fb = fingerprint(&mut cb, b.len() as u64).unwrap();
        assert_eq!(fa, fb, "sorting CD entries must make order irrelevant");
    }

    #[test]
    fn different_contents_differ() {
        let a = build_minimal_zip(&[("x", b"1"), ("y", b"2")]);
        let b = build_minimal_zip(&[("x", b"1"), ("y", b"3")]);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        let fa = fingerprint(&mut ca, a.len() as u64).unwrap();
        let fb = fingerprint(&mut cb, b.len() as u64).unwrap();
        assert_ne!(fa, fb);
    }
}
