//! Matroska / WebM Tier 0 fingerprint.
//!
//! Matroska files start with an EBML header that identifies the doc
//! type (`matroska` / `webm`), followed by a `Segment` element whose
//! first child is typically `SeekHead`, then `Info`. We fingerprint:
//!
//! * The EBML header bytes (DocType + version)
//! * The `Info` element's `TimecodeScale`, `Duration`, `MuxingApp`,
//!   `WritingApp`, and `SegmentUID` if present.

use std::io::{Read, Seek, SeekFrom};

const EBML_HEADER_ID: u32 = 0x1A45DFA3;
const SEGMENT_ID: u32 = 0x18538067;
const INFO_ID: u32 = 0x1549A966;
const SEGMENT_UID_ID: u32 = 0x73A4;
const TIMECODE_SCALE_ID: u32 = 0x2AD7B1;
const DURATION_ID: u32 = 0x4489;
const MUXING_APP_ID: u32 = 0x4D80;
const WRITING_APP_ID: u32 = 0x5741;

pub fn fingerprint<R: Read + Seek>(r: &mut R, size: u64) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"MKV|");

    // 1) EBML header.
    let (id, len) = read_ebml_header(r)?;
    if id != EBML_HEADER_ID {
        return Err(io_err("not an EBML stream"));
    }
    let ebml_end = position(r)? + len;
    let mut ebml_buf = vec![0u8; len.min(256) as usize];
    r.read_exact(&mut ebml_buf)?;
    hasher.update(b"EBML|");
    hasher.update(&ebml_buf);
    r.seek(SeekFrom::Start(ebml_end))?;

    // 2) Walk the top-level Segment looking for Info.
    while position(r)? < size {
        let (id, payload_len) = match read_ebml_header(r) {
            Ok(v) => v,
            Err(_) => break,
        };
        let payload_start = position(r)?;
        let payload_end = payload_start + payload_len;
        if id == SEGMENT_ID {
            walk_segment(r, payload_end, &mut hasher)?;
            break;
        }
        r.seek(SeekFrom::Start(payload_end))?;
    }
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn walk_segment<R: Read + Seek>(
    r: &mut R,
    end: u64,
    hasher: &mut blake3::Hasher,
) -> std::io::Result<()> {
    while position(r)? + 2 < end {
        let (id, payload_len) = match read_ebml_header(r) {
            Ok(v) => v,
            Err(_) => break,
        };
        let payload_start = position(r)?;
        let payload_end = payload_start + payload_len;
        if payload_end > end {
            break;
        }
        if id == INFO_ID {
            hasher.update(b"Info|");
            walk_info(r, payload_end, hasher)?;
            return Ok(());
        }
        r.seek(SeekFrom::Start(payload_end))?;
    }
    Ok(())
}

fn walk_info<R: Read + Seek>(
    r: &mut R,
    end: u64,
    hasher: &mut blake3::Hasher,
) -> std::io::Result<()> {
    while position(r)? + 2 < end {
        let (id, payload_len) = match read_ebml_header(r) {
            Ok(v) => v,
            Err(_) => break,
        };
        let payload_start = position(r)?;
        let payload_end = payload_start + payload_len;
        if payload_end > end {
            break;
        }
        match id {
            SEGMENT_UID_ID | TIMECODE_SCALE_ID | DURATION_ID | MUXING_APP_ID | WRITING_APP_ID => {
                let n = payload_len.min(256) as usize;
                let mut buf = vec![0u8; n];
                r.read_exact(&mut buf)?;
                hasher.update(&id.to_be_bytes());
                hasher.update(b"|");
                hasher.update(&(payload_len as u32).to_be_bytes());
                hasher.update(&buf);
                let already = payload_start + n as u64;
                if already < payload_end {
                    r.seek(SeekFrom::Start(payload_end))?;
                }
            }
            _ => {
                r.seek(SeekFrom::Start(payload_end))?;
            }
        }
    }
    Ok(())
}

fn position<S: Seek>(s: &mut S) -> std::io::Result<u64> {
    s.seek(SeekFrom::Current(0))
}

