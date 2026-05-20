//! MP3 Tier 0 fingerprint.
//!
//! Fingerprint = BLAKE3 over:
//!
//! * the ID3v2 header (if present) plus its full tag payload
//! * the first MPEG audio frame header (4 bytes)
//! * the last MPEG audio frame header (4 bytes) by scanning back from
//!   the (optional) ID3v1 footer
//! * the total number of MPEG audio frames seen
//!
//! Pure audio re-encodes will differ in the frame headers and frame
//! count; bit-identical copies share all four.

use std::io::{Read, Seek, SeekFrom};

pub fn fingerprint<R: Read + Seek>(r: &mut R, size: u64) -> std::io::Result<Vec<u8>> {
    r.seek(SeekFrom::Start(0))?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"MP3|");

    let mut audio_start = 0u64;
    let mut header_bytes = [0u8; 10];
    if r.read_exact(&mut header_bytes).is_ok() && &header_bytes[..3] == b"ID3" {
        let size_syncsafe = ((header_bytes[6] as u32 & 0x7F) << 21)
            | ((header_bytes[7] as u32 & 0x7F) << 14)
            | ((header_bytes[8] as u32 & 0x7F) << 7)
            | (header_bytes[9] as u32 & 0x7F);
        let tag_size = size_syncsafe as u64;
        audio_start = 10 + tag_size;
        hasher.update(b"ID3v2|");
        hasher.update(&header_bytes);

        // Read the rest of the tag (capped to avoid huge allocations).
        let cap = tag_size.min(64 * 1024) as usize;
        let mut tag = vec![0u8; cap];
        r.read_exact(&mut tag)?;
        hasher.update(&tag);
    }

    // ID3v1 footer? If the file ends with "TAG", chop 128 bytes off the
    // audio region we consider.
    let mut audio_end = size;
    if size >= 128 {
        r.seek(SeekFrom::Start(size - 128))?;
        let mut footer = [0u8; 3];
        if r.read_exact(&mut footer).is_ok() && &footer == b"TAG" {
            audio_end = size - 128;
        }
    }
    if audio_end <= audio_start {
        return Err(io_err("no audio frames"));
    }

    // First MPEG audio frame header. Scan up to 4 KiB looking for an
    // MPEG sync (0xFFE).
    r.seek(SeekFrom::Start(audio_start))?;
    let scan_len = (audio_end - audio_start).min(4096) as usize;
    let mut scan = vec![0u8; scan_len];
    r.read_exact(&mut scan)?;
    let first_off = find_sync(&scan).ok_or_else(|| io_err("no MPEG sync"))?;
    let first_header_abs = audio_start + first_off as u64;
    if first_header_abs + 4 > audio_end {
        return Err(io_err("first frame truncated"));
    }
    let first_header: [u8; 4] = scan[first_off..first_off + 4]
        .try_into()
        .map_err(|_| io_err("frame header"))?;
    hasher.update(b"AUDIO0|");
    hasher.update(&first_header);

    // Walk frames to count them and find the last header.
    let frame_count_cap = 200_000usize;
    let (count, last_header) =
        walk_frames(r, first_header_abs, audio_end, frame_count_cap)?;
    hasher.update(b"COUNT|");
    hasher.update(&(count as u32).to_be_bytes());
    if let Some(h) = last_header {
        hasher.update(b"AUDIOZ|");
        hasher.update(&h);
    }
    Ok(hasher.finalize().as_bytes().to_vec())
}

fn find_sync(buf: &[u8]) -> Option<usize> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == 0xFF && (buf[i + 1] & 0xE0) == 0xE0 {
            return Some(i);
        }
    }
    None
}

fn walk_frames<R: Read + Seek>(
    r: &mut R,
    first_off: u64,
    end: u64,
    cap: usize,
) -> std::io::Result<(usize, Option<[u8; 4]>)> {
    let mut count = 0usize;
    let mut last: Option<[u8; 4]> = None;
    let mut off = first_off;
    while off + 4 <= end && count < cap {
        r.seek(SeekFrom::Start(off))?;
        let mut hdr = [0u8; 4];
        if r.read_exact(&mut hdr).is_err() {
            break;
        }
        if hdr[0] != 0xFF || (hdr[1] & 0xE0) != 0xE0 {
            // Lost sync: search forward up to 1 KiB for a resync.
            let scan_len = ((end - off).min(1024)) as usize;
            let mut scan = vec![0u8; scan_len];
            r.seek(SeekFrom::Start(off))?;
            r.read_exact(&mut scan)?;
            match find_sync(&scan) {
                Some(s) => {
                    off += s as u64;
                    continue;
                }
                None => break,
            }
        }
        last = Some(hdr);
        count += 1;
        let frame_len = frame_length(&hdr);
        if frame_len < 4 {
            break;
        }
        off += frame_len as u64;
    }
    Ok((count, last))
}

