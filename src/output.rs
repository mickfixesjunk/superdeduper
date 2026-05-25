//! Output formatting for scan results.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::cli::OutputFormat;
use crate::pipeline::{DuplicateGroup, SkippedFile};
use crate::Result;

#[derive(Serialize)]
struct Report<'a> {
    schema: &'static str,
    groups: &'a [DuplicateGroup],
    /// T2.1 phase 7 surface (schema v2): one record per placeholder
    /// observed during inventory (RecallOnOpen, RecallOnDataAccess,
    /// ReparseDedup, OtherReparse). Empty array when no placeholders
    /// fired — present-but-empty makes the schema shape stable across
    /// runs. ReparseDedup files appear here AND in `groups` because
    /// they're observed AND hashable; downstream filters as needed.
    skipped: &'a [SkippedFile],
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
    /// T2.1 phase 7: count of placeholder files observed during
    /// inventory. Equal to `skipped.len()`. Promoted to summary so
    /// scripts that only consume the summary block (without parsing
    /// the whole skipped[] array) can still tell at a glance that
    /// placeholders existed and were handled.
    placeholder_skipped: usize,
}

pub fn write(
    out: &mut dyn Write,
    format: OutputFormat,
    groups: &[DuplicateGroup],
    skipped: &[SkippedFile],
) -> Result<()> {
    let summary = summarize(groups, skipped);
    match format {
        OutputFormat::Text => write_text(out, groups, skipped, &summary)?,
        OutputFormat::Json => {
            let report = Report {
                // Schema bumped v1 → v2 for the skipped[] +
                // placeholder_skipped additions. Consumers reading
                // older v1 outputs continue to work; v2 adds keys
                // they can opt into.
                schema: "superdeduper.scan.v2",
                groups,
                skipped,
                summary,
            };
            serde_json::to_writer_pretty(&mut *out, &report).map_err(io_err)?;
            out.write_all(b"\n")?;
        }
        OutputFormat::Csv => write_csv(out, groups)?,
        OutputFormat::Report => write_report(out, groups, skipped, &summary)?,
    }
    Ok(())
}

/// Markdown-friendly one-page summary. fclones-style. Designed to
/// paste cleanly into a GitHub issue, a Discord/Slack message, or a
/// terminal block-screenshot. Stable section order; humansize is
/// binary (KiB/MiB/GiB) per the rest of the engine's output voice.
fn write_report(
    out: &mut dyn Write,
    groups: &[DuplicateGroup],
    skipped: &[SkippedFile],
    summary: &Summary,
) -> Result<()> {
    writeln!(out, "# superdeduper scan report")?;
    writeln!(out)?;
    writeln!(out, "- **Files scanned:** {}", summary.files)?;
    writeln!(out, "- **Duplicate groups:** {}", summary.groups)?;
    writeln!(
        out,
        "- **Reclaimable (path-aware):** {}",
        humansize::format_size(summary.reclaimable_bytes, humansize::BINARY)
    )?;
    writeln!(
        out,
        "- **Reclaimable (inode-aware):** {}",
        humansize::format_size(summary.reclaimable_inode_bytes, humansize::BINARY)
    )?;
    if summary.placeholder_skipped > 0 {
        writeln!(
            out,
            "- **Placeholders skipped:** {} (cloud / reparse-point files we won't hydrate)",
            summary.placeholder_skipped
        )?;
    }
    writeln!(out)?;

    if groups.is_empty() {
        writeln!(out, "No duplicates found.")?;
        return Ok(());
    }

    // Top 10 groups by inode-aware reclaim. Path-aware would penalise
    // hardlink-heavy groups unfairly; the inode-aware view answers
    // "what actually frees disk space?" which is what a triage report
    // wants to surface first.
    const TOP_N: usize = 10;
    let mut ranked: Vec<(usize, u64)> = groups
        .iter()
        .enumerate()
        .map(|(i, g)| {
            let reclaim = if g.link_equivalent {
                0
            } else {
                g.unique_inodes.saturating_sub(1) * g.size
            };
            (i, reclaim)
        })
        .collect();
    ranked.sort_by_key(|(_, reclaim)| std::cmp::Reverse(*reclaim));
    let shown = ranked.iter().take(TOP_N).count();

    writeln!(out, "## Top {shown} group(s) by reclaimable bytes")?;
    writeln!(out)?;
    writeln!(out, "| # | Size | Files | Reclaim | Sample path |")?;
    writeln!(out, "|---|------|-------|---------|-------------|")?;
    for (rank, (idx, reclaim)) in ranked.iter().take(TOP_N).enumerate() {
        let g = &groups[*idx];
        let sample = g.files.first().map(|p| display_path(p)).unwrap_or_default();
        writeln!(
            out,
            "| {} | {} | {} | {} | `{}` |",
            rank + 1,
            humansize::format_size(g.size, humansize::BINARY),
            g.files.len(),
            humansize::format_size(*reclaim, humansize::BINARY),
            sample,
        )?;
    }
    writeln!(out)?;

    if !skipped.is_empty() {
        writeln!(
            out,
            "_{} placeholder(s) skipped from hashing (recall-on-open / reparse). See full JSON output for the list._",
            skipped.len()
        )?;
        writeln!(out)?;
    }

    writeln!(out, "## Next step")?;
    writeln!(out)?;
    writeln!(
        out,
        "Save the scan result with `--format json --output results.json`, then preview destructive actions:"
    )?;
    writeln!(out)?;
    writeln!(out, "```")?;
    writeln!(
        out,
        "superdeduper dedupe results.json --strategy oldest --action safe-rename --dry-run"
    )?;
    writeln!(out, "```")?;
    writeln!(out)?;
    writeln!(
        out,
        "Drop `--dry-run` to apply. Use `--action recycle` to send losers to the system trash, or `--action hardlink` to dedup in place on a single volume."
    )?;
    Ok(())
}