/// Read an EBML element header: variable-length element ID followed by
/// variable-length size. Returns `(id, size)`. The ID we return
/// includes its leading marker bit so callers can compare against the
/// hex constants in the spec (e.g. `0x1A45DFA3`).
fn read_ebml_header<R: Read>(r: &mut R) -> std::io::Result<(u32, u64)> {
    let id = read_vint_raw(r)?;
    let (size, _w) = read_vint(r)?;
    Ok((id as u32, size))
}

/// Read a VINT and return its decoded value (leading marker stripped).
fn read_vint<R: Read>(r: &mut R) -> std::io::Result<(u64, usize)> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let b = first[0];
    if b == 0 {
        return Err(io_err("invalid VINT marker"));
    }
    let mut width = 1usize;
    let mut mask = 0x80u8;
    while (b & mask) == 0 {
        width += 1;
        mask >>= 1;
        if width > 8 {
            return Err(io_err("VINT too wide"));
        }
    }
    let mut value = (b & (mask - 1)) as u64;
    for _ in 1..width {
        let mut x = [0u8; 1];
        r.read_exact(&mut x)?;
        value = (value << 8) | x[0] as u64;
    }
    Ok((value, width))
}

/// Read a VINT but keep the leading marker bit in place — used for
/// element IDs which are canonically written with the marker (e.g.
/// `0x1A45DFA3`).
fn read_vint_raw<R: Read>(r: &mut R) -> std::io::Result<u64> {
    let mut first = [0u8; 1];
    r.read_exact(&mut first)?;
    let b = first[0];
    if b == 0 {
        return Err(io_err("invalid VINT marker"));
    }
    let mut width = 1usize;
    let mut mask = 0x80u8;
    while (b & mask) == 0 {
        width += 1;
        mask >>= 1;
        if width > 8 {
            return Err(io_err("VINT too wide"));
        }
    }
    let mut value = b as u64;
    for _ in 1..width {
        let mut x = [0u8; 1];
        r.read_exact(&mut x)?;
        value = (value << 8) | x[0] as u64;
    }
    Ok(value)
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn vint(v: u64, width: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(width);
        let marker = 1u8 << (8 - width);
        let mut v_bytes = vec![0u8; width];
        let mut tmp = v;
        for i in (0..width).rev() {
            v_bytes[i] = (tmp & 0xFF) as u8;
            tmp >>= 8;
        }
        v_bytes[0] |= marker;
        bytes.extend_from_slice(&v_bytes);
        bytes
    }

    fn write_id(id: u32) -> Vec<u8> {
        // Encode the ID using its natural width (the ID already has its
        // marker bit set).
        let mut buf = id.to_be_bytes().to_vec();
        while buf.first() == Some(&0) {
            buf.remove(0);
        }
        buf
    }

    fn elem(id: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(write_id(id));
        out.extend(vint(payload.len() as u64, 8));
        out.extend_from_slice(payload);
        out
    }

    fn make_mkv(duration: f64, muxing_app: &str) -> Vec<u8> {
        let ebml_header = elem(
            EBML_HEADER_ID,
            b"\x42\x82\x88matroska", // DocType "matroska"
        );
        let mut info_body = Vec::new();
        info_body.extend(elem(TIMECODE_SCALE_ID, &1_000_000u64.to_be_bytes()[4..]));
        info_body.extend(elem(DURATION_ID, &duration.to_be_bytes()));
        info_body.extend(elem(MUXING_APP_ID, muxing_app.as_bytes()));
        let info = elem(INFO_ID, &info_body);
        let segment = elem(SEGMENT_ID, &info);

        let mut file = Vec::new();
        file.extend(ebml_header);
        file.extend(segment);
        file
    }

    #[test]
    fn identical_mkvs_match() {
        let a = make_mkv(123.0, "libsuperdupe-tests");
        let mut c1 = Cursor::new(a.clone());
        let mut c2 = Cursor::new(a.clone());
        assert_eq!(
            fingerprint(&mut c1, a.len() as u64).unwrap(),
            fingerprint(&mut c2, a.len() as u64).unwrap()
        );
    }

    #[test]
    fn muxing_app_changes_fingerprint() {
        let a = make_mkv(123.0, "muxer-a");
        let b = make_mkv(123.0, "muxer-b");
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }

    #[test]
    fn duration_changes_fingerprint() {
        let a = make_mkv(100.0, "x");
        let b = make_mkv(200.0, "x");
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }
}
