//! Resolved scan configuration. The CLI parses into [`cli::ScanArgs`]; this
//! module turns that into a validated, engine-ready [`ScanConfig`].

use std::path::PathBuf;

use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::cli::{self, OutputFormat, ScanArgs};
use crate::{Error, Result};

/// Validated configuration for a single `scan` invocation.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub roots: Vec<PathBuf>,
    pub reference_roots: Vec<PathBuf>,
    pub min_size: u64,
    pub max_size: Option<u64>,
    /// Runtime override for the Tier 1 head-read size.
    /// Default 4 KiB; CLI `--tier1-bytes` flag lets bench coord
    /// experiment with smaller (matches cz's partial-hash) or
    /// larger (saturate IO queue) values.
    pub tier1_bytes: u64,
    pub include: Option<GlobSet>,
    pub exclude: Option<GlobSet>,
    pub format: OutputFormat,
    pub paranoid: bool,
    pub use_cache: bool,
    pub use_format_aware: bool,
    pub threads: usize,
    /// Worker count for the hashing par_iter specifically. Defaults
    /// to `threads × 3` because Tier 1 (and the small-file Tier 3
    /// fast path) is dominated by `CreateFileW` / `ReadFile` /
    /// `CloseHandle` syscalls — workers spend most of their wall
    /// time blocked, so oversubscription buys real throughput
    /// without saturating the CPU. Set explicitly via `--io-threads`
    /// to sweep where the curve flattens for a given disk + AV mix.
    pub io_threads: usize,
    pub output: Option<PathBuf>,
    pub follow_links: bool,
    pub allow_system_paths: bool,
    /// T2.1 phase 6: when true, the hash worker tier guard accepts
    /// cloud-recall placeholders (forcing hydration on read). Default
    /// false. Flows from `--allow-recall-on-read`.
    pub allow_recall_on_read: bool,
    /// Which content-hash algorithm to use for Tier 1/2/3 + format
    /// fingerprints. BLAKE3 is the default; DDH-128 is the
    /// in-development alternative (currently an xxhash3-128 stub).
    pub hash_algo: crate::pipeline::hash::HashAlgo,
    /// Settings → Exclusions runtime filter (preset packs + custom
    /// extensions + custom path patterns). Defaults to disabled
    /// (master toggle OFF); the walker short-circuits to Included
    /// on every file when this is in the disabled state. Compile
    /// from [`crate::exclusions::ExclusionConfig`] at scan start
    /// once the GUI / CLI exposes a way to populate the config
    /// (Days 3-5 of the scan-options branch).
    pub exclusion_policy: crate::exclusions::ExclusionPolicy,
    /// Live counters bumped by the walker each time an exclusion
    /// fires. Shared via [`std::sync::Arc`] so worker threads
    /// increment atomically; the scan summary reads via the same
    /// pointer at scan end. Reset implicitly per scan (a fresh
    /// `ScanConfig::from_args` makes a fresh counter).
    pub exclusion_counters: std::sync::Arc<crate::exclusions::ExclusionCounters>,
}

impl ScanConfig {
    pub fn from_args(args: &ScanArgs) -> Result<Self> {
        if args.paths.is_empty() {
            return Err(Error::other("at least one scan path is required"));
        }

        let min_size = cli::parse_size(&args.min_size)?;
        let max_size = args.max_size.as_deref().map(cli::parse_size).transpose()?;
        let tier1_bytes = cli::parse_size(&args.tier1_bytes)?;
        if tier1_bytes == 0 {
            return Err(Error::other("--tier1-bytes must be > 0"));
        }
        if let (Some(max), min) = (max_size, min_size) {
            if max < min {
                return Err(Error::other(format!(
                    "--max-size ({max}) is below --min-size ({min})",
                )));
            }
        }

        Ok(Self {
            roots: args.paths.clone(),
            reference_roots: args.reference.clone(),
            min_size,
            max_size,
            tier1_bytes,
            include: build_globset(&args.include)?,
            exclude: build_globset(&args.exclude)?,
            format: args.format,
            paranoid: args.paranoid,
            use_cache: !args.no_cache,
            use_format_aware: !args.no_format_aware,
            threads: args.threads.unwrap_or_else(num_cpus),
            io_threads: {
                let cpu_threads = args.threads.unwrap_or_else(num_cpus);
                args.io_threads
                    .unwrap_or(cpu_threads.saturating_mul(3).max(1))
            },
            output: args.output.clone(),
            follow_links: args.follow_links,
            allow_system_paths: args.allow_system_paths,
            allow_recall_on_read: args.allow_recall_on_read,
            hash_algo: args.hash_algo.into(),
            // Until the CLI's `--exclusions on` / Settings UI wires
            // up an actual `ExclusionConfig`, the walker runs with
            // a disabled policy (master toggle off). Behaviour
            // matches today's "show every duplicate" default.
            exclusion_policy: crate::exclusions::ExclusionPolicy::disabled(),
            exclusion_counters: crate::exclusions::ExclusionCounters::new(),
        })
    }
}

