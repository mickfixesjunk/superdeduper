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
    /// Bytes that could be reclaimed if every group kept exactly one file.
    reclaimable_bytes: u64,
}

pub fn write(out: &mut dyn Write, format: OutputFormat, groups: &[DuplicateGroup]) -> Result<()> {
    let summary = summarize(groups);
    match format {
        OutputFormat::Text => write_text(out, groups, &summary)?,
        OutputFormat::Json => {
            let report = Report {
                schema: "superdupe.scan.v1",
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
    for g in groups {
        files += g.files.len();
        let n = g.files.len() as u64;
        if n > 1 {
            reclaimable_bytes = reclaimable_bytes.saturating_add(g.size.saturating_mul(n - 1));
        }
    }
    Summary {
        groups: groups.len(),
        files,
        reclaimable_bytes,
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
