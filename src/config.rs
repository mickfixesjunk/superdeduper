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
    /// When `true`, opt into the MFT volume-root fast path for any
    /// scan whose roots are all volume roots (e.g. `C:\`, `D:\`).
    /// Default `false`: every root goes through the directory walker
    /// (per v0.3.16 Path B; Mick GO 2026-06-01 07:53 PDT).
    ///
    /// Requires an elevated process token (admin) to actually engage;
    /// non-admin processes will see MFT enum return `ACCESS_DENIED`
    /// and fall through to the walker with a warning. Subdir scans
    /// always use the walker regardless of this flag.
    ///
    /// Tradeoffs the user accepts when setting `--force-mft`:
    /// * Hardlink aliases collapsed to canonical paths (data-loss
    ///   versus walker's full alias visibility -- 738 vs 11299
    ///   measured ratio on a heavily-hardlinked Windows volume).
    /// * Exclusion filters (`is_superdeduper_self_path`,
    ///   `dropped_by_exclusions`) historically not applied via this
    ///   path. Plus ~41k system-DLL / OS-protected paths surface as
    ///   candidates that the walker excludes by policy.
    /// * In exchange: per-file enum cost drops via the batched
    ///   `FSCTL_ENUM_USN_DATA` syscall when the volume happens to be
    ///   shaped right for the MFT path's full-volume scan.
    pub force_mft: bool,
    /// v0.3.23 (2026-06-01): when true, multi-root scans walk all
    /// roots concurrently via rayon `par_iter` rather than serially.
    /// Each parallel root gets its own symlink-cycle `visited_dirs`
    /// HashSet (cross-root duplicate aliases caught by the post-walk
    /// `dedup_by_path` pass). Default false; opt-in via
    /// `--parallel-roots` for the multi-root use case (Mick: 4 folders
    /// on the same HDD scanned together). On a single-root scan the
    /// flag is a no-op.
    pub parallel_roots: bool,
    /// T2.1 phase 6: when true, the hash worker tier guard accepts
    /// cloud-recall placeholders (forcing hydration on read). Default
    /// false. Flows from `--allow-recall-on-read`.
    pub allow_recall_on_read: bool,
    /// 2026-06-02 R2 engine-ask: when true, every file read in the
    /// scan path opens with FILE_FLAG_NO_BUFFERING (Windows) /
    /// O_DIRECT (Linux non-WSL) / F_NOCACHE (macOS) so the OS page
    /// cache is bypassed. Lets sdd-testwin's io-thread scaling sweep
    /// measure cold-cache behaviour regardless of corpus size vs RAM.
    /// Flows from `--cold-enforced`. Per-tier impact: tier 1 + tier 3
    /// cold-bypass; tier 2 falls back to buffered (mid/tail seeks not
    /// sector-aligned for arbitrary file sizes). MEASUREMENT mode only.
    pub cold_enforced: bool,
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
            return Err(Error::ConfigInvalid {
                field: "paths",
                reason: "at least one scan path is required".into(),
            });
        }

        let min_size = cli::parse_size(&args.min_size)?;
        let max_size = args.max_size.as_deref().map(cli::parse_size).transpose()?;
        let tier1_bytes = cli::parse_size(&args.tier1_bytes)?;
        if tier1_bytes == 0 {
            return Err(Error::ConfigInvalid {
                field: "--tier1-bytes",
                reason: "must be > 0".into(),
            });
        }
        if let (Some(max), min) = (max_size, min_size) {
            if max < min {
                return Err(Error::ConfigInvalid {
                    field: "--max-size",
                    reason: format!("({max}) is below --min-size ({min})"),
                });
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
            use_cache: !args.no_cache,
            use_format_aware: !args.no_format_aware,
            threads: args.threads.unwrap_or_else(num_cpus),
            io_threads: {
                let cpu_threads = args.threads.unwrap_or_else(num_cpus);
                args.io_threads
                    .unwrap_or_else(|| default_io_threads(cpu_threads, args.paths.first()))
            },
            output: args.output.clone(),
            follow_links: args.follow_links,
            allow_system_paths: args.allow_system_paths,
            force_mft: args.force_mft,
            parallel_roots: args.parallel_roots,
            allow_recall_on_read: args.allow_recall_on_read,
            cold_enforced: args.cold_enforced,
            hash_algo: args.hash_algo.into(),
            // #81 — Wire the CLI's exclusion flags into the runtime
            // policy. Defaults to safe-defaults ON (the new v0.2.7+
            // baseline); --exclusions off disables entirely;
            // --exclusion-pack / --exclusion-pack-disable modify the
            // active-packs vector either side of the default.
            exclusion_policy: build_cli_exclusion_policy(args)?,
            exclusion_counters: crate::exclusions::ExclusionCounters::new(),
        })
    }
}

