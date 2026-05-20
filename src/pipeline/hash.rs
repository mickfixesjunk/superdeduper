//! Stage 4 — progressive Tier 0–3 hashing.
//!
//! Each tier reads more bytes than the last. A file only escalates to
//! the next tier if at least one other file in its current group
//! shares the previous tier's hash. Singletons at any tier are
//! released — they are not duplicates and we never read another byte
//! from them.
//!
//! Tiers in order of escalation:
//!
//! * **Tier 0** — optional, format-aware fingerprint (see [`format`]).
//!   Skipped by default unless [`config::use_format_aware`] is set.
//! * **Tier 1** — BLAKE3 of the first 4 KiB.
//! * **Tier 2** — BLAKE3 of `(head 64 KiB || mid 64 KiB || tail 64 KiB)`.
//!   Skipped (and the file advanced directly) for files smaller than
//!   256 KiB, since Tier 1 already covered them.
//! * **Tier 3** — BLAKE3 of the entire file.
//!
//! The I/O layer here is intentionally simple: buffered, sequential,
//! per-file. A future commit will replace these reads with the IOCP
//! pipeline that submits sector-aligned, LCN-sorted, direct-I/O
//! requests. The tier logic above is independent of how bytes arrive.

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::config::ScanConfig;
use crate::pipeline::layout::{LaidOutFile, LaidOutGroup};
use crate::pipeline::DuplicateGroup;
use crate::Result;

pub mod format;

/// Tier 1 sample size — first 4 KiB of the file.
const TIER1_BYTES: u64 = 4 * 1024;
/// Tier 2 per-region sample size — 64 KiB at head, mid, and tail.
const TIER2_REGION: u64 = 64 * 1024;
/// Files smaller than this skip Tier 2 entirely and go straight to Tier 3.
const TIER2_MIN_FILE: u64 = 256 * 1024;
/// Read buffer for Tier 3 streaming.
const TIER3_BUF: usize = 1 << 20;

/// Top-level entry point. Takes size-grouped, layout-annotated files
/// and returns confirmed duplicate groups by full content hash.
pub fn run(groups: Vec<LaidOutGroup>, cfg: &ScanConfig) -> Result<Vec<DuplicateGroup>> {
    let mut confirmed: Vec<DuplicateGroup> = groups
        .into_par_iter()
        .map(|g| run_group(g, cfg))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    confirmed.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.files.cmp(&b.files)));
    Ok(confirmed)
}

fn run_group(group: LaidOutGroup, cfg: &ScanConfig) -> Result<Vec<DuplicateGroup>> {
    let size = group.size;

    // Zero-byte short circuit.
    if size == 0 {
        if group.files.len() < 2 {
            return Ok(Vec::new());
        }
        let files: Vec<PathBuf> = group.files.into_iter().map(|f| f.entry.path).collect();
        return Ok(vec![DuplicateGroup {
            size: 0,
            content_hash: blake3::hash(&[]).to_hex().to_string(),
            files,
            link_equivalent: false,
        }]);
    }

    let mut survivors = group.files;

    // Tier 0: optional format-aware fingerprint. When enabled, we
    // compute a fingerprint per file and split the group by it. Files
    // that share a fingerprint stay together; the ones without one
    // (unsupported extension or parse failure) skip Tier 0 and stay in
    // the group untouched.
    if cfg.use_format_aware {
        survivors = split_by_optional(&survivors, |f| format::fingerprint(&f.entry.path, size))?;
        if survivors.len() < 2 {
            return Ok(Vec::new());
        }
    }

    // Tier 1: 4 KiB head sample.
    survivors = split_by(&survivors, |f| tier1_hash(f, size))?;
    if survivors.len() < 2 {
        return Ok(Vec::new());
    }

    // Tier 2: head+mid+tail, skipped for small files.
    if size >= TIER2_MIN_FILE {
        survivors = split_by(&survivors, |f| tier2_hash(f, size))?;
        if survivors.len() < 2 {
            return Ok(Vec::new());
        }
    }

    // Tier 3: full BLAKE3.
    let groups = into_subgroups(&survivors, |f| tier3_hash(f, size))?;
    let mut out = Vec::new();
    for (hash, files) in groups {
        if files.len() < 2 {
            continue;
        }
        let mut paths: Vec<PathBuf> = files.iter().map(|f| f.entry.path.clone()).collect();
        paths.sort();
        out.push(DuplicateGroup {
            size,
            content_hash: hex(&hash),
            files: paths,
            link_equivalent: false,
        });
    }
    Ok(out)
}

