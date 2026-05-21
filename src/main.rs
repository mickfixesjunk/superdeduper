use std::io::{self, BufWriter, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use parking_lot::Mutex;
use tracing_subscriber::EnvFilter;

use superdupe::cache::Cache;
use superdupe::cli::{CacheCommand, Cli, Command, DedupeArgs, ScanArgs};
use superdupe::config::ScanConfig;
use superdupe::{dedupe, inventory, output, pipeline};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    match cli.command {
        Command::Scan(args) => run_scan(args),
        Command::Dedupe(args) => run_dedupe(args),
        Command::Cache(cmd) => run_cache(cmd),
        Command::DriveInfo => run_drive_info(),
    }
}

/// Print one block per drive Windows can see, enumerating the raw
/// storage-detection inputs (bus type number + name, seek-penalty
/// IOCTL result, partition + device numbers) plus the rule we used
/// to pick HDD vs SSD. Designed to be pasted verbatim when a drive
/// gets misclassified — the data is all in one place.
fn run_drive_info() -> anyhow::Result<()> {
    #[cfg(not(windows))]
    {
        eprintln!("drive-info is Windows-only.");
        Ok(())
    }
    #[cfg(windows)]
    {
        use std::path::PathBuf;
        use superdupe::winapi_wrappers::{bus_type_name, query_storage_device, volume_for_path};
        // Enumerate drive letters A..Z; skip any that GetDriveTypeW
        // says aren't real fixed/removable/network drives. Probing
        // is per-letter to avoid pulling in another IOCTL.
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            let path = PathBuf::from(&root);
            if !path.exists() {
                continue;
            }
            // Skip CD-ROM / unknown / no-root-dir drives by checking
            // GetDriveTypeW. We don't have a wrapper for it; query
            // via the volume_for_path probe which fails on those.
            let volume = match volume_for_path(&path) {
                Ok(v) => v,
                Err(e) => {
                    println!("=== {root} ===\n  (skipped: {e})\n");
                    continue;
                }
            };
            println!("=== {root} ===");
            println!("  volume guid:        {volume}");
            match query_storage_device(&volume) {
                Ok(info) => {
                    println!("  device #:           {}", info.device_number);
                    println!("  partition #:        {}", info.partition_number);
                    println!(
                        "  bus type:           {} (0x{:02x}) — {}",
                        info.bus_type,
                        info.bus_type,
                        bus_type_name(info.bus_type),
                    );
                    println!(
                        "  seek-penalty IOCTL: {}",
                        match info.seek_penalty_ioctl {
                            Some(true) => "Yes (says HDD)",
                            Some(false) => "No (says SSD)",
                            None => "FAILED (IOCTL didn't answer)",
                        }
                    );
                    println!(
                        "  classified as:      {}",
                        if info.has_seek_penalty { "HDD" } else { "SSD" }
                    );
                    println!("  reason:             {}", info.classification_reason);
                }
                Err(e) => {
                    println!("  (query_storage_device failed: {e})");
                }
            }
            println!();
        }
        Ok(())
    }
}

