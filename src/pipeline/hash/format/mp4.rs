//! MP4 / MOV / M4A / M4V Tier 0 fingerprint.
//!
//! ISO Base Media File Format containers. We walk the top-level atom
//! list, descend into `moov`, and fingerprint:
//!
//! * `mvhd` — duration + creation time + modification time + timescale
//! * every `trak/tkhd` — track id + duration
//! * a count of all `stsz` entries we can reach (sum-of-sample-sizes
//!   proxy)
//!
//! Files that share every byte of the above are essentially certain
//! to be duplicates; re-muxes or transcodes diverge in mvhd timestamps
//! or sample-size totals and fingerprint differently.

use std::io::{Read, Seek, SeekFrom};

pub fn fingerprint<R: Read + Seek>(r: &mut R, size: u64) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"MP4|");
    walk_atoms(r, size, 0, &mut hasher, 0)?;
    Ok(hasher.finalize().as_bytes().to_vec())
}

const MAX_DEPTH: u32 = 6;
const MAX_ATOMS_PER_LEVEL: u32 = 1024;

fn walk_atoms<R: Read + Seek>(
    r: &mut R,
    end: u64,
    base: u64,
    hasher: &mut blake3::Hasher,
    depth: u32,
) -> std::io::Result<()> {
    if depth > MAX_DEPTH {
        return Ok(());
    }
    let mut pos = base;
    let mut atom_count = 0u32;
    while pos + 8 <= end && atom_count < MAX_ATOMS_PER_LEVEL {
        atom_count += 1;
        r.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        if r.read_exact(&mut header).is_err() {
            break;
        }
        let size32 = u32::from_be_bytes([header[0], header[1], header[2], header[3]]);
        let atom_type = [header[4], header[5], header[6], header[7]];
        let (atom_size, payload_off) = match size32 {
            0 => (end - pos, pos + 8),
            1 => {
                // 64-bit largesize follows.
                let mut large = [0u8; 8];
                r.read_exact(&mut large)?;
                (u64::from_be_bytes(large), pos + 16)
            }
            n => (n as u64, pos + 8),
        };
        if atom_size < 8 || pos + atom_size > end {
            break;
        }
        let payload_end = pos + atom_size;

        match &atom_type {
            b"moov" | b"trak" | b"mdia" | b"minf" | b"stbl" | b"udta" => {
                hasher.update(b"BOX|");
                hasher.update(&atom_type);
                walk_atoms(r, payload_end, payload_off, hasher, depth + 1)?;
            }
            b"mvhd" => fingerprint_mvhd(r, payload_off, payload_end, hasher)?,
            b"tkhd" => fingerprint_tkhd(r, payload_off, payload_end, hasher)?,
            b"stsz" => fingerprint_stsz(r, payload_off, payload_end, hasher)?,
            _ => { /* skip non-fingerprint atoms */ }
        }
        pos = payload_end;
    }
    Ok(())
}

fn fingerprint_mvhd<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    hasher: &mut blake3::Hasher,
) -> std::io::Result<()> {
    r.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start).min(120) as usize];
    r.read_exact(&mut buf)?;
    if buf.len() < 4 {
        return Ok(());
    }
    let version = buf[0];
    hasher.update(b"mvhd|");
    hasher.update(&[version]);
    // Pull (creation, modification, timescale, duration) per version.
    if version == 1 && buf.len() >= 4 + 8 + 8 + 4 + 8 {
        hasher.update(&buf[4..4 + 8]); // creation
        hasher.update(&buf[12..12 + 8]); // modification
        hasher.update(&buf[20..20 + 4]); // timescale
        hasher.update(&buf[24..24 + 8]); // duration
    } else if buf.len() >= 4 + 4 + 4 + 4 + 4 {
        hasher.update(&buf[4..4 + 4]);
        hasher.update(&buf[8..8 + 4]);
        hasher.update(&buf[12..12 + 4]);
        hasher.update(&buf[16..16 + 4]);
    }
    Ok(())
}

fn fingerprint_tkhd<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    hasher: &mut blake3::Hasher,
) -> std::io::Result<()> {
    r.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; (end - start).min(96) as usize];
    r.read_exact(&mut buf)?;
    if buf.len() < 4 {
        return Ok(());
    }
    let version = buf[0];
    hasher.update(b"tkhd|");
    hasher.update(&[version]);
    if version == 1 && buf.len() >= 4 + 8 + 8 + 4 + 4 + 8 {
        hasher.update(&buf[4..4 + 8]); // creation
        hasher.update(&buf[12..12 + 8]); // modification
        hasher.update(&buf[20..20 + 4]); // track id
        hasher.update(&buf[28..28 + 8]); // duration (skipping reserved)
    } else if buf.len() >= 4 + 4 + 4 + 4 + 4 {
        hasher.update(&buf[4..4 + 4]);
        hasher.update(&buf[8..8 + 4]);
        hasher.update(&buf[12..12 + 4]);
        hasher.update(&buf[20..20 + 4]);
    }
    Ok(())
}