fn write_text(
    out: &mut dyn Write,
    groups: &[DuplicateGroup],
    skipped: &[SkippedFile],
    summary: &Summary,
) -> Result<()> {
    if groups.is_empty() && skipped.is_empty() {
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
    if !skipped.is_empty() {
        writeln!(
            out,
            "# placeholders observed (skipped from hashing where blocking)"
        )?;
        for s in skipped {
            match s.reparse_tag {
                Some(tag) => writeln!(
                    out,
                    "  [{}:0x{:08x}] {}",
                    s.placeholder,
                    tag,
                    display_path(&s.path)
                )?,
                None => writeln!(out, "  [{}] {}", s.placeholder, display_path(&s.path))?,
            }
        }
        writeln!(out)?;
    }
    writeln!(
        out,
        "Summary: {} group(s), {} file(s), {} reclaimable, {} placeholder(s) skipped.",
        summary.groups,
        summary.files,
        humansize::format_size(summary.reclaimable_bytes, humansize::BINARY),
        summary.placeholder_skipped,
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

fn summarize(groups: &[DuplicateGroup], skipped: &[SkippedFile]) -> Summary {
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
        placeholder_skipped: skipped.len(),
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

#[cfg(test)]
mod summarize_tests {
    //! Coverage for `summarize` — the engine's external contract
    //! for `summary.reclaimable_bytes` + `summary.reclaimable_inode_bytes`
    //! in the JSON output. Backend leaderboard payload + GUI header
    //! tile + the dual-reclaim narrative all depend on these being
    //! correctly computed.

    use super::summarize;
    use crate::pipeline::{DuplicateGroup, SimilarityKind};
    use std::path::PathBuf;

    fn group(size: u64, files: usize, link_equivalent: bool, unique_inodes: u64) -> DuplicateGroup {
        DuplicateGroup {
            size,
            content_hash: "h".into(),
            files: (0..files)
                .map(|i| PathBuf::from(format!("/p/{i}")))
                .collect(),
            link_equivalent,
            unique_inodes,
            similarity_kind: SimilarityKind::ByteIdentical,
        }
    }

    #[test]
    fn empty_input_yields_zero_summary() {
        let s = summarize(&[], &[]);
        assert_eq!(s.groups, 0);
        assert_eq!(s.files, 0);
        assert_eq!(s.reclaimable_bytes, 0);
        assert_eq!(s.reclaimable_inode_bytes, 0);
    }

    #[test]
    fn single_alias_group_credits_both_metrics_equally() {
        // 3 files, each a separate inode → (3-1)*size for both
        // path-aware and inode-aware.
        let g = group(1000, 3, false, 3);
        let s = summarize(&[g], &[]);
        assert_eq!(s.reclaimable_bytes, 2000);
        assert_eq!(s.reclaimable_inode_bytes, 2000);
        assert_eq!(s.groups, 1);
        assert_eq!(s.files, 3);
    }

    #[test]
    fn link_equivalent_group_credits_zero_to_both() {
        // 8 hardlinks of one inode → 0 reclaim from either metric.
        // Crucial invariant: a fully-hardlinked group's bytes are
        // ALREADY shared on disk; counting them as "freeable" is
        // the inflation bug Mick hit on C:\\Windows.
        let g = group(4096, 8, true, 1);
        let s = summarize(&[g], &[]);
        assert_eq!(s.reclaimable_bytes, 0);
        assert_eq!(s.reclaimable_inode_bytes, 0);
        assert_eq!(s.groups, 1);
        assert_eq!(s.files, 8);
    }

    #[test]
    fn partial_hardlink_group_diverges_between_metrics() {
        // 8 paths sharing 3 inodes — the dual-reclaim story. Each
        // metric is `(count - 1) * size` for its respective count.
        // path-aware (count=8): 7000
        // inode-aware (count=3): 2000
        let g = group(1000, 8, false, 3);
        let s = summarize(&[g], &[]);
        assert_eq!(s.reclaimable_bytes, 7000);
        assert_eq!(s.reclaimable_inode_bytes, 2000);
        assert_ne!(s.reclaimable_bytes, s.reclaimable_inode_bytes);
    }

    #[test]
    fn unique_inodes_zero_falls_back_to_files_len_for_inode_metric() {
        // Older checkpoint formats persisted unique_inodes=0 (the
        // field didn't exist). When loaded, summarize must NOT
        // crash + fall back to path-aware for the inode metric.
        let g = group(500, 4, false, 0);
        let s = summarize(&[g], &[]);
        assert_eq!(s.reclaimable_bytes, 1500);
        // Fallback equals path-aware figure in this case.
        assert_eq!(s.reclaimable_inode_bytes, 1500);
    }

    #[test]
    fn mixed_groups_sum_correctly() {
        // Realistic /usr/lib-shaped mix: a hardlink group, a
        // single-alias dup group, a partial-hardlink group.
        let g_hardlink = group(11_000_000, 114, true, 1); // 0 reclaim
        let g_dup = group(1_000_000, 3, false, 3); // 2_000_000 / 2_000_000
        let g_mixed = group(500_000, 6, false, 2); // 2_500_000 / 500_000
        let s = summarize(&[g_hardlink, g_dup, g_mixed], &[]);
        assert_eq!(s.reclaimable_bytes, 4_500_000);
        assert_eq!(s.reclaimable_inode_bytes, 2_500_000);
        assert_eq!(s.groups, 3);
        assert_eq!(s.files, 114 + 3 + 6);
    }

    #[test]
    fn singleton_groups_credit_nothing() {
        // A group with only 1 file has n-1 = 0 reclaim.
        // Engine shouldn't emit these but the function must handle
        // them sanely. files counter still includes the file.
        let g = group(1000, 1, false, 1);
        let s = summarize(&[g], &[]);
        assert_eq!(s.reclaimable_bytes, 0);
        assert_eq!(s.reclaimable_inode_bytes, 0);
        assert_eq!(s.files, 1);
    }

    #[test]
    fn largest_per_group_reclaim_invariant() {
        // Backend sanity check requires
        // `largest_single_group_bytes <= duplicate_bytes_reclaimable`.
        // Verify it holds across the realistic mix.
        let g_a = group(2_000_000, 3, false, 3); // 4_000_000
        let g_b = group(800_000, 5, false, 5); // 3_200_000
        let g_c = group(10_000, 2, false, 2); // 10_000
        let s = summarize(&[g_a, g_b, g_c], &[]);
        let largest_inode = 4_000_000u64;
        assert!(
            largest_inode <= s.reclaimable_inode_bytes,
            "max per-group reclaim ({largest_inode}) must be <= total ({})",
            s.reclaimable_inode_bytes
        );
    }
}
