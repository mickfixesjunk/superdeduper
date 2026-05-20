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
    pub include: Option<GlobSet>,
    pub exclude: Option<GlobSet>,
    pub format: OutputFormat,
    pub paranoid: bool,
    pub use_cache: bool,
    pub use_format_aware: bool,
    pub threads: usize,
    pub queue_depth: Option<usize>,
    pub output: Option<PathBuf>,
    pub follow_links: bool,
    pub allow_system_paths: bool,
    /// Which content-hash algorithm to use for Tier 1/2/3 + format
    /// fingerprints. BLAKE3 is the default; DDH-128 is the
    /// in-development alternative (currently an xxhash3-128 stub).
    pub hash_algo: crate::pipeline::hash::HashAlgo,
}

impl ScanConfig {
    pub fn from_args(args: &ScanArgs) -> Result<Self> {
        if args.paths.is_empty() {
            return Err(Error::other("at least one scan path is required"));
        }

        let min_size = cli::parse_size(&args.min_size)?;
        let max_size = args.max_size.as_deref().map(cli::parse_size).transpose()?;
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
            include: build_globset(&args.include)?,
            exclude: build_globset(&args.exclude)?,
            format: args.format,
            paranoid: args.paranoid,
            use_cache: !args.no_cache,
            use_format_aware: !args.no_format_aware,
            threads: args.threads.unwrap_or_else(num_cpus),
            queue_depth: args.queue_depth,
            output: args.output.clone(),
            follow_links: args.follow_links,
            allow_system_paths: args.allow_system_paths,
            hash_algo: args.hash_algo.into(),
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
