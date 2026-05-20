//! Stage 4 — progressive Tier 0–3 hashing.
//!
//! For v0 we ship a single-tier fallback: full BLAKE3 of each file using
//! buffered reads on a Rayon thread pool. The IOCP + direct-I/O pipeline
//! and Tier 0/1/2 escalation arrive in Implementation Order steps 5–6.
//!
//! Despite being a "fallback", this is already a correct duplicate
//! detector — it just doesn't yet exploit the spec's I/O tricks.

use std::fs::File;
use std::io::{BufReader, Read};

use hashbrown::HashMap;
use rayon::prelude::*;

use crate::pipeline::layout::{LaidOutFile, LaidOutGroup};
use crate::pipeline::DuplicateGroup;
use crate::Result;

const READ_BUF: usize = 1 << 20; // 1 MiB

/// Take size-grouped, layout-annotated files; return confirmed duplicate
/// groups by full content hash.
pub fn run(groups: Vec<LaidOutGroup>) -> Result<Vec<DuplicateGroup>> {
    let mut all_dups: Vec<DuplicateGroup> = groups
        .into_par_iter()
        .map(hash_group)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    all_dups.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.files.cmp(&b.files)));
    Ok(all_dups)
}

fn hash_group(group: LaidOutGroup) -> Result<Vec<DuplicateGroup>> {
    // Special-case: zero-byte files are always identical.
    if group.size == 0 {
        if group.files.len() < 2 {
            return Ok(Vec::new());
        }
        let files = group.files.into_iter().map(|f| f.entry.path).collect();
        return Ok(vec![DuplicateGroup {
            size: 0,
            content_hash: blake3::hash(&[]).to_hex().to_string(),
            files,
            link_equivalent: false,
        }]);
    }

    let hashed: Vec<(LaidOutFile, [u8; 32])> = group
        .files
        .into_par_iter()
        .filter_map(|f| match full_blake3(&f) {
            Ok(h) => Some((f, h)),
            Err(e) => {
                tracing::warn!(path = %f.entry.path.display(), error = %e, "hash failed; skipping file");
                None
            }
        })
        .collect();

    let mut by_hash: HashMap<[u8; 32], Vec<LaidOutFile>> = HashMap::new();
    for (file, hash) in hashed {
        by_hash.entry(hash).or_default().push(file);
    }

    let mut out = Vec::new();
    for (hash, files) in by_hash {
        if files.len() < 2 {
            continue;
        }
        let mut paths: Vec<_> = files.into_iter().map(|f| f.entry.path).collect();
        paths.sort();
        out.push(DuplicateGroup {
            size: group.size,
            content_hash: hex(&hash),
            files: paths,
            link_equivalent: false,
        });
    }
    Ok(out)
}

fn full_blake3(f: &LaidOutFile) -> std::io::Result<[u8; 32]> {
    let file = File::open(&f.entry.path)?;
    let mut reader = BufReader::with_capacity(READ_BUF, file);
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; READ_BUF];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(*hasher.finalize().as_bytes())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{:02x}", b);
    }
    s
}