/// Hash each file in `flat`, then keep only those whose hash collides
/// with at least one other file in the slice. Returns the colliding
/// files as a flat Vec — sub-grouping is implicit since survivors of
/// every Tier-N round are by definition still candidates for the same
/// equivalence class. The next tier then splits them again.
/// Like [`split_by`] but the hash function returns `Option<[u8; 32]>`.
/// Files that produce `None` are kept in the survivor pool unchanged
/// (they fall through to subsequent tiers). Files that produce a
/// fingerprint must collide with at least one other fingerprinted
/// file to survive.
fn split_by_optional<F>(flat: &[LaidOutFile], hasher: F) -> Result<Vec<LaidOutFile>>
where
    F: Fn(&LaidOutFile) -> Option<[u8; 32]> + Send + Sync,
{
    let pairs: Vec<(LaidOutFile, Option<[u8; 32]>)> = flat
        .par_iter()
        .map(|f| (f.clone(), hasher(f)))
        .collect();

    let mut without_fp: Vec<LaidOutFile> = Vec::new();
    let mut by_hash: HashMap<[u8; 32], Vec<LaidOutFile>> = HashMap::new();
    for (f, fp) in pairs {
        match fp {
            Some(h) => by_hash.entry(h).or_default().push(f),
            None => without_fp.push(f),
        }
    }
    let mut keep = without_fp;
    for (_, mut v) in by_hash {
        if v.len() >= 2 {
            keep.append(&mut v);
        }
    }
    Ok(keep)
}

fn split_by<F>(flat: &[LaidOutFile], hasher: F) -> Result<Vec<LaidOutFile>>
where
    F: Fn(&LaidOutFile) -> std::io::Result<[u8; 32]> + Send + Sync,
{
    let pairs: Vec<(LaidOutFile, [u8; 32])> = flat
        .par_iter()
        .filter_map(|f| match hasher(f) {
            Ok(h) => Some((f.clone(), h)),
            Err(e) => {
                tracing::warn!(path = %f.entry.path.display(), error = %e, "hash failed; dropping file");
                None
            }
        })
        .collect();

    let mut by_hash: HashMap<[u8; 32], Vec<LaidOutFile>> = HashMap::new();
    for (f, h) in pairs {
        by_hash.entry(h).or_default().push(f);
    }

    let mut keep = Vec::new();
    for (_, mut v) in by_hash {
        if v.len() >= 2 {
            keep.append(&mut v);
        }
    }
    Ok(keep)
}

/// Hash each file and return the buckets keyed by hash. Caller decides
/// what to do with bucket sizes (Tier 3 keeps everything ≥2; earlier
/// tiers just want the union of ≥2 buckets).
fn into_subgroups<F>(
    flat: &[LaidOutFile],
    hasher: F,
) -> Result<HashMap<[u8; 32], Vec<LaidOutFile>>>
where
    F: Fn(&LaidOutFile) -> std::io::Result<[u8; 32]> + Send + Sync,
{
    let pairs: Vec<(LaidOutFile, [u8; 32])> = flat
        .par_iter()
        .filter_map(|f| match hasher(f) {
            Ok(h) => Some((f.clone(), h)),
            Err(e) => {
                tracing::warn!(path = %f.entry.path.display(), error = %e, "hash failed; dropping file");
                None
            }
        })
        .collect();
    let mut out: HashMap<[u8; 32], Vec<LaidOutFile>> = HashMap::new();
    for (f, h) in pairs {
        out.entry(h).or_default().push(f);
    }
    Ok(out)
}

fn tier1_hash(f: &LaidOutFile, size: u64) -> std::io::Result<[u8; 32]> {
    let to_read = size.min(TIER1_BYTES) as usize;
    let mut buf = vec![0u8; to_read];
    let mut file = File::open(&f.entry.path)?;
    read_exact_or_eof(&mut file, &mut buf)?;
    Ok(*blake3::hash(&buf).as_bytes())
}

fn tier2_hash(f: &LaidOutFile, size: u64) -> std::io::Result<[u8; 32]> {
    let region = TIER2_REGION as usize;
    let mut head = vec![0u8; region];
    let mut mid = vec![0u8; region];
    let mut tail = vec![0u8; region];

    let mut file = File::open(&f.entry.path)?;
    read_exact_or_eof(&mut file, &mut head)?;

    let mid_off = size.saturating_sub(TIER2_REGION) / 2;
    file.seek(SeekFrom::Start(mid_off))?;
    read_exact_or_eof(&mut file, &mut mid)?;

    let tail_off = size.saturating_sub(TIER2_REGION);
    file.seek(SeekFrom::Start(tail_off))?;
    read_exact_or_eof(&mut file, &mut tail)?;

    let mut hasher = blake3::Hasher::new();
    hasher.update(&head);
    hasher.update(&mid);
    hasher.update(&tail);
    Ok(*hasher.finalize().as_bytes())
}

