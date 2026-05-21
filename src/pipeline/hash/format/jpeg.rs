//! JPEG Tier 0 fingerprint.
//!
//! Reads the SOI + every APPn marker up to (but not including) the
//! first SOS (start of scan). That covers JFIF, EXIF, ICC, XMP, etc.
//! Then locates SOF0/SOF2 to extract the image dimensions. Fingerprint
//! = BLAKE3 over the concatenated APPn data + dimensions + (if
//! present) EXIF `DateTimeOriginal`.
//!
//! This is robust to differences in raw entropy-coded image data only
//! if the metadata is identical — which is the case for files that
//! truly are copies. Re-encoded or re-saved JPEGs differ in metadata
//! and rightly fingerprint differently.

use std::io::{Read, Seek};

use super::{read_u16_be, read_u32_be, seek_to};

const SOI: u16 = 0xFFD8;
const EOI: u16 = 0xFFD9;
const SOS: u16 = 0xFFDA;
const APP0: u16 = 0xFFE0;
const APP15: u16 = 0xFFEF;
const COM: u16 = 0xFFFE;

pub fn fingerprint<R: Read + Seek>(r: &mut R, _size: u64) -> std::io::Result<Vec<u8>> {
    seek_to(r, 0)?;
    let sig = read_u16_be(r)?;
    if sig != SOI {
        return Err(io_err("not a JPEG"));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"JPEG|");

    let mut datetime_original: Option<Vec<u8>> = None;

    while let Ok(marker) = read_u16_be(r) {
        if marker == SOS || marker == EOI {
            break;
        }
        // Stand-alone markers without a payload: 0xFFD0..=0xFFD7 (RSTn)
        // and 0xFF01. Everything else has a 2-byte length following.
        if matches!(marker, 0xFF01 | 0xFFD0..=0xFFD7) {
            continue;
        }
        let len = read_u16_be(r)? as u64;
        if len < 2 {
            return Err(io_err("bad segment length"));
        }
        let payload_len = (len - 2) as usize;
        let mut payload = vec![0u8; payload_len];
        r.read_exact(&mut payload)?;

        // APP segments + COM contribute to the fingerprint.
        if (APP0..=APP15).contains(&marker) || marker == COM {
            hasher.update(&marker.to_be_bytes());
            hasher.update(&(payload_len as u32).to_be_bytes());
            hasher.update(&payload);

            // Sniff EXIF for DateTimeOriginal.
            if marker == 0xFFE1 && payload.starts_with(b"Exif\0\0") {
                datetime_original = scan_exif_datetime(&payload[6..]);
            }
        }
        // SOFn: 0xFFC0..=0xFFCF except 0xFFC4 (DHT) and 0xFFCC (DAC).
        if (0xFFC0..=0xFFCF).contains(&marker)
            && marker != 0xFFC4
            && marker != 0xFFCC
            && payload_len >= 5
        {
            let precision = payload[0];
            let height = u16::from_be_bytes([payload[1], payload[2]]);
            let width = u16::from_be_bytes([payload[3], payload[4]]);
            hasher.update(b"SOF|");
            hasher.update(&[precision]);
            hasher.update(&height.to_be_bytes());
            hasher.update(&width.to_be_bytes());
        }
    }

    if let Some(dt) = datetime_original {
        hasher.update(b"DTO|");
        hasher.update(&dt);
    }

    Ok(hasher.finalize().as_bytes().to_vec())
}