fn fingerprint_stsz<R: Read + Seek>(
    r: &mut R,
    start: u64,
    end: u64,
    hasher: &mut blake3::Hasher,
) -> std::io::Result<()> {
    // stsz format: version(1) + flags(3) + sample_size(4) + count(4)
    // + table[count]*4. We hash version + sample_size + count, plus
    // the table's sum.
    r.seek(SeekFrom::Start(start))?;
    let mut head = [0u8; 12];
    if r.read_exact(&mut head).is_err() {
        return Ok(());
    }
    let sample_size = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
    let count = u32::from_be_bytes([head[8], head[9], head[10], head[11]]);
    hasher.update(b"stsz|");
    hasher.update(&head[4..12]);
    if sample_size == 0 && count > 0 {
        let table_bytes = (count as u64) * 4;
        let table_end = start + 12 + table_bytes;
        if table_end <= end {
            // Stream the table and accumulate the sum without buffering
            // all of it.
            let mut remaining = table_bytes;
            let mut sum: u64 = 0;
            let mut buf = [0u8; 4096];
            while remaining > 0 {
                let chunk = remaining.min(buf.len() as u64) as usize;
                r.read_exact(&mut buf[..chunk])?;
                for window in buf[..chunk].chunks_exact(4) {
                    sum = sum.wrapping_add(u32::from_be_bytes(window.try_into().unwrap()) as u64);
                }
                remaining -= chunk as u64;
            }
            hasher.update(b"sum|");
            hasher.update(&sum.to_be_bytes());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn atom(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut v = Vec::new();
        v.extend_from_slice(&size.to_be_bytes());
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    fn make_mp4(mvhd_duration: u32, track_id: u32) -> Vec<u8> {
        // mvhd v0: version(1) + flags(3) + creation(4) + modification(4)
        // + timescale(4) + duration(4) + remaining = 100 bytes total.
        let mut mvhd_payload = Vec::new();
        mvhd_payload.extend_from_slice(&[0u8, 0, 0, 0]); // version + flags
        mvhd_payload.extend_from_slice(&123_456u32.to_be_bytes());
        mvhd_payload.extend_from_slice(&234_567u32.to_be_bytes());
        mvhd_payload.extend_from_slice(&1000u32.to_be_bytes()); // timescale
        mvhd_payload.extend_from_slice(&mvhd_duration.to_be_bytes());
        while mvhd_payload.len() < 100 {
            mvhd_payload.push(0);
        }
        // tkhd v0
        let mut tkhd_payload = Vec::new();
        tkhd_payload.extend_from_slice(&[0u8, 0, 0, 1]); // version + flags
        tkhd_payload.extend_from_slice(&123_456u32.to_be_bytes());
        tkhd_payload.extend_from_slice(&234_567u32.to_be_bytes());
        tkhd_payload.extend_from_slice(&track_id.to_be_bytes());
        tkhd_payload.extend_from_slice(&0u32.to_be_bytes()); // reserved
        tkhd_payload.extend_from_slice(&1000u32.to_be_bytes()); // duration
        while tkhd_payload.len() < 84 {
            tkhd_payload.push(0);
        }
        let trak = atom(b"trak", &atom(b"tkhd", &tkhd_payload));
        let mut moov_body = Vec::new();
        moov_body.extend(atom(b"mvhd", &mvhd_payload));
        moov_body.extend(trak);
        let moov = atom(b"moov", &moov_body);

        let mut file = Vec::new();
        file.extend(atom(b"ftyp", b"isom\x00\x00\x02\x00mp41mp42isom"));
        file.extend(moov);
        // throw in some unused mdat to look realistic
        let mdat = atom(b"mdat", &vec![0u8; 32]);
        file.extend(mdat);
        file
    }

    #[test]
    fn identical_mp4s_match() {
        let a = make_mp4(60_000, 1);
        let mut c1 = Cursor::new(a.clone());
        let mut c2 = Cursor::new(a.clone());
        assert_eq!(
            fingerprint(&mut c1, a.len() as u64).unwrap(),
            fingerprint(&mut c2, a.len() as u64).unwrap()
        );
    }

    #[test]
    fn duration_changes_fingerprint() {
        let a = make_mp4(60_000, 1);
        let b = make_mp4(60_001, 1);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }

    #[test]
    fn track_id_changes_fingerprint() {
        let a = make_mp4(60_000, 1);
        let b = make_mp4(60_000, 7);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }
}
