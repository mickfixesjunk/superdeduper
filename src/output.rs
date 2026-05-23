//! Output formatting for scan results.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::pipeline::DuplicateGroup;
use crate::Result;

#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    groups: &'a [DuplicateGroup],
    summary: Summary,
}

#[derive(Serialize)]
struct Summary {
    groups: usize,
    files: usize,
    /// Path-aware reclaimable: bytes you'd save by deleting every
    /// duplicate path, summed across groups. Overstates the actual
    /// disk free for hardlink-heavy corpora (deleting N-1 hardlink
    /// aliases of one inode frees zero bytes — the data stays).
    /// Kept under the old name for backwards-compat with existing
    /// consumers; semantically equivalent to "duplicate path bytes."
    reclaimable_bytes: u64,
    /// Inode-aware reclaimable: bytes that ACTUALLY come back after
    /// dedup, computed as `(distinct inodes - 1) * size` per group.
    /// On hardlink-heavy corpora (System32 ↔ WinSxS) this is
    /// substantially smaller than `reclaimable_bytes`. This is the
    /// number a user wants when answering "how much disk will I get
    /// back?" `0` for groups whose JSON predates the `unique_inodes`
    /// field (it falls back to the path-aware metric in that case).
    reclaimable_inode_bytes: u64,
}

pub fn write(out: &mut dyn Write, format: OutputFormat, groups: &[DuplicateGroup]) -> Result<()> {
    let summary = summarize(groups);
    match format {
        OutputFormat::Text => write_text(out, groups, &summary)?,
        OutputFormat::Json => {
            let report = Report {
                schema: "superdeduper.scan.v1",
                groups,
                summary,
            };
            serde_json::to_writer_pretty(&mut *out, &report).map_err(io_err)?;
            out.write_all(b"\n")?;
        }
        OutputFormat::Csv => write_csv(out, groups)?,
    }
    Ok(())
}

fn write_text(out: &mut dyn Write, groups: &[DuplicateGroup], summary: &Summary) -> Result<()> {
    if groups.is_empty() {
        writeln!(out, "No duplicates found.")?;
        return Ok(());
    }
    for (i, g) in groups.iter().enumerate() {
        writeln!(
            out,
            "# group {}  size={}  hash={}{}",
            i + 1,
            humansize::format_size(g.size, humansize::BINARY),
            &g.content_hash,
            if g.link_equivalent {
                "  (link-equivalent)"
            } else {
                ""
            },
        )?;
        for p in &g.files {
            writeln!(out, "  {}", display_path(p))?;
        }
        writeln!(out)?;
    }
    writeln!(
        out,
        "Summary: {} group(s), {} file(s), {} reclaimable.",
        summary.groups,
        summary.files,
        humansize::format_size(summary.reclaimable_bytes, humansize::BINARY),
    )?;
    Ok(())
}

fn write_csv(out: &mut dyn Write, groups: &[DuplicateGroup]) -> Result<()> {
    let mut w = csv::Writer::from_writer(out);
    w.write_record([
        "group",
        "size_bytes",
        "content_hash",
        "path",
        "link_equivalent",
    ])
    .map_err(csv_err)?;
    for (i, g) in groups.iter().enumerate() {
        for p in &g.files {
            w.write_record([
                &(i + 1).to_string(),
                &g.size.to_string(),
                &g.content_hash,
                &display_path(p),
                &g.link_equivalent.to_string(),
            ])
            .map_err(csv_err)?;
        }
    }
    w.flush().map_err(io_err)?;
    Ok(())
}

fn summarize(groups: &[DuplicateGroup]) -> Summary {
    let mut files = 0usize;
    let mut reclaimable_bytes = 0u64;
    let mut reclaimable_inode_bytes = 0u64;
    for g in groups {
        files += g.files.len();
        let n = g.files.len() as u64;
        // Hardlinked groups (link_equivalent) already share storage
        // on disk; there's nothing to reclaim by collapsing them.
        // Counting them overstates the rolling Reclaimable header
        // figure that downstream tooling reads.
        if n > 1 && !g.link_equivalent {
            reclaimable_bytes = reclaimable_bytes.saturating_add(g.size.saturating_mul(n - 1));
        }
        // Inode-aware metric — what disk would ACTUALLY free if we
        // dedup'd this group: (distinct inodes - 1) * size. Hardlink
        // aliases collapse to one inode here, so a group with 5 paths
        // sharing 2 inodes contributes 1*size, not 4*size. `0` in
        // `unique_inodes` means "unknown" (older JSON), in which case
        // we fall back to the path-aware metric.
        let unique = if g.unique_inodes == 0 {
            n
        } else {
            g.unique_inodes
        };
        if unique > 1 && !g.link_equivalent {
            reclaimable_inode_bytes =
                reclaimable_inode_bytes.saturating_add(g.size.saturating_mul(unique - 1));
        }
    }
    Summary {
        groups: groups.len(),
        files,
        reclaimable_bytes,
        reclaimable_inode_bytes,
    }
}

fn display_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn io_err(e: impl std::error::Error) -> crate::Error {
    crate::Error::other(e.to_string())
}

fn csv_err(e: csv::Error) -> crate::Error {
    crate::Error::other(e.to_string())
}