/// Locate the EXIF `DateTimeOriginal` (TIFF tag 0x9003) in the TIFF
/// payload of an APP1 EXIF segment. Returns the raw bytes or `None`
/// if the tag isn't present.
fn scan_exif_datetime(tiff: &[u8]) -> Option<Vec<u8>> {
    if tiff.len() < 8 {
        return None;
    }
    let little_endian = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read_u16 = |bs: &[u8]| -> Option<u16> {
        if bs.len() < 2 {
            return None;
        }
        Some(if little_endian {
            u16::from_le_bytes([bs[0], bs[1]])
        } else {
            u16::from_be_bytes([bs[0], bs[1]])
        })
    };
    let read_u32 = |bs: &[u8]| -> Option<u32> {
        if bs.len() < 4 {
            return None;
        }
        Some(if little_endian {
            u32::from_le_bytes([bs[0], bs[1], bs[2], bs[3]])
        } else {
            u32::from_be_bytes([bs[0], bs[1], bs[2], bs[3]])
        })
    };

    if read_u16(&tiff[2..4])? != 42 {
        return None;
    }
    let ifd0_off = read_u32(&tiff[4..8])? as usize;
    if ifd0_off + 2 > tiff.len() {
        return None;
    }
    let n = read_u16(&tiff[ifd0_off..])? as usize;

    let mut exif_ifd: Option<usize> = None;
    for i in 0..n {
        let off = ifd0_off + 2 + i * 12;
        if off + 12 > tiff.len() {
            return None;
        }
        let tag = read_u16(&tiff[off..])?;
        if tag == 0x8769 {
            // ExifOffset sub-IFD pointer
            exif_ifd = Some(read_u32(&tiff[off + 8..])? as usize);
            break;
        }
    }
    let exif_ifd = exif_ifd?;
    if exif_ifd + 2 > tiff.len() {
        return None;
    }
    let m = read_u16(&tiff[exif_ifd..])? as usize;
    for i in 0..m {
        let off = exif_ifd + 2 + i * 12;
        if off + 12 > tiff.len() {
            return None;
        }
        let tag = read_u16(&tiff[off..])?;
        if tag == 0x9003 {
            // DateTimeOriginal — ASCII, count includes trailing NUL.
            let count = read_u32(&tiff[off + 4..])? as usize;
            if count == 0 {
                return Some(Vec::new());
            }
            if count <= 4 {
                return Some(tiff[off + 8..off + 8 + count].to_vec());
            }
            let val_off = read_u32(&tiff[off + 8..])? as usize;
            if val_off + count > tiff.len() {
                return None;
            }
            return Some(tiff[val_off..val_off + count].to_vec());
        }
    }
    None
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

// Silence unused-fn warnings on read_u32_be — kept available for the
// SOFn precision extraction in a future expansion.
#[allow(dead_code)]
fn _keep_alive<R: Read>(r: &mut R) -> std::io::Result<u32> {
    read_u32_be(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn jpeg_with(metadata: &[u8], width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&[0xFF, 0xD8]); // SOI
                                              // APP0: JFIF
        out.extend_from_slice(&[0xFF, 0xE0]);
        let payload: Vec<u8> = b"JFIF\0\x01\x02\x00\x00\x48\x00\x48\x00\x00".to_vec();
        let len = (payload.len() + 2) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&payload);
        // COM segment: caller's metadata
        if !metadata.is_empty() {
            out.extend_from_slice(&[0xFF, 0xFE]);
            let len = (metadata.len() + 2) as u16;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(metadata);
        }
        // SOF0
        out.extend_from_slice(&[0xFF, 0xC0]);
        let sof: Vec<u8> = {
            let mut v = Vec::new();
            v.push(8); // precision
            v.extend_from_slice(&height.to_be_bytes());
            v.extend_from_slice(&width.to_be_bytes());
            v.push(3); // components
            v.extend_from_slice(&[1, 0x22, 0, 2, 0x11, 1, 3, 0x11, 1]);
            v
        };
        let len = (sof.len() + 2) as u16;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&sof);
        // SOS — fingerprint stops here.
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    #[test]
    fn identical_jpegs_match() {
        let j = jpeg_with(b"shot1", 100, 200);
        let mut c1 = Cursor::new(j.clone());
        let mut c2 = Cursor::new(j.clone());
        assert_eq!(
            fingerprint(&mut c1, 0).unwrap(),
            fingerprint(&mut c2, 0).unwrap()
        );
    }

    #[test]
    fn metadata_changes_fingerprint() {
        let a = jpeg_with(b"shot1", 100, 200);
        let b = jpeg_with(b"shot2", 100, 200);
        let mut ca = Cursor::new(a);
        let mut cb = Cursor::new(b);
        assert_ne!(
            fingerprint(&mut ca, 0).unwrap(),
            fingerprint(&mut cb, 0).unwrap()
        );
    }

    #[test]
    fn dimensions_change_fingerprint() {
        let a = jpeg_with(b"x", 100, 200);
        let b = jpeg_with(b"x", 100, 300);
        let mut ca = Cursor::new(a);
        let mut cb = Cursor::new(b);
        assert_ne!(
            fingerprint(&mut ca, 0).unwrap(),
            fingerprint(&mut cb, 0).unwrap()
        );
    }

    #[test]
    fn non_jpeg_rejected() {
        let mut c = Cursor::new(vec![0x00, 0x01, 0x02]);
        assert!(fingerprint(&mut c, 3).is_err());
    }
}
