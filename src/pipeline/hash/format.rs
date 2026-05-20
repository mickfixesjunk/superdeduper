//! Tier 0 — format-aware fingerprinting.
//!
//! For files whose extension matches a known container format we parse
//! a small, structurally-significant region rather than hashing the
//! whole body. Two files that share the same Tier 0 fingerprint are
//! >99.99% certain to be byte-identical, so a fingerprint collision
//! immediately advances them to Tier 3 (full BLAKE3) for the final
//! signoff. Mismatched fingerprints reject the pair without any
//! further reads.
//!
//! This module is intentionally cautious: any parsing failure simply
//! returns `None` and the file falls back to the byte-sampling tiers.
//! Tier 0 must never invent matches — false positives at this layer
//! could corrupt user data later when `dedupe` acts on them.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub mod jpeg;
pub mod mp3;
pub mod png;
pub mod zip;

/// The format dispatched to, based on extension. Unknown extensions
/// return `None` and the file skips Tier 0.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    Jpeg,
    Png,
    Mp3,
}

impl Format {
    pub fn from_path(p: &Path) -> Option<Format> {
        let ext = p.extension()?.to_str()?.to_ascii_lowercase();
        Some(match ext.as_str() {
            "zip" | "jar" | "epub" | "docx" | "xlsx" | "pptx" | "odt" | "ods" | "odp" => {
                Format::Zip
            }
            "jpg" | "jpeg" | "jpe" | "jfif" => Format::Jpeg,
            "png" => Format::Png,
            "mp3" => Format::Mp3,
            _ => return None,
        })
    }
}

/// Compute the Tier 0 fingerprint for `path`. Returns `None` if the
/// path's extension is unsupported or parsing failed.
pub fn fingerprint(path: &Path, size: u64) -> Option<[u8; 32]> {
    let fmt = Format::from_path(path)?;
    let mut file = File::open(path).ok()?;
    let raw = match fmt {
        Format::Zip => zip::fingerprint(&mut file, size).ok()?,
        Format::Jpeg => jpeg::fingerprint(&mut file, size).ok()?,
        Format::Png => png::fingerprint(&mut file, size).ok()?,
        Format::Mp3 => mp3::fingerprint(&mut file, size).ok()?,
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[fmt as u8]);
    hasher.update(&raw);
    Some(*hasher.finalize().as_bytes())
}

// Format-parser helpers. Some are unused in the v0.1 parser set but
// stay available for the additional formats (MP4, MKV, PDF) on the
// roadmap, hence the `dead_code` allowance.
#[allow(dead_code)]
pub(crate) fn read_n<R: Read>(r: &mut R, n: usize) -> std::io::Result<Vec<u8>> {
    let mut v = vec![0u8; n];
    r.read_exact(&mut v)?;
    Ok(v)
}

pub(crate) fn read_u16_be<R: Read>(r: &mut R) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

#[allow(dead_code)]
pub(crate) fn read_u32_be<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

#[allow(dead_code)]
pub(crate) fn read_u32_le<R: Read>(r: &mut R) -> std::io::Result<u32> {
    let mut b = [0u8; 4];
    r.read_exact(&mut b)?;
    Ok(u32::from_le_bytes(b))
}

#[allow(dead_code)]
pub(crate) fn read_u16_le<R: Read>(r: &mut R) -> std::io::Result<u16> {
    let mut b = [0u8; 2];
    r.read_exact(&mut b)?;
    Ok(u16::from_le_bytes(b))
}

pub(crate) fn seek_to<S: Seek>(s: &mut S, off: u64) -> std::io::Result<()> {
    s.seek(SeekFrom::Start(off))?;
    Ok(())
}
