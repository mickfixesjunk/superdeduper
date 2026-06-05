//! Submission-payload metadata helpers shared by the CLI
//! (`src/main.rs`'s `run_scan`) and the GUI
//! (`src/gui/live.rs`'s engine thread).
//!
//! #142 — Pre-#142 these helpers lived only in `gui::live` because
//! only the GUI built a submission payload at scan-finish. The CLI
//! flow's `run_scan` wrote a scan_history row WITHOUT a payload,
//! which made every CLI-scanned row invisible to `submit-pending`
//! (it skips rows with no payload), blocking testdesign's e2e
//! flow + every pathfinder achievement that depends on a CLI
//! submission landing on the leaderboard.
//!
//! Moved here so both surfaces compute scope / corpus_kind /
//! share_count_in_scope from the same source.

#![cfg(feature = "telemetry")]

use std::path::{Path, PathBuf};

/// Scope heuristic: `"selection"` (multi-root), `"whole-volume"`
/// (single drive-root), or `"subdirectory"` (single non-root path).
/// Backend uses this for run-shape bucketing.
pub fn classify_scope(roots: &[PathBuf]) -> String {
    if roots.len() > 1 {
        return "selection".to_string();
    }
    match roots.first() {
        Some(p) if is_drive_root(p) => "whole-volume".to_string(),
        Some(_) => "subdirectory".to_string(),
        None => "subdirectory".to_string(),
    }
}

/// `"system"` if any root path looks like an OS-system tree;
/// otherwise `"user-data"`. Conservative heuristic — the backend
/// uses this for category bucketing.
pub fn classify_corpus_kind(roots: &[PathBuf]) -> String {
    for p in roots {
        let s = p.to_string_lossy().to_ascii_lowercase();
        if s.contains("\\windows\\")
            || s.ends_with("\\windows")
            || s.contains("/system/")
            || s.starts_with("/system")
            || s.contains("\\program files")
            || s.starts_with("/usr/")
            || s.starts_with("/bin/")
            || s.starts_with("/sbin/")
        {
            return "system".to_string();
        }
    }
    "user-data".to_string()
}

/// True if `p` looks like a whole-volume root. Windows `C:\`,
/// verbatim `\\?\C:\`, or Unix `/`.
pub fn is_drive_root(p: &Path) -> bool {
    let s = p.to_string_lossy();
    s == "/"
        || (s.len() == 3 && s.chars().nth(1) == Some(':') && s.ends_with('\\'))
        || (s.len() == 7 && s.starts_with("\\\\?\\") && s.ends_with('\\'))
}

/// True if `p` looks like a network share — Windows UNC
/// (`\\server\share\...` or its verbatim form `\\?\UNC\...`) or a
/// URL-style network-FS scheme (`smb://`, `nfs://`, `cifs://`).
/// Used by `count_distinct_share_roots` + as a feature-bit input
/// elsewhere in the submission build.
pub fn is_network_share_path(p: &Path) -> bool {
    let s = p.to_string_lossy();
    let bytes = s.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'\\' && bytes[1] == b'\\' {
        let prefix3 = bytes.get(2).copied();
        if prefix3 != Some(b'?') && prefix3 != Some(b'.') {
            return true;
        }
        if s.starts_with("\\\\?\\UNC\\") {
            return true;
        }
    }
    s.starts_with("smb://") || s.starts_with("nfs://") || s.starts_with("cifs://")
}

/// Count distinct `\\server\share` (or URL-authority) groupings
/// among `paths`. Multiple roots into the same share count once.
/// Backend uses this for the `multi-share-maestro` latent grant.
pub fn count_distinct_share_roots(paths: &[PathBuf]) -> u64 {
    use std::collections::HashSet;
    let mut shares: HashSet<String> = HashSet::new();
    for p in paths {
        if !is_network_share_path(p) {
            continue;
        }
        let s = p.to_string_lossy();
        let key = if let Some(rest) = s.strip_prefix("\\\\?\\UNC\\") {
            let two: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
            format!("unc:{}", two.join("\\"))
        } else if let Some(rest) = s.strip_prefix("\\\\") {
            let two: Vec<&str> = rest.splitn(3, '\\').take(2).collect();
            format!("unc:{}", two.join("\\"))
        } else if let Some(rest) = s.strip_prefix("smb://") {
            let authority = rest.split('/').next().unwrap_or("");
            format!("smb:{authority}")
        } else if let Some(rest) = s.strip_prefix("nfs://") {
            let authority = rest.split('/').next().unwrap_or("");
            format!("nfs:{authority}")
        } else if let Some(rest) = s.strip_prefix("cifs://") {
            let authority = rest.split('/').next().unwrap_or("");
            format!("cifs:{authority}")
        } else {
            continue;
        };
        shares.insert(key);
    }
    shares.len() as u64
}