/// #81 — Build the runtime `ExclusionPolicy` from the CLI flags.
///
/// Behaviour:
/// * `--exclusions off` ⇒ disabled policy (every file scannable).
/// * Otherwise: start from the safe-defaults shape (4 packs ON);
///   remove any pack named in `--exclusion-pack-disable`; add any
///   pack named in `--exclusion-pack`. Custom extension / glob CLI
///   flags are not yet wired (deferred to a follow-up; users with
///   custom needs can edit the GUI settings TOML).
fn build_cli_exclusion_policy(args: &ScanArgs) -> Result<crate::exclusions::ExclusionPolicy> {
    use crate::exclusions::{ExclusionConfig, ExclusionPolicy, PresetPackId};
    if matches!(args.exclusions, cli::ExclusionsToggle::Off) {
        return Ok(ExclusionPolicy::disabled());
    }
    let mut config = ExclusionConfig::default();
    for s in &args.exclusion_pack_disable {
        let id = parse_pack_id(s)?;
        config.active_packs.retain(|p| *p != id);
    }
    for s in &args.exclusion_pack_enable {
        let id = parse_pack_id(s)?;
        if !config.active_packs.contains(&id) {
            config.active_packs.push(id);
        }
    }
    // Preserve canonical order so the in-memory config matches what
    // the GUI / TOML would produce.
    config.active_packs.sort_by_key(|p| {
        PresetPackId::ALL
            .iter()
            .position(|x| x == p)
            .unwrap_or(usize::MAX)
    });
    ExclusionPolicy::compile(&config, &crate::exclusions::presets::BuiltinPresets).map_err(|e| {
        Error::ConfigInvalid {
            field: "exclusions",
            reason: format!("compile failed: {e}"),
        }
    })
}

fn parse_pack_id(s: &str) -> Result<crate::exclusions::PresetPackId> {
    use crate::exclusions::PresetPackId;
    // PresetPackId serialises as kebab-case (see serde rename
    // attribute on the enum) — accept that form on the CLI for
    // consistency with how it appears in TOML / `--list-
    // exclusion-packs` output.
    let kebab = s.trim().to_ascii_lowercase();
    let json = format!("\"{kebab}\"");
    serde_json::from_str::<PresetPackId>(&json).map_err(|_| Error::ConfigInvalid {
        field: "--exclusion-pack",
        reason: format!("unknown preset pack id {s:?} — see --list-exclusion-packs"),
    })
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

/// Per-disk-class default io-threads, derived from sdd-testwin's
/// cross-platform probes (iothreads-windows-probe-1 + 2; 2026-06-02).
///
/// Knee values are FIXED, not multiples of `cpu_threads`, because the
/// device queue-depth that wins parallelism is per-DEVICE, not
/// per-CPU.
const IO_THREADS_HDD: usize = 1;
const IO_THREADS_NVME: usize = 8;
const IO_THREADS_SSD: usize = 8;
const IO_THREADS_UNKNOWN_DEFAULT: usize = 8;
const IO_THREADS_WSL: usize = 1;

/// Compute the default `--io-threads` value when the user didn't pass
/// one explicitly.
///
/// PARKED 2026-06-02 (option (c) workload-aware default; awaiting Mick
/// GO per testdesign 09:33 PDT routing + design 09:33 PDT LGTM):
///
/// | Regime | Default | Source data |
/// |--------|---------|-------------|
/// | SUPERDEDUPER_IOTHREADS_PARKED env set | 1 | manual override (kept for symmetry) |
/// | WSL (`/proc/version` matches `microsoft`) | 1 | overnight WSL ext4 warm: io=1 wins 2.7-3.5x |
/// | `disk_class.contains("HDD")` | 1 | sdd-testwin probe-2: 1.4% span across io=1..64 on USB-HDD cold; io=1 marginally fastest + zero pool overhead |
/// | `disk_class.contains("NVMe")` | 8 | sdd-testwin probe-1: knee at io=4..16; 8 is the midpoint |
/// | `disk_class.contains("SSD")` (SATA/USB) | 8 | not separately measured; defaulting to NVMe-shape (queue depth helps; not seek-bottlenecked) |
/// | unknown class / `None` | 8 | conservative-by-perf: io=1 is 3x worse on NVMe (the dominant Windows hardware), so unknowns get NVMe-shape rather than HDD-shape |
///
/// Trade-offs documented in workdirs/design/profile-data.md addenda
/// 3 + 4 (2026-06-02). The env-var-gate (PARKED commit 89e9bb6)
/// stays for users who want to manually pick io=1.
///
/// Detection routes through the existing leaderboard hardware probe
/// (`detect_with_root_hint`) when the `telemetry` feature is on; when
/// off, falls through to `IO_THREADS_UNKNOWN_DEFAULT` (8).
///
/// Misclassification gotcha (per design 09:33 PDT note): a USB-HDD
/// through an older SAT pass-through bridge can classify as USB-SSD
/// and get io=8 instead of io=1. The actual perf impact is ~zero
/// because the HDD is seek-bottlenecked at the device level
/// regardless of thread count (1.4% wall span per sdd-testwin); the
/// "wrong" thread count only costs per-thread CPU efficiency, not
/// wall.
fn default_io_threads(cpu_threads: usize, first_root: Option<&PathBuf>) -> usize {
    #[cfg(feature = "telemetry")]
    let disk_class = Some(
        crate::leaderboard::hardware::detect_with_root_hint(first_root.map(|p| p.as_path()))
            .disk_class,
    );
    #[cfg(not(feature = "telemetry"))]
    let disk_class: Option<String> = {
        let _ = first_root;
        None
    };
    compute_default_io_threads(cpu_threads, disk_class.as_deref())
}

/// True when running under WSL. Reads `/proc/version` for the
/// `microsoft` marker; cached via OnceLock at first call. False on
/// non-Linux targets.
#[cfg(target_os = "linux")]
fn is_wsl_runtime() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        std::fs::read_to_string("/proc/version")
            .map(|s| s.to_lowercase().contains("microsoft"))
            .unwrap_or(false)
    })
}
#[cfg(not(target_os = "linux"))]
fn is_wsl_runtime() -> bool {
    false
}