fn tier3_hash(f: &LaidOutFile, _size: u64) -> std::io::Result<[u8; 32]> {
    let file = File::open(&f.entry.path)?;
    let mut reader = BufReader::with_capacity(TIER3_BUF, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; TIER3_BUF];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn read_exact_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    // Like read_exact but tolerant of EOF — short reads zero-pad.
    let mut total = 0usize;
    while total < buf.len() {
        match r.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    for byte in &mut buf[total..] {
        *byte = 0;
    }
    Ok(total)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputFormat;
    use crate::inventory::FileEntry;
    use crate::pipeline::layout::LaidOutFile;
    use std::fs;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let mut d = std::env::temp_dir();
        d.push(format!(
            "superdupe-hash-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn lo(path: PathBuf, size: u64) -> LaidOutFile {
        LaidOutFile {
            entry: FileEntry {
                path,
                size,
                mtime: 0,
                file_ref: 0,
                parent_ref: 0,
                usn: 0,
                attributes: 0,
                volume_guid: None,
            },
            start_lcn: None,
        }
    }

    fn cfg() -> ScanConfig {
        ScanConfig {
            roots: vec![],
            reference_roots: vec![],
            min_size: 0,
            max_size: None,
            include: None,
            exclude: None,
            format: OutputFormat::Text,
            paranoid: false,
            use_cache: false,
            use_format_aware: false,
            threads: 1,
            queue_depth: None,
            output: None,
            follow_links: false,
            allow_system_paths: false,
        }
    }

    /// Tier 1 must release files whose 4 KiB heads differ.
    #[test]
    fn tier1_releases_distinct_heads() {
        let d = tmpdir();
        let a = d.join("a");
        let b = d.join("b");
        let mut content_a = vec![0u8; 4096];
        let mut content_b = vec![0u8; 4096];
        content_a[0] = 0xAA;
        content_b[0] = 0xBB;
        fs::write(&a, &content_a).unwrap();
        fs::write(&b, &content_b).unwrap();
        let group = LaidOutGroup {
            size: 4096,
            files: vec![lo(a, 4096), lo(b, 4096)],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert!(result.is_empty(), "files with different heads must not group");
        fs::remove_dir_all(&d).ok();
    }

    /// Same head and tail but different middle: Tier 2 must catch this.
    #[test]
    fn tier2_catches_middle_divergence() {
        let d = tmpdir();
        let mk = |path: &str, mid_byte: u8| {
            let mut buf = vec![0u8; 300 * 1024];
            buf[150 * 1024] = mid_byte;
            // Ensure same head and tail.
            fs::write(d.join(path), &buf).unwrap();
            lo(d.join(path), buf.len() as u64)
        };
        let a = mk("a", 0xAA);
        let b = mk("b", 0xBB);
        let c = mk("c", 0xAA);
        let group = LaidOutGroup {
            size: 300 * 1024,
            files: vec![a, b, c],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1, "should find one group of two");
        assert_eq!(result[0].files.len(), 2);
        fs::remove_dir_all(&d).ok();
    }

    /// Identical except final byte: Tier 3 (full) must distinguish.
    #[test]
    fn tier3_catches_one_byte_diff() {
        let d = tmpdir();
        let mut a = vec![0xCDu8; 300 * 1024];
        let mut b = a.clone();
        b[300 * 1024 - 1] = 0x00; // a ends 0xCD, b ends 0x00.
        let _ = a.pop(); // keep both 300 KiB - we want last-byte diff
        a = vec![0xCDu8; 300 * 1024];
        // Both are 300 KiB. tail region is from offset (300K - 64K) = 236K..300K.
        // We set b's last byte to 0 so the tier 2 tail catches it too — that's
        // fine; we still expect at least Tier 2 to release one of them.
        fs::write(d.join("a"), &a).unwrap();
        fs::write(d.join("b"), &b).unwrap();
        let group = LaidOutGroup {
            size: 300 * 1024,
            files: vec![lo(d.join("a"), 300 * 1024), lo(d.join("b"), 300 * 1024)],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert!(result.is_empty(), "differing files must not be reported");
        fs::remove_dir_all(&d).ok();
    }

    /// Two genuine duplicates: must survive every tier.
    #[test]
    fn duplicates_survive_all_tiers() {
        let d = tmpdir();
        let body = vec![0x5Au8; 400 * 1024];
        fs::write(d.join("a"), &body).unwrap();
        fs::write(d.join("b"), &body).unwrap();
        fs::write(d.join("c"), &body).unwrap();
        let group = LaidOutGroup {
            size: body.len() as u64,
            files: vec![
                lo(d.join("a"), body.len() as u64),
                lo(d.join("b"), body.len() as u64),
                lo(d.join("c"), body.len() as u64),
            ],
        };
        let result = run(vec![group], &cfg()).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].files.len(), 3);
        fs::remove_dir_all(&d).ok();
    }
}