/// #162 -- streaming accumulator for the 3 esoteric run_shape dup-group
/// metrics (zero_byte_group_max / max_hardlink_count_in_scan /
/// name_collision_count). Lets the GUI emit-loop feed groups one at a time
/// (`add_group` per emission), then `finalize()` at RunShape build time --
/// SAME algorithm as `run_shape_esoterics` (the batch CLI path), so the
/// two surfaces are physically guaranteed to agree (drift = test failure
/// via `run_shape_esoterics_streaming_matches_batch`).
///
/// Naming: keeps the wider "run_shape esoterics" frame so a future
/// addition (next esoteric metric) lands in one place + propagates to
/// both surfaces.
#[derive(Default)]
pub struct RunShapeEsotericsAccumulator {
    zero_byte_group_max: u64,
    max_hardlink_count_in_scan: u64,
    basename_to_hashes:
        std::collections::HashMap<String, std::collections::HashSet<String>>,
}

impl RunShapeEsotericsAccumulator {
    /// Empty accumulator -- equivalent to no groups observed.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update with one group. `paths` is the iterator of the group's
    /// member paths -- accepts anything `AsRef<Path>` so both
    /// `Vec<PathBuf>` (CLI) and `&[PathBuf]` slices (GUI summary) work
    /// without a clone.
    pub fn add_group<'p, P>(
        &mut self,
        size: u64,
        content_hash: &str,
        link_equivalent: bool,
        paths: impl IntoIterator<Item = &'p P>,
    ) where
        P: AsRef<std::path::Path> + 'p,
    {
        let mut members: u64 = 0;
        for path in paths {
            members += 1;
            if let Some(name) = path.as_ref().file_name().and_then(|n| n.to_str()) {
                self.basename_to_hashes
                    .entry(name.to_string())
                    .or_default()
                    .insert(content_hash.to_string());
            }
        }
        if size == 0 && members > self.zero_byte_group_max {
            self.zero_byte_group_max = members;
        }
        if link_equivalent && members > self.max_hardlink_count_in_scan {
            self.max_hardlink_count_in_scan = members;
        }
    }

    /// Finalize into the `(zero_byte_group_max,
    /// max_hardlink_count_in_scan, name_collision_count)` triple with
    /// the `>0 ? Some : None` convention RunShape expects.
    pub fn finalize(self) -> (Option<u64>, Option<u64>, Option<u64>) {
        let name_collision_count = self
            .basename_to_hashes
            .values()
            .filter(|hs| hs.len() >= 2)
            .count() as u64;
        let opt = |n: u64| if n > 0 { Some(n) } else { None };
        (
            opt(self.zero_byte_group_max),
            opt(self.max_hardlink_count_in_scan),
            opt(name_collision_count),
        )
    }
}

/// #162 -- the 3 esoteric run_shape dup-group metrics, computed from a SINGLE
/// shared source so the CLI (`main.rs`) and the GUI emitter agree and can't
/// drift. Previously the GUI computed these inline (gui/live.rs) while the CLI
/// hardcoded `None`, so zero-byte-reunion / hardlink-farm / name-twins were
/// unearnable on CLI. Returns `(zero_byte_group_max, max_hardlink_count_in_scan,
/// name_collision_count)` with the GUI's `>0 ? Some : None` convention:
/// - `zero_byte_group_max`: largest 0-byte dup group by member count.
/// - `max_hardlink_count_in_scan`: largest `link_equivalent` group by member
///   count (a confirmed lower bound on that inode's nlink).
/// - `name_collision_count`: basenames resolving to ≥2 distinct content hashes.
///
/// Implementation just wraps the streaming accumulator above -- so the
/// batch (CLI) and streaming (GUI) paths share one body of code.
pub fn run_shape_esoterics(
    groups: &[crate::pipeline::DuplicateGroup],
) -> (Option<u64>, Option<u64>, Option<u64>) {
    let mut acc = RunShapeEsotericsAccumulator::new();
    for g in groups {
        acc.add_group(g.size, &g.content_hash, g.link_equivalent, &g.files);
    }
    acc.finalize()
}

