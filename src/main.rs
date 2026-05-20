use std::io::{self, BufWriter, Write};

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use superdupe::cli::{Cli, Command, ScanArgs};
use superdupe::config::ScanConfig;
use superdupe::{inventory, output, pipeline};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    match cli.command {
        Command::Scan(args) => run_scan(args),
        Command::Dedupe(_) => {
            anyhow::bail!("`dedupe` is not yet implemented in this build");
        }
        Command::Cache(_) => {
            anyhow::bail!("`cache` is not yet implemented in this build");
        }
    }
}

fn run_scan(args: ScanArgs) -> anyhow::Result<()> {
    let cfg = ScanConfig::from_args(&args).context("invalid scan configuration")?;

    tracing::info!(
        roots = ?cfg.roots,
        threads = cfg.threads,
        min_size = cfg.min_size,
        "starting scan",
    );

    // Apply the requested rayon thread pool size globally. Failure here is
    // not fatal — rayon falls back to its default and we log once.
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

    let duplicates = pipeline::hash::run(laid_out).context("hashing failed")?;
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