fn build_globset(patterns: &[String]) -> Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        let g = Glob::new(p).map_err(|source| Error::BadGlob {
            pattern: p.clone(),
            source,
        })?;
        b.add(g);
    }
    Ok(Some(b.build().map_err(|source| Error::BadGlob {
        pattern: patterns.join(","),
        source,
    })?))
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests {
    //! Coverage for `ScanConfig::from_args` validation rules + the
    //! default-derivation logic. This is the CLI's contract — a
    //! regression here breaks every command-line invocation.

    use super::*;
    use crate::cli::{HashAlgoArg, OutputFormat, ScanArgs};
    use std::path::PathBuf;

    fn args_with_paths(paths: Vec<PathBuf>) -> ScanArgs {
        ScanArgs {
            paths,
            reference: vec![],
            min_size: "4K".into(),
            max_size: None,
            tier1_bytes: "4K".into(),
            include: vec![],
            exclude: vec![],
            format: OutputFormat::Text,
            paranoid: false,
            no_cache: false,
            no_format_aware: false,
            threads: None,
            io_threads: None,
            output: None,
            follow_links: false,
            allow_system_paths: false,
            placeholders_only: false,
            force_hash: false,
            allow_recall_on_read: false,
            hash_algo: HashAlgoArg::River5,
            mode: crate::cli::ScanMode::Exact,
            image_similarity_threshold: 5,
            image_hash_algorithm: crate::cli::ImageHashAlgoArg::Dhash,
            audio_similarity_threshold: 5.0,
        }
    }

    #[test]
    fn empty_paths_rejected() {
        let a = args_with_paths(vec![]);
        let r = ScanConfig::from_args(&a);
        assert!(r.is_err(), "empty paths must reject");
    }

    #[test]
    fn min_size_parses_suffixes() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.min_size = "1K".into();
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(cfg.min_size, 1024);

        a.min_size = "5M".into();
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(cfg.min_size, 5 * 1024 * 1024);

        a.min_size = "2G".into();
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(cfg.min_size, 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn max_below_min_rejected() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.min_size = "10M".into();
        a.max_size = Some("1M".into());
        let r = ScanConfig::from_args(&a);
        assert!(
            r.is_err(),
            "max-size below min-size must reject (--max < --min is incoherent)"
        );
    }

    #[test]
    fn tier1_bytes_zero_rejected() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.tier1_bytes = "0".into();
        let r = ScanConfig::from_args(&a);
        assert!(r.is_err(), "--tier1-bytes 0 must reject");
    }

    #[test]
    fn tier1_bytes_default_4k() {
        let a = args_with_paths(vec![PathBuf::from("/tmp")]);
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(cfg.tier1_bytes, 4096, "default --tier1-bytes is 4K");
    }

    #[test]
    fn io_threads_defaults_to_3x_threads() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.threads = Some(8);
        a.io_threads = None;
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(
            cfg.io_threads, 24,
            "io_threads defaults to threads*3 (sd oversubscribes IO)"
        );
    }

    #[test]
    fn io_threads_explicit_override_respected() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.threads = Some(4);
        a.io_threads = Some(99);
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(cfg.io_threads, 99);
    }

    #[test]
    fn use_cache_flag_inverts_no_cache() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.no_cache = false;
        assert!(ScanConfig::from_args(&a).unwrap().use_cache);
        a.no_cache = true;
        assert!(!ScanConfig::from_args(&a).unwrap().use_cache);
    }

    #[test]
    fn use_format_aware_flag_inverts_no_format_aware() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.no_format_aware = false;
        assert!(ScanConfig::from_args(&a).unwrap().use_format_aware);
        a.no_format_aware = true;
        assert!(!ScanConfig::from_args(&a).unwrap().use_format_aware);
    }

    #[test]
    fn invalid_glob_pattern_propagates_error() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.include = vec!["[invalid".into()];
        let r = ScanConfig::from_args(&a);
        assert!(r.is_err(), "malformed glob must surface as BadGlob");
    }
}