/// Codex-review item 2 (v0.3.25 2026-06-02): shared
/// `SubmissionInputs` builder for non-bench scan submissions. Centralizes
/// the constant fields (`client_version` / `run_uuid` / `walker_variant`
/// / `dry_run` / `groups_reviewed_count` / `actions_taken_summary` /
/// `placeholder_skip_bytes` / `client_found_dupsets` / `bench` / `lane`)
/// so a wire-shape evolution touches ONE function instead of two.
///
/// Callers populate `ScanSubmissionArgs` (struct-init syntax keeps
/// argument labelling clear despite the parameter count) + invoke
/// `build_scan_submission_inputs`. Used by:
/// * `src/main.rs::run_scan` -- CLI scan-finish path
/// * `src/gui/live.rs::engine_thread` -- GUI streaming-accumulator path
///
/// Pre-refactor the two call sites had byte-identical field-name
/// layouts but differed in variant-value sourcing (e.g. CLI's
/// `reclaimable_bytes` vs GUI's `reclaimable_inode`); the helper
/// preserves that pattern while pulling the constants into one place.
pub struct ScanSubmissionArgs {
    pub scan_id: String,
    pub hardware: superdeduper_bench_iface::HardwareFingerprint,
    pub wall_clock_seconds: f64,
    pub bytes_scanned: u64,
    pub files_scanned: u64,
    pub hash_algorithm: String,
    pub scope: String,
    pub features_used_bitmap: u64,
    pub corpus_kind: String,
    pub cache_hit_ratio: Option<f64>,
    pub easter_egg_hits: Vec<String>,
    pub zero_byte_group_max: Option<u64>,
    pub max_hardlink_count_in_scan: Option<u64>,
    pub name_collision_count: Option<u64>,
    pub share_count_in_scope: Option<u64>,
    pub duplicate_groups: u64,
    pub duplicate_bytes_reclaimable: u64,
    pub largest_single_group_bytes: u64,
    pub placeholder_skip_count: Option<u64>,
}