/// Outer testable layer: pulls the WSL flag from runtime + delegates
/// to the pure decision-logic inner. Splitting lets tests target the
/// decision logic without depending on whether the test host happens
/// to be WSL or not.
fn compute_default_io_threads(cpu_threads: usize, disk_class: Option<&str>) -> usize {
    compute_default_io_threads_inner(cpu_threads, disk_class, is_wsl_runtime())
}

/// Pure / testable inner of [`compute_default_io_threads`]. Tests
/// pass `is_wsl` explicitly + thereby cover both branches independent
/// of the actual host.
fn compute_default_io_threads_inner(
    cpu_threads: usize,
    disk_class: Option<&str>,
    is_wsl: bool,
) -> usize {
    let _ = cpu_threads; // kept in signature for forward compat
    if std::env::var("SUPERDEDUPER_IOTHREADS_PARKED").is_ok() {
        return 1;
    }
    if is_wsl {
        return IO_THREADS_WSL;
    }
    match disk_class {
        Some(c) if c.contains("HDD") => IO_THREADS_HDD,
        Some(c) if c.contains("NVMe") => IO_THREADS_NVME,
        Some(c) if c.contains("SSD") => IO_THREADS_SSD,
        _ => IO_THREADS_UNKNOWN_DEFAULT,
    }
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
            no_cache: false,
            no_format_aware: false,
            threads: None,
            io_threads: None,
            output: None,
            follow_links: false,
            allow_system_paths: false,
            force_mft: false,
            parallel_roots: false,
            placeholders_only: false,
            force_hash: false,
            cold_enforced: false,
            allow_recall_on_read: false,
            hash_algo: HashAlgoArg::River5,
            mode: crate::cli::ScanMode::Exact,
            image_similarity_threshold: crate::cli::ImageSimilarityThresholdArg::Fixed(10),
            image_hash_algorithm: crate::cli::ImageHashAlgoArg::Phash,
            audio_similarity_threshold: 5.0,
            exclusions: crate::cli::ExclusionsToggle::On,
            exclusion_pack_disable: vec![],
            exclusion_pack_enable: vec![],
            list_exclusion_packs: false,
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

    // Option (c) 2026-06-02: tests below match the new per-disk-class
    // table. Data source: sdd-testwin iothreads-windows-probe-1 + 2.
    // See workdirs/design/profile-data.md addenda 3 + 4.

    #[test]
    fn io_threads_default_is_8_on_nvme_class() {
        // sdd-testwin probe-1 (Windows NVMe cold+warm): knee at io=4..16;
        // testdesign picked io=8 as the midpoint. Fixed value, not a
        // function of cpu_threads -- queue depth is per-device.
        assert_eq!(
            compute_default_io_threads_inner(8, Some("NVMe-Gen4"), false),
            IO_THREADS_NVME,
        );
        assert_eq!(
            compute_default_io_threads_inner(32, Some("NVMe-Gen5"), false),
            IO_THREADS_NVME,
            "high cpu count still gets the per-device knee (not cpu*3)"
        );
    }

    #[test]
    fn io_threads_default_is_8_on_ssd_classes() {
        // SATA-SSD / USB-SSD not separately measured; default to
        // NVMe-shape (queue depth helps; not seek-bottlenecked).
        // Bracketed by testdesign 09:33 PDT.
        assert_eq!(
            compute_default_io_threads_inner(8, Some("SATA-SSD"), false),
            IO_THREADS_SSD,
        );
        assert_eq!(
            compute_default_io_threads_inner(8, Some("USB-SSD"), false),
            IO_THREADS_SSD,
        );
    }

    #[test]
    fn io_threads_default_is_8_on_unknown_class() {
        // Conservative-by-perf: io=1 is 3x worse on the dominant
        // Windows hardware (NVMe). Unknown defaults to NVMe-shape.
        assert_eq!(
            compute_default_io_threads_inner(8, None, false),
            IO_THREADS_UNKNOWN_DEFAULT,
            "no probe (None) -> NVMe-shape default (8), not legacy threads*3"
        );
        assert_eq!(
            compute_default_io_threads_inner(8, Some("network"), false),
            IO_THREADS_UNKNOWN_DEFAULT,
            "unrecognized class -> NVMe-shape default"
        );
        assert_eq!(
            compute_default_io_threads_inner(8, Some("mixed"), false),
            IO_THREADS_UNKNOWN_DEFAULT,
        );
    }

    #[test]
    fn io_threads_default_is_1_on_hdd_class() {
        // sdd-testwin probe-2 (Windows USB-HDD cold via v0.3.30
        // --cold-enforced): 1.4% wall span across io=1/4/16/64.
        // io=1 marginally fastest + zero pool overhead.
        assert_eq!(
            compute_default_io_threads_inner(8, Some("HDD"), false),
            IO_THREADS_HDD,
        );
        assert_eq!(
            compute_default_io_threads_inner(32, Some("USB-HDD"), false),
            IO_THREADS_HDD,
            "USB-HDD treated as HDD; cpu_threads ignored",
        );
    }

    #[test]
    fn io_threads_default_is_1_on_wsl() {
        // WSL override engages regardless of disk_class -- WSL warm
        // regime is the more dominant signal than the underlying
        // virtual disk shape (which misdetects as HDD anyway per
        // yesterday's 71857b2 fix; "WSL2" string now bypasses the
        // HDD branch BUT we also want the explicit is_wsl override
        // so that any future disk_class detection change can't
        // accidentally re-engage HDD-shape thread counts here).
        assert_eq!(
            compute_default_io_threads_inner(8, Some("WSL2"), true),
            IO_THREADS_WSL,
        );
        assert_eq!(
            compute_default_io_threads_inner(8, Some("NVMe-Gen4"), true),
            IO_THREADS_WSL,
            "WSL flag overrides even if disk_class detects NVMe (defensive)"
        );
        assert_eq!(
            compute_default_io_threads_inner(8, None, true),
            IO_THREADS_WSL,
        );
    }

    #[test]
    fn io_threads_default_is_fixed_regardless_of_cpu_count() {
        // The whole point of the new table: per-DEVICE knee, not a
        // function of cpu_threads. Low-thread and high-thread machines
        // get the same per-class default.
        for cpu in [1usize, 2, 4, 8, 16, 32, 64, 128] {
            assert_eq!(
                compute_default_io_threads_inner(cpu, Some("NVMe-Gen4"), false),
                IO_THREADS_NVME,
                "NVMe default is cpu-independent (cpu={cpu})"
            );
            assert_eq!(
                compute_default_io_threads_inner(cpu, Some("HDD"), false),
                IO_THREADS_HDD,
                "HDD default is cpu-independent (cpu={cpu})"
            );
        }
    }

    #[test]
    fn io_threads_explicit_override_respected() {
        let mut a = args_with_paths(vec![PathBuf::from("/tmp")]);
        a.threads = Some(4);
        a.io_threads = Some(99);
        let cfg = ScanConfig::from_args(&a).unwrap();
        assert_eq!(
            cfg.io_threads, 99,
            "--io-threads explicit always wins, even past the HDD cap"
        );
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
