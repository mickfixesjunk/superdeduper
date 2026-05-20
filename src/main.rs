use std::io::{self, BufWriter, Write};

use anyhow::Context;
use clap::Parser;
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
    }
}

fn run_scan(args: ScanArgs) -> anyhow::Result<()> {
    let cfg = ScanConfig::from_args(&args).context("invalid scan configuration")?;

    tracing::info!(
        roots = ?cfg.roots,
        threads = cfg.threads,
        min_size = cfg.min_size,
        format_aware = cfg.use_format_aware,
        cache = cfg.use_cache,
        "starting scan",
    );

    if let Err(e) = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.threads)
        .build_global()
    {
        tracing::debug!(error = %e, "rayon global pool already initialized; keeping existing");
    }

    let inventory = inventory::enumerate(&cfg).context("inventory failed")?;
    tracing::info!(count = inventory.len(), "stage 1: inventory complete");

    let size_groups = pipeline::grouping::group_by_size(inventory);
    tracing::info!(groups = size_groups.len(), "stage 2: size grouping complete");

    let laid_out = pipeline::layout::resolve(size_groups).context("layout resolution failed")?;
    tracing::info!(groups = laid_out.len(), "stage 3: layout resolution complete");

    let duplicates = pipeline::hash::run(laid_out, &cfg).context("hashing failed")?;
    tracing::info!(groups = duplicates.len(), "stage 4: hashing complete");

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
