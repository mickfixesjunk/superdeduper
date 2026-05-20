//! PNG Tier 0 fingerprint.
//!
//! Fingerprint = BLAKE3 over:
//!
//! * the 8-byte PNG signature
//! * the IHDR chunk (image dimensions, bit depth, colour type, etc.)
//! * the first IDAT chunk's *header* (length + type + first 16 bytes)
//! * every `tEXt`, `iTXt`, and `zTXt` chunk's full payload
//!
//! Two truly-identical PNGs share all of these. Re-encoded PNGs that
//! happen to share dimensions but differ in compression byte-stream
//! will still differ in their IDAT-header bytes, which include the
//! deflate-stream prefix.

use std::io::{Read, Seek, SeekFrom};

const PNG_SIG: [u8; 8] = [137, 80, 78, 71, 13, 10, 26, 10];

pub fn fingerprint<R: Read + Seek>(r: &mut R, _size: u64) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(0))?;
    let mut sig = [0u8; 8];
    r.read_exact(&mut sig)?;
    if sig != PNG_SIG {
        return Err(io_err("not a PNG"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"PNG|");
    hasher.update(&sig);

    let mut seen_idat = false;
    loop {
        let mut len_bytes = [0u8; 4];
        if r.read_exact(&mut len_bytes).is_err() {
            break;
        }
        let len = u32::from_be_bytes(len_bytes) as usize;
        let mut typ = [0u8; 4];
        r.read_exact(&mut typ)?;
        let payload_len_limit = len.min(16 * 1024); // hard cap; text chunks are tiny
        let mut peek = vec![0u8; payload_len_limit];
        r.read_exact(&mut peek)?;
        // Drain the rest of the payload + CRC.
        let to_skip = (len - peek.len()) as i64;
        r.seek(SeekFrom::Current(to_skip + 4))?;

        match &typ {
            b"IHDR" => {
                hasher.update(b"IHDR|");
                hasher.update(&(len as u32).to_be_bytes());
                hasher.update(&peek);
            }
            b"IDAT" if !seen_idat => {
                seen_idat = true;
                hasher.update(b"IDAT0|");
                hasher.update(&(len as u32).to_be_bytes());
                hasher.update(&peek[..peek.len().min(16)]);
            }
            b"tEXt" | b"iTXt" | b"zTXt" => {
                hasher.update(&typ);
                hasher.update(b"|");
                hasher.update(&(len as u32).to_be_bytes());
                hasher.update(&peek);
            }
            b"IEND" => break,
            _ => { /* ignored */ }
        }
    }

    Ok(hasher.finalize().as_bytes().to_vec())
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn png_chunk(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v.extend_from_slice(&[0, 0, 0, 0]); // fake CRC; we don't verify
        v
    }

    fn make_png(width: u32, height: u32, txt: &[u8], idat_prefix: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&PNG_SIG);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
        if !txt.is_empty() {
            out.extend_from_slice(&png_chunk(b"tEXt", txt));
        }
        out.extend_from_slice(&png_chunk(b"IDAT", idat_prefix));
        out.extend_from_slice(&png_chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn identical_pngs_match() {
        let p = make_png(640, 480, b"Comment\x00Hello", &[0x78, 0x9c, 0xed]);
        let mut c1 = Cursor::new(p.clone());
        let mut c2 = Cursor::new(p.clone());
        assert_eq!(
            fingerprint(&mut c1, p.len() as u64).unwrap(),
            fingerprint(&mut c2, p.len() as u64).unwrap()
        );
    }

    #[test]
    fn dimensions_change_fingerprint() {
        let a = make_png(640, 480, b"", &[0x78, 0x9c]);
        let b = make_png(640, 481, b"", &[0x78, 0x9c]);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }

    #[test]
    fn text_chunks_matter() {
        let a = make_png(100, 100, b"Title\x00Hello", &[0x78]);
        let b = make_png(100, 100, b"Title\x00World", &[0x78]);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }

    #[test]
    fn idat_prefix_matters() {
        let a = make_png(100, 100, b"", &[0x78, 0x9c, 0x01]);
        let b = make_png(100, 100, b"", &[0x78, 0x9c, 0x02]);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }
}
