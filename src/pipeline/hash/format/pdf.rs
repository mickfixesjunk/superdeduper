//! PDF Tier 0 fingerprint.
//!
//! PDFs end with a trailer dictionary preceded by `startxref` and the
//! byte offset of the cross-reference table. We:
//!
//! 1. Read the last 4 KiB of the file.
//! 2. Locate `%%EOF` and the preceding `startxref` line.
//! 3. Hash the trailer dict bytes plus the xref-pointed region.
//!
//! Re-saved PDFs differ in trailer (new `/ID`, new file size) and
//! fingerprint differently.

use std::io::{Read, Seek, SeekFrom};

const TAIL_WINDOW: usize = 4096;

pub fn fingerprint<R: Read + Seek>(r: &mut R, size: u64) -> std::io::Result<Vec<u8>> {
    if size < 16 {
        return Err(io_err("file too small for a PDF"));
    }
    let tail_len = TAIL_WINDOW.min(size as usize);
    let tail_start = size - tail_len as u64;
    r.seek(SeekFrom::Start(tail_start))?;
    let mut tail = vec![0u8; tail_len];
    r.read_exact(&mut tail)?;

    let eof = find_last(&tail, b"%%EOF").ok_or_else(|| io_err("no %%EOF"))?;
    let trailer_region = &tail[..eof + b"%%EOF".len()];

    let startxref_off =
        find_last(trailer_region, b"startxref").ok_or_else(|| io_err("no startxref"))?;
    // Parse the integer after `startxref` to learn where the xref
    // table lives in the file.
    let after = &trailer_region[startxref_off + b"startxref".len()..];
    let xref_off: u64 = parse_int_after_ws(after).ok_or_else(|| io_err("bad startxref value"))?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(b"PDF|");
    hasher.update(trailer_region);

    if xref_off < size {
        r.seek(SeekFrom::Start(xref_off))?;
        let to_read = (size - xref_off).min(64 * 1024) as usize;
        let mut xref = vec![0u8; to_read];
        if r.read_exact(&mut xref).is_ok() {
            hasher.update(b"XREF|");
            hasher.update(&xref);
        }
    }
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn find_last(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let mut i = haystack.len() - needle.len();
    loop {
        if &haystack[i..i + needle.len()] == needle {
            return Some(i);
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

fn parse_int_after_ws(buf: &[u8]) -> Option<u64> {
    let mut i = 0;
    while i < buf.len() && matches!(buf[i], b' ' | b'\t' | b'\r' | b'\n') {
        i += 1;
    }
    let start = i;
    while i < buf.len() && buf[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    std::str::from_utf8(&buf[start..i]).ok()?.parse().ok()
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn make_pdf(info_value: &str) -> Vec<u8> {
        let body = format!(
            "%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n\
             2 0 obj\n<< /Info {} >>\nendobj\n",
            info_value
        );
        let xref_off = body.len();
        let mut out = body.into_bytes();
        out.extend_from_slice(
            b"xref\n0 3\n0000000000 65535 f \n0000000010 00000 n \n0000000055 00000 n \n",
        );
        out.extend_from_slice(b"trailer\n<< /Size 3 /Root 1 0 R >>\nstartxref\n");
        out.extend_from_slice(xref_off.to_string().as_bytes());
        out.extend_from_slice(b"\n%%EOF\n");
        out
    }

    #[test]
    fn identical_pdfs_match() {
        let a = make_pdf("(hello)");
        let mut c1 = Cursor::new(a.clone());
        let mut c2 = Cursor::new(a.clone());
        assert_eq!(
            fingerprint(&mut c1, a.len() as u64).unwrap(),
            fingerprint(&mut c2, a.len() as u64).unwrap()
        );
    }

    #[test]
    fn different_info_dict_differs() {
        let a = make_pdf("(one)");
        let b = make_pdf("(two)");
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }

    #[test]
    fn malformed_returns_err() {
        let mut c = Cursor::new(b"not a pdf".to_vec());
        assert!(fingerprint(&mut c, 9).is_err());
    }
}