fn run_scan(args: ScanArgs) -> anyhow::Result<()> {
    let cfg = ScanConfig::from_args(&args).context("invalid scan configuration")?;

    let scan_started = std::time::Instant::now();
    // Tell the user which content-hash core is actually linked. For
    // BLAKE3 this is just the compile-time crate version; for
    // River5 it's whatever the upstream lib reports via
    // `impl_name()` — `river5-stub-xxh3` vs `river5-aesni-v2`
    // tells you definitively whether the AES-NI core is live.
    let hash_impl: &str = match cfg.hash_algo {
        crate::pipeline::hash::HashAlgo::Blake3 => "blake3 (Rust crate)",
        crate::pipeline::hash::HashAlgo::River5 => river5::impl_name(),
    };
    tracing::info!(
        roots = ?cfg.roots,
        threads = cfg.threads,
        min_size = cfg.min_size,
        format_aware = cfg.use_format_aware,
        cache = cfg.use_cache,
        hash_algo = cfg.hash_algo.tag(),
        hash_impl,
        "starting scan",
    );
    // Also surface it in the stderr timing block (which lands at WARN
    // level by default) so users running with --quiet still see it.
    eprintln!("hash impl: {hash_impl}");

    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads)
        .build_global()
    {
        tracing::debug!(error = %e, "rayon global pool already initialized; keeping existing");
    }

    // Open the cache up front. `--no-cache` is about benchmarking
    // clean hash throughput — it should NOT also disable the
    // inventory-snapshot warm path (which is orthogonal: it speeds
    // up Stage 1, not Stage 4). So we always try to open; only the
    // Stage 4 hash-lookup decision below gates on `cfg.use_cache`.
    // If the open itself fails (permissions, disk full, …) we
    // surrender both and fall back to fully cacheless behaviour.
    let cache = match superdupe::cache::default_cache_path().and_then(|p| Cache::open(&p)) {
        Ok(c) => Some(Arc::new(Mutex::new(c))),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "cache disabled (couldn't open) — both warm-path inventory and hash cache off"
            );
            None
        }
    };

    let t_inventory = std::time::Instant::now();
    let inventory = inventory::enumerate(&cfg, cache.as_ref()).context("inventory failed")?;
    let inventory_ms = t_inventory.elapsed().as_millis();
    tracing::info!(
        count = inventory.len(),
        elapsed_ms = inventory_ms as u64,
        "stage 1: inventory complete"
    );

    let t_group = std::time::Instant::now();
    let size_groups = pipeline::grouping::group_by_size(inventory);
    let group_ms = t_group.elapsed().as_millis();
    tracing::info!(
        groups = size_groups.len(),
        elapsed_ms = group_ms as u64,
        "stage 2: size grouping complete"
    );

    let t_layout = std::time::Instant::now();
    let laid_out = pipeline::layout::resolve(size_groups).context("layout resolution failed")?;
    let layout_ms = t_layout.elapsed().as_millis();
    tracing::info!(
        groups = laid_out.len(),
        elapsed_ms = layout_ms as u64,
        "stage 3: layout resolution complete"
    );

    // Stage 4 hash cache: only handed to the hasher when --no-cache
    // wasn't passed. The cache handle itself stays alive (the warm
    // path snapshot above used it, and the cold MFT fallback may
    // still need to write the snapshot in `mft::enumerate`).
    let hash_cache = if cfg.use_cache { cache.clone() } else { None };
    let t_hash = std::time::Instant::now();
    let (duplicates, counters) =
        pipeline::hash::run_with_counters(laid_out, &cfg, hash_cache).context("hashing failed")?;
    let hash_ms = t_hash.elapsed().as_millis();
    tracing::info!(
        groups = duplicates.len(),
        cache_hits = counters.cache_hits.load(Ordering::Relaxed),
        cache_writes = counters.cache_writes.load(Ordering::Relaxed),
        bytes_read = counters.bytes_read.load(Ordering::Relaxed),
        elapsed_ms = hash_ms as u64,
        "stage 4: hashing complete"
    );
    // Per-tier breakdown — shows whether the time gap between two
    // hash algorithms is in Tier 1 (per-file FFI overhead) or Tier 3
    // (bulk throughput).
    let mut stderr = io::stderr().lock();
    let _ = writeln!(
        stderr,
        "\n--- timing ({}) ---\n\
         stage 1 inventory:    {:>6} ms ({} files)\n\
         stage 2 grouping:     {:>6} ms\n\
         stage 3 layout:       {:>6} ms\n\
         stage 4 hashing:      {:>6} ms (wallclock) — bytes_read={}",
        cfg.hash_algo.tag(),
        inventory_ms,
        counters.cache_hits.load(Ordering::Relaxed)
            + counters.tier_count[1].load(Ordering::Relaxed),
        group_ms,
        layout_ms,
        hash_ms,
        humansize::format_size(
            counters.bytes_read.load(Ordering::Relaxed),
            humansize::BINARY
        ),
    );
    // CPU-summed time = wall-clock time spent in the per-file
    // open+read+hash closure, summed across all rayon workers. For
    // tiers that read small per-file payloads (Tier 0, Tier 1), most
    // of this is the open() syscall, NTFS metadata fetch and OS
    // cache lookup — not the hash compute. So the "MB/s/thread" is
    // an *effective* throughput including I/O overhead, which is
    // what users actually feel; bulk-only hash throughput is what
    // Tier 3 approaches once the per-file overhead is amortised.
    let _ = writeln!(
        stderr,
        "  (per-tier CPU-summed includes file open + read + hash; \
         compare effective MB/s side-by-side across algos)"
    );
    for (i, tier_name) in ["Tier 0 fmt ", "Tier 1 head", "Tier 2 hmt ", "Tier 3 full"]
        .iter()
        .enumerate()
    {
        let micros = counters.tier_micros[i].load(Ordering::Relaxed);
        let count = counters.tier_count[i].load(Ordering::Relaxed);
        let bytes = counters.tier_bytes[i].load(Ordering::Relaxed);
        if count == 0 && micros == 0 {
            continue;
        }
        let cpu_ms = micros / 1000;
        // bytes per microsecond happens to read as MB/s.
        let mbps = if micros == 0 {
            0.0
        } else {
            bytes as f64 / micros as f64
        };
        // files per second across the summed-CPU window — for
        // small-payload tiers this is the more honest metric: it
        // tells you how fast each worker is churning through file
        // open+read cycles.
        let files_per_s = if micros == 0 {
            0.0
        } else {
            count as f64 / (micros as f64 / 1_000_000.0)
        };
        let _ = writeln!(
            stderr,
            "  {tier_name}: {count:>6} files · {:>10} hashed · {:>7} ms CPU-summed · {:>8.2} MB/s/thread · {:>7.0} files/s/thread",
            humansize::format_size(bytes, humansize::BINARY),
            cpu_ms,
            mbps,
            files_per_s,
        );
    }
    let _ = writeln!(
        stderr,
        "total wallclock:      {:>6} ms",
        scan_started.elapsed().as_millis()
    );

    let duplicates = if cfg.paranoid {
        pipeline::confirm::paranoid_verify(duplicates).context("paranoid verification failed")?
    } else {
        duplicates
    };

    let mut writer: Box<dyn Write> = match &cfg.output {
        Some(p) => Box::new(BufWriter::new(
            std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?,
        )),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };
    output::write(writer.as_mut(), cfg.format, &duplicates)?;
    writer.flush()?;

    Ok(())
}