pub fn build_scan_submission_inputs(
    args: ScanSubmissionArgs,
) -> crate::leaderboard::submission::SubmissionInputs {
    use crate::leaderboard::submission::{ResultSummary, RunShape, SubmissionInputs};
    SubmissionInputs {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        run_uuid: uuid::Uuid::new_v4().to_string(),
        scan_id: Some(args.scan_id),
        hardware: args.hardware,
        run_shape: RunShape {
            wall_clock_seconds: args.wall_clock_seconds,
            bytes_scanned: args.bytes_scanned,
            files_scanned: args.files_scanned,
            hash_algorithm: args.hash_algorithm,
            walker_variant: "hybrid".to_string(),
            scope: args.scope,
            features_used_bitmap: args.features_used_bitmap,
            corpus_kind: args.corpus_kind,
            cache_hit_ratio: args.cache_hit_ratio,
            easter_egg_hits: args.easter_egg_hits,
            zero_byte_group_max: args.zero_byte_group_max,
            max_hardlink_count_in_scan: args.max_hardlink_count_in_scan,
            name_collision_count: args.name_collision_count,
            share_count_in_scope: args.share_count_in_scope,
            // #89: kept None per design's catalog-semantic flag (see
            // submission.rs comment on the field). Reactivate when a
            // real dry-run UX ships.
            dry_run: None,
            // #89: group-reviews happen post-scan-finish; the count
            // is always 0 at initial submission time.
            groups_reviewed_count: None,
        },
        result_summary: ResultSummary {
            duplicate_groups: args.duplicate_groups,
            duplicate_bytes_reclaimable: args.duplicate_bytes_reclaimable,
            largest_single_group_bytes: args.largest_single_group_bytes,
            // Always empty at scan-end -- actions happen post-scan;
            // PATCH /actions populates this later.
            actions_taken_summary: std::collections::BTreeMap::new(),
            placeholder_skip_count: args.placeholder_skip_count,
            // Always None at scan-end -- the tier guard hasn't been
            // threaded to track per-placeholder byte totals yet
            // (separate follow-up).
            placeholder_skip_bytes: None,
            // T-BENCH-ME field; always None for non-bench scans.
            client_found_dupsets: None,
        },
        // T-BENCH-ME canonical-bench fields; always None on the
        // non-bench scan-submission path.
        bench: None,
        // Mick bench-lane UX; always None for non-bench submissions
        // (server falls back to has_account_linkage gating).
        lane: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn classify_scope_buckets() {
        assert_eq!(classify_scope(&[p("C:\\")]), "whole-volume");
        assert_eq!(classify_scope(&[p("/")]), "whole-volume");
        assert_eq!(classify_scope(&[p("/home/mick")]), "subdirectory");
        assert_eq!(
            classify_scope(&[p("/home/mick"), p("/tmp")]),
            "selection",
        );
        assert_eq!(classify_scope(&[]), "subdirectory");
    }

    #[test]
    fn classify_corpus_kind_buckets() {
        assert_eq!(
            classify_corpus_kind(&[p("C:\\Windows\\System32")]),
            "system",
        );
        assert_eq!(classify_corpus_kind(&[p("/usr/lib")]), "system");
        assert_eq!(classify_corpus_kind(&[p("/home/mick")]), "user-data");
        assert_eq!(classify_corpus_kind(&[]), "user-data");
    }

    #[test]
    fn is_network_share_path_recognises_common_forms() {
        assert!(is_network_share_path(&p("\\\\server\\share\\file")));
        assert!(is_network_share_path(&p("\\\\?\\UNC\\server\\share")));
        assert!(is_network_share_path(&p("smb://server/share")));
        assert!(is_network_share_path(&p("nfs://server/export")));
        assert!(!is_network_share_path(&p("\\\\?\\C:\\path")));
        assert!(!is_network_share_path(&p("C:\\path")));
        assert!(!is_network_share_path(&p("/home/mick")));
    }

    #[test]
    fn count_distinct_share_roots_groups_by_server_share() {
        let paths = vec![
            p("\\\\server\\share1\\a"),
            p("\\\\server\\share1\\b"),
            p("\\\\server\\share2\\c"),
            p("smb://other/export"),
            p("/home/mick"),
        ];
        // 3 distinct share roots: server\share1, server\share2, smb:other
        assert_eq!(count_distinct_share_roots(&paths), 3);
    }

    // #162 — the shared run_shape esoterics computation (CLI + GUI source of
    // truth). Locks the 3 metrics + the >0 ? Some : None convention so the CLI
    // can't silently drop them again (it used to hardcode None).
    #[test]
    fn run_shape_esoterics_computes_the_three_metrics() {
        use crate::pipeline::DuplicateGroup;
        let g = |size: u64, hash: &str, link: bool, files: &[&str]| DuplicateGroup {
            size,
            content_hash: hash.to_string(),
            files: files.iter().map(PathBuf::from).collect(),
            link_equivalent: link,
            ..Default::default()
        };
        let groups = vec![
            // largest 0-byte group has 3 members -> zero_byte_group_max = 3.
            g(0, "z", false, &["/a/e1", "/a/e2", "/a/e3"]),
            // largest link_equivalent group has 4 members -> max_hardlink = 4.
            g(100, "hl", true, &["/b/h1", "/b/h2", "/b/h3", "/b/h4"]),
            // "twin.txt" appears in two groups with DIFFERENT hashes -> 1 collision.
            g(10, "ha", false, &["/x/twin.txt", "/x/uniq_a"]),
            g(20, "hb", false, &["/y/twin.txt", "/y/uniq_b"]),
        ];
        assert_eq!(
            run_shape_esoterics(&groups),
            (Some(3), Some(4), Some(1)),
            "zero_byte_group_max / max_hardlink_count_in_scan / name_collision_count"
        );

        // No 0-byte / no link-equiv / no name-twin -> all None (the convention).
        let plain = vec![g(50, "x", false, &["/p/only.bin", "/p/only2.bin"])];
        assert_eq!(run_shape_esoterics(&plain), (None, None, None));
        assert_eq!(run_shape_esoterics(&[]), (None, None, None));
    }

    /// #162 -- A-cross-surface-emitter-parity-guard. The CLI path
    /// calls `run_shape_esoterics(&groups)` (batch) and the GUI path
    /// calls `RunShapeEsotericsAccumulator::add_group(...).finalize()`
    /// (streaming, one group at a time as emissions arrive). Both
    /// surfaces MUST produce identical output for the same input set,
    /// otherwise the same 3 achievements (`zero-byte-reunion`,
    /// `hardlink-farm`, `name-twins`) are earnable on one surface but
    /// not the other -- the exact drift class #162 was filed to
    /// close. This test pins the streaming = batch invariant; a
    /// future divergence (someone "optimizes" only one path) is a
    /// compile-or-test failure rather than a silent achievement gap.
    #[test]
    fn run_shape_esoterics_streaming_matches_batch() {
        use crate::pipeline::DuplicateGroup;
        let g = |size: u64, hash: &str, link: bool, files: &[&str]| DuplicateGroup {
            size,
            content_hash: hash.to_string(),
            files: files.iter().map(PathBuf::from).collect(),
            link_equivalent: link,
            ..Default::default()
        };
        // Mix of shapes that exercise every accumulator branch:
        // zero-byte largest, hardlink-equivalent largest, multiple
        // basename collisions across distinct hashes, plus
        // non-contributing groups (plain dups).
        let groups = vec![
            g(0, "z", false, &["/a/e1", "/a/e2", "/a/e3"]),
            g(100, "hl", true, &["/b/h1", "/b/h2", "/b/h3", "/b/h4"]),
            g(10, "ha", false, &["/x/twin.txt", "/x/uniq_a"]),
            g(20, "hb", false, &["/y/twin.txt", "/y/uniq_b"]),
            g(30, "p1", false, &["/p/only.bin", "/p/only2.bin"]),
            g(0, "z2", false, &["/c/zb1", "/c/zb2"]),
        ];

        // Batch path -- what the CLI computes.
        let batch = run_shape_esoterics(&groups);

        // Streaming path -- what the GUI emit-loop computes.
        let mut acc = RunShapeEsotericsAccumulator::new();
        for grp in &groups {
            acc.add_group(grp.size, &grp.content_hash, grp.link_equivalent, &grp.files);
        }
        let streaming = acc.finalize();

        assert_eq!(
            batch, streaming,
            "CLI batch run_shape_esoterics and GUI streaming RunShapeEsotericsAccumulator \
             must agree byte-for-byte (#162 cross-surface emitter-parity guard)"
        );

        // Sanity: empty input both ways.
        assert_eq!(
            run_shape_esoterics(&[]),
            RunShapeEsotericsAccumulator::new().finalize(),
        );
    }

    /// #162 -- streaming order independence. Shuffling the order in
    /// which groups are fed to the accumulator MUST NOT change the
    /// final triple. The accumulator's state shape (max-so-far +
    /// HashMap insertions) is order-independent by construction; this
    /// test pins that property so a future change that introduces
    /// order sensitivity (e.g. "first group wins" semantics) fails
    /// loudly.
    #[test]
    fn run_shape_esoterics_streaming_is_order_independent() {
        use crate::pipeline::DuplicateGroup;
        let g = |size: u64, hash: &str, link: bool, files: &[&str]| DuplicateGroup {
            size,
            content_hash: hash.to_string(),
            files: files.iter().map(PathBuf::from).collect(),
            link_equivalent: link,
            ..Default::default()
        };
        let groups = vec![
            g(0, "z", false, &["/a/e1", "/a/e2", "/a/e3"]),
            g(100, "hl", true, &["/b/h1", "/b/h2", "/b/h3", "/b/h4"]),
            g(10, "ha", false, &["/x/twin.txt"]),
            g(20, "hb", false, &["/y/twin.txt"]),
        ];

        let mut forward = RunShapeEsotericsAccumulator::new();
        for grp in groups.iter() {
            forward.add_group(grp.size, &grp.content_hash, grp.link_equivalent, &grp.files);
        }

        let mut reverse = RunShapeEsotericsAccumulator::new();
        for grp in groups.iter().rev() {
            reverse.add_group(grp.size, &grp.content_hash, grp.link_equivalent, &grp.files);
        }

        assert_eq!(forward.finalize(), reverse.finalize());
    }
}