/// Compute the byte length of an MPEG audio frame from its 4-byte
/// header. Implements MPEG-1 Layer III for the bit-rate table; other
/// layers and MPEG-2 fall back to a conservative estimate so we still
/// progress through the file without infinite-looping.
fn frame_length(hdr: &[u8; 4]) -> u32 {
    let version_id = (hdr[1] >> 3) & 0x03; // 11 = MPEG1
    let layer = (hdr[1] >> 1) & 0x03; // 01 = Layer III
    let bitrate_idx = (hdr[2] >> 4) & 0x0F;
    let sample_idx = (hdr[2] >> 2) & 0x03;
    let padding = ((hdr[2] >> 1) & 0x01) as u32;

    let bitrate = match (version_id, layer, bitrate_idx) {
        // MPEG-1 Layer III bit rates in kbps.
        (0b11, 0b01, 1) => 32,
        (0b11, 0b01, 2) => 40,
        (0b11, 0b01, 3) => 48,
        (0b11, 0b01, 4) => 56,
        (0b11, 0b01, 5) => 64,
        (0b11, 0b01, 6) => 80,
        (0b11, 0b01, 7) => 96,
        (0b11, 0b01, 8) => 112,
        (0b11, 0b01, 9) => 128,
        (0b11, 0b01, 10) => 160,
        (0b11, 0b01, 11) => 192,
        (0b11, 0b01, 12) => 224,
        (0b11, 0b01, 13) => 256,
        (0b11, 0b01, 14) => 320,
        _ => return 144, // conservative fallback
    } as u32;

    let sample_rate = match (version_id, sample_idx) {
        (0b11, 0) => 44_100,
        (0b11, 1) => 48_000,
        (0b11, 2) => 32_000,
        _ => return 144,
    };

    144 * (bitrate * 1000) / sample_rate + padding
}

fn io_err(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn frame_header_mpeg1_l3_128() -> [u8; 4] {
        // sync(11) + ver=11 + layer=01 + protection=1 + bitrate=1001 (128 kbps)
        // + samplerate=00 (44.1 kHz) + pad=0 + private=0 + channel=00 (stereo)
        // + mode_ext=00 + copy=0 + orig=0 + emphasis=00
        [0xFF, 0xFB, 0x90, 0x00]
    }

    fn make_mp3(num_frames: usize, with_id3v2: bool) -> Vec<u8> {
        let mut v = Vec::new();
        if with_id3v2 {
            v.extend_from_slice(b"ID3\x03\x00\x00\x00\x00\x00\x10");
            v.extend_from_slice(&[0u8; 16]);
        }
        let hdr = frame_header_mpeg1_l3_128();
        let frame_len = frame_length(&hdr) as usize;
        for _ in 0..num_frames {
            v.extend_from_slice(&hdr);
            v.extend_from_slice(&vec![0u8; frame_len - 4]);
        }
        v
    }

    #[test]
    fn identical_mp3s_match() {
        let a = make_mp3(50, true);
        let mut c1 = Cursor::new(a.clone());
        let mut c2 = Cursor::new(a.clone());
        assert_eq!(
            fingerprint(&mut c1, a.len() as u64).unwrap(),
            fingerprint(&mut c2, a.len() as u64).unwrap()
        );
    }

    #[test]
    fn frame_count_distinguishes() {
        let a = make_mp3(10, false);
        let b = make_mp3(11, false);
        let mut ca = Cursor::new(a.clone());
        let mut cb = Cursor::new(b.clone());
        assert_ne!(
            fingerprint(&mut ca, a.len() as u64).unwrap(),
            fingerprint(&mut cb, b.len() as u64).unwrap()
        );
    }
}