fn run_dedupe(args: DedupeArgs) -> anyhow::Result<()> {
    let outcome = dedupe::run(&args).context("dedupe failed")?;
    let mut stderr = io::stderr().lock();
    writeln!(
        stderr,
        "Planned: {} · executed: {} · skipped (reference): {} · skipped (system): {} · skipped (changed): {} · failed: {}",
        outcome.planned,
        outcome.executed,
        outcome.skipped_reference,
        outcome.skipped_system,
        outcome.skipped_invalidated,
        outcome.failed,
    )?;
    writeln!(
        stderr,
        "Reclaimed (planned): {}",
        humansize::format_size(outcome.bytes_reclaimed, humansize::BINARY),
    )?;
    if outcome.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn run_cache(cmd: CacheCommand) -> anyhow::Result<()> {
    let path = superdupe::cache::default_cache_path()?;
    let cache = Cache::open(&path).context("opening cache database")?;
    match cmd {
        CacheCommand::Info => {
            let stats = cache.stats(&path).context("reading cache stats")?;
            println!("path:           {}", stats.path.display());
            println!("rows:           {}", stats.rows);
            println!(
                "size on disk:   {}",
                humansize::format_size(stats.bytes_on_disk, humansize::BINARY)
            );
        }
        CacheCommand::Clear => {
            cache.clear()?;
            tracing::info!(path = %path.display(), "cache cleared");
        }
        CacheCommand::Vacuum => {
            cache.vacuum()?;
            tracing::info!(path = %path.display(), "cache compacted");
        }
    }
    Ok(())
}

fn init_logging(verbose: u8, quiet: bool) {
    let filter = if quiet {
        EnvFilter::new("error")
    } else if let Ok(env) = std::env::var("SUPERDUPE_LOG") {
        EnvFilter::new(env)
    } else {
        EnvFilter::new(match verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        })
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(io::stderr)
        .init();
}
