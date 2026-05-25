use std::io::{self, BufWriter, Write};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use parking_lot::Mutex;
use tracing_subscriber::EnvFilter;

use superdeduper::cache::Cache;
use superdeduper::cli::{CacheCommand, Cli, Command, DedupeArgs, ScanArgs};
use superdeduper::config::ScanConfig;
use superdeduper::{dedupe, inventory, output, pipeline};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_logging(cli.verbose, cli.quiet);

    // Channel resolution: --channel flag > SUPERDEDUPER_CHANNEL env var
    // > [network] channel in persisted config > default `prod`. Set
    // once at startup so every downstream consumer (install path,
    // submit URL, GUI banner, CLI footer) reads a consistent value
    // for the duration of this invocation.
    let active = superdeduper::channel::resolve_active_channel(cli.channel.as_deref())
        .map_err(|e| anyhow::anyhow!("--channel / SUPERDEDUPER_CHANNEL / config.toml: {e}"))?;
    superdeduper::channel::set_active_channel(active);

    let result = dispatch(cli.command);

    // Per dev-channel-spec.md §5.5: every CLI command on non-prod
    // prints a footer line so the user is reminded which environment
    // their action just hit. Goes to stderr so it doesn't corrupt
    // stdout-piped JSON/CSV. Suppressed when --quiet (we suppress
    // every non-error stream in that mode).
    if active.is_non_prod() && !cli.quiet && result.is_ok() {
        eprintln!(
            "(channel: {} — submissions go to {})",
            active,
            superdeduper::channel::server_url_for(active)
        );
    }

    result
}

fn dispatch(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Scan(args) => run_scan(args),
        Command::Dedupe(args) => run_dedupe(args),
        Command::Cache(cmd) => run_cache(cmd),
        Command::DriveInfo => run_drive_info(),
        Command::Diagnose(args) => superdeduper::diagnose::run(args),
        Command::Debug(cmd) => run_debug(cmd),
        #[cfg(feature = "telemetry")]
        Command::Register(args) => run_register(args),
        #[cfg(feature = "telemetry")]
        Command::Config(cmd) => run_config(cmd),
        #[cfg(feature = "telemetry")]
        Command::Achievements(cmd) => run_achievements(cmd),
        #[cfg(feature = "telemetry")]
        Command::Account(cmd) => run_account(cmd),
        Command::ScanHistory(cmd) => run_scan_history(cmd),
    }
}

/// #38 v1 — CLI for inspecting + pruning the local scan history.
/// Cross-platform; reads the same JSON files the GUI History tab
/// surfaces.
fn run_scan_history(cmd: superdeduper::cli::ScanHistoryCommand) -> anyhow::Result<()> {
    use superdeduper::cli::{OutputFormat, ScanHistoryCommand};
    use superdeduper::scan_history;

    match cmd {
        ScanHistoryCommand::List { format } => {
            let records = scan_history::list()?;
            match format {
                OutputFormat::Json => {
                    let json = serde_json::to_string_pretty(&records)?;
                    println!("{json}");
                }
                OutputFormat::Csv => {
                    // Light-touch CSV — id, ts, channel, files,
                    // bytes_read, dups, reclaim, state. Mirrors the
                    // record's user-relevant fields.
                    println!("scan_id,started_at_unix,channel,total_files,total_bytes_read,total_dups,reclaimable_bytes,submission_state");
                    for r in &records {
                        println!(
                            "{},{},{},{},{},{},{},{:?}",
                            r.scan_id,
                            r.started_at_unix,
                            r.channel,
                            r.total_files,
                            r.total_bytes_read,
                            r.total_dups,
                            r.reclaimable_bytes,
                            r.submission_state,
                        );
                    }
                }
                OutputFormat::Text | OutputFormat::Report => {
                    if records.is_empty() {
                        println!("No scans recorded.");
                        return Ok(());
                    }
                    println!(
                        "{:32}  {:10}  {:6}  {:>8}  {:>10}  {:>10}  state",
                        "scan_id", "channel", "files", "dups", "reclaim", "started",
                    );
                    for r in &records {
                        println!(
                            "{:32}  {:10}  {:>6}  {:>8}  {:>10}  {:>10}  {:?}",
                            r.scan_id,
                            r.channel,
                            r.total_files,
                            r.total_dups,
                            humansize::format_size(r.reclaimable_bytes, humansize::BINARY),
                            r.started_at_unix,
                            r.submission_state,
                        );
                    }
                }
            }
            Ok(())
        }
        ScanHistoryCommand::Delete { scan_id } => {
            scan_history::delete(&scan_id)?;
            println!("scan-history: removed (or absent) {scan_id}");
            Ok(())
        }
    }
}

/// G3: `superdeduper account link <provider>` / `account unlink` /
/// `account status`. Per `gamification-client-spec.md` §10.3 +
/// Mick's 2026-05-24T22:14:51Z directive.
#[cfg(feature = "telemetry")]
fn run_account(cmd: superdeduper::cli::AccountCommand) -> anyhow::Result<()> {
    use std::str::FromStr;
    use superdeduper::channel;
    use superdeduper::cli::{AccountCommand, OutputFormat};
    use superdeduper::leaderboard::{install, oauth};

    let active = channel::active_channel();
    match cmd {
        AccountCommand::Link {
            provider,
            timeout_secs,
        } => {
            let provider =
                oauth::Provider::from_str(&provider).map_err(|e| anyhow::anyhow!("{e}"))?;
            let install_state = install::load()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "not registered on channel `{}` — run `superdeduper register --channel {}` first",
                    active,
                    active,
                )
            })?;
            let server_url = channel::server_url_for(active);
            println!(
                "Opening browser to link {} (channel: {}). \
                 Complete the OAuth flow in your browser; this CLI \
                 will exit once the callback arrives or after \
                 {timeout_secs}s.",
                provider.display_name(),
                active,
            );
            let token = oauth::link_via_loopback(
                provider,
                active,
                server_url,
                &install_state.install_id,
                std::time::Duration::from_secs(timeout_secs),
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            println!(
                "Linked: {} ({}) on channel `{}`. Token stored at {}.",
                token.display_name,
                token.provider.display_name(),
                active,
                oauth::oauth_path()?.display(),
            );
            Ok(())
        }
        AccountCommand::Unlink => {
            let prior = oauth::status()?;
            oauth::unlink_for(active)?;
            match prior {
                oauth::AccountStatus::Anonymous => {
                    println!("No account was linked on channel `{}`.", active);
                }
                oauth::AccountStatus::Linked {
                    provider,
                    display_name,
                    ..
                } => {
                    println!(
                        "Unlinked {} ({}) from channel `{}`. Token file removed.",
                        display_name,
                        provider.display_name(),
                        active,
                    );
                }
            }
            Ok(())
        }
        AccountCommand::Status { format } => {
            let s = oauth::status()?;
            let install_state = install::load()?;
            let install_id = install_state
                .as_ref()
                .map(|s| s.install_id.as_str())
                .unwrap_or("<not registered>");
            match format {
                OutputFormat::Json => {
                    let payload = match &s {
                        oauth::AccountStatus::Anonymous => serde_json::json!({
                            "channel": active.to_string(),
                            "install_id": install_id,
                            "linked": false,
                        }),
                        oauth::AccountStatus::Linked {
                            provider,
                            display_name,
                            account_id,
                        } => serde_json::json!({
                            "channel": active.to_string(),
                            "install_id": install_id,
                            "linked": true,
                            "provider": provider.as_slug(),
                            "display_name": display_name,
                            "account_id": account_id,
                        }),
                    };
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&payload).unwrap_or_default()
                    );
                }
                // Report falls back to Text for account status —
                // there's no markdown table shape to emit.
                OutputFormat::Text | OutputFormat::Csv | OutputFormat::Report => match s {
                    oauth::AccountStatus::Anonymous => {
                        println!("channel:    {}", active);
                        println!("install_id: {}", install_id);
                        println!("account:    Anonymous (use `superdeduper account link google` or `… link discord` to claim achievements across machines)");
                    }
                    oauth::AccountStatus::Linked {
                        provider,
                        display_name,
                        account_id,
                    } => {
                        println!("channel:      {}", active);
                        println!("install_id:   {}", install_id);
                        println!(
                            "account:      Linked — {} ({})",
                            display_name,
                            provider.display_name()
                        );
                        println!("account_id:   {}", account_id);
                    }
                },
            }
            Ok(())
        }
    }
}

/// `superdeduper debug snapshot` — emit the canonical containment-
/// test snapshot for `<path>` as JSON. Used by sdd-testwin +
/// testrunner to capture pre/post state around an action under test.
fn run_debug(cmd: superdeduper::cli::DebugCommand) -> anyhow::Result<()> {
    use superdeduper::cli::{DebugCommand, SnapshotFormat};
    use superdeduper::debug::snapshot;
    match cmd {
        DebugCommand::Snapshot { path, format, out } => {
            let SnapshotFormat::Json = format;
            let snap =
                snapshot::capture(&path).with_context(|| format!("snapshot {:?} failed", path))?;
            match out {
                Some(file) => {
                    let f = std::fs::File::create(&file)
                        .with_context(|| format!("create {:?}", file))?;
                    let mut w = BufWriter::new(f);
                    snapshot::write_json(&snap, &mut w)?;
                    w.flush()?;
                }
                None => {
                    let stdout = io::stdout();
                    let mut handle = stdout.lock();
                    snapshot::write_json(&snap, &mut handle)?;
                }
            }
            Ok(())
        }
    }
}

/// G-track: `superdeduper achievements` — list / refetch the install's
/// achievement state. Triage tool: pair with the GUI when the badge
/// wall looks wrong.
#[cfg(feature = "telemetry")]
fn run_achievements(cmd: superdeduper::cli::AchievementsCommand) -> anyhow::Result<()> {
    use superdeduper::cli::AchievementsCommand;
    use superdeduper::leaderboard::{catalog, install};

    let state = match install::load()? {
        Some(s) if s.registered => s,
        Some(_) => {
            anyhow::bail!(
                "not registered yet — run `superdeduper register` first to enable achievement tracking"
            );
        }
        None => {
            anyhow::bail!("no install.json found — run `superdeduper register` to create one");
        }
    };

    match cmd {
        AchievementsCommand::Refetch { quiet } => {
            let profile = catalog::fetch_profile_fresh(&state.server_url, &state.install_id)
                .map_err(|e| anyhow::anyhow!("profile fetch failed: {e:?}"))?;
            catalog::set_profile(Ok(profile.clone()));
            if !quiet {
                let granted = profile.achievements.iter().filter(|g| g.granted).count();
                let total = profile.achievements.len();
                println!(
                    "Refetched: {granted}/{total} granted; lifetime_reclaimed_bytes={}",
                    profile.lifetime_reclaimed_bytes()
                );
            }
            Ok(())
        }
        AchievementsCommand::List { format, all } => {
            // List reads from the local cache. If the slot hasn't
            // been populated by spawn_initial_fetch (CLI doesn't run
            // the GUI app start path), we fetch once on-demand.
            let cat_state = catalog::peek_state();
            let catalog_data = match cat_state.catalog {
                Some(Ok(c)) => c,
                _ => catalog::fetch_catalog(&state.server_url)
                    .map_err(|e| anyhow::anyhow!("catalog fetch failed: {e:?}"))?,
            };
            let profile = match cat_state.profile {
                Some(Ok(p)) => p,
                _ => catalog::fetch_profile_fresh(&state.server_url, &state.install_id)
                    .map_err(|e| anyhow::anyhow!("profile fetch failed: {e:?}"))?,
            };
            print_achievements(&catalog_data, &profile, format, all);
            Ok(())
        }
        AchievementsCommand::Verify { format, quiet } => {
            // Verify reads from the local cache + fetches if absent
            // (mirrors List). Always prints the audit on success
            // (regardless of `all`-style filter — verify is the
            // "show me everything" flavour). Then bumps the local
            // invocation counter; reaching 10 unlocks the
            // `verify-veteran` predicate on the next scan.
            let cat_state = catalog::peek_state();
            let catalog_data = match cat_state.catalog {
                Some(Ok(c)) => c,
                _ => catalog::fetch_catalog(&state.server_url)
                    .map_err(|e| anyhow::anyhow!("catalog fetch failed: {e:?}"))?,
            };
            let profile = match cat_state.profile {
                Some(Ok(p)) => p,
                _ => catalog::fetch_profile_fresh(&state.server_url, &state.install_id)
                    .map_err(|e| anyhow::anyhow!("profile fetch failed: {e:?}"))?,
            };
            if !quiet {
                print_achievements(&catalog_data, &profile, format, /* all = */ true);
            }
            // Always bump — even in --quiet mode the counter ticks
            // (per the predicate spec: "Engine increments a local
            // counter on each `achievements verify` invocation.").
            install::bump_achievements_verify_invocations()
                .map_err(|e| anyhow::anyhow!("counter bump failed: {e}"))?;
            // Re-load to read the post-bump value for the user-
            // visible "you're at N/10" hint.
            if !quiet {
                if let Some(post) = install::load()? {
                    let n = post.counters.achievements_verify_invocations;
                    if n < 10 {
                        println!("\n(verify invocation #{n}; verify-veteran unlocks at 10.)");
                    } else {
                        println!(
                            "\n(verify invocation #{n}; verify-veteran qualifies — will grant on next scan submit.)"
                        );
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(feature = "telemetry")]
fn print_achievements(
    catalog: &superdeduper::leaderboard::catalog::Catalog,
    profile: &superdeduper::leaderboard::catalog::Profile,
    format: superdeduper::cli::OutputFormat,
    all: bool,
) {
    use std::collections::HashMap;
    use superdeduper::cli::OutputFormat;

    let grants: HashMap<&str, &superdeduper::leaderboard::catalog::ProfileGrant> = profile
        .achievements
        .iter()
        .map(|g| (g.achievement_id.as_str(), g))
        .collect();

    let mut entries: Vec<_> = catalog.achievements.iter().collect();
    // Granted entries first (visual-test-friendly), then by
    // display_order. Matches the badge-wall ordering.
    entries.sort_by_key(|e| {
        let granted = grants
            .get(e.id.as_str())
            .map(|g| g.granted)
            .unwrap_or(false);
        (!granted, e.display_order)
    });

    match format {
        OutputFormat::Json => {
            let rows: Vec<serde_json::Value> = entries
                .iter()
                .filter_map(|e| {
                    let granted = grants
                        .get(e.id.as_str())
                        .map(|g| g.granted)
                        .unwrap_or(false);
                    if !all && !granted {
                        return None;
                    }
                    let granted_at = grants
                        .get(e.id.as_str())
                        .and_then(|g| g.granted_at.as_deref());
                    Some(serde_json::json!({
                        "id": e.id,
                        "name": e.name,
                        "tier": e.tier,
                        "unlock_kind": e.unlock_kind,
                        "granted": granted,
                        "granted_at": granted_at,
                    }))
                })
                .collect();
            let payload = serde_json::json!({
                "install_id": profile.install_id,
                "lifetime_reclaimed_bytes": profile.lifetime_reclaimed_bytes(),
                "lifetime_scans": profile.lifetime_scans(),
                "achievements": rows,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).unwrap_or_default()
            );
        }
        // Same fallback rationale as account status — Report has no
        // tabular shape that matches achievements list.
        OutputFormat::Text | OutputFormat::Csv | OutputFormat::Report => {
            println!("install_id: {}", profile.install_id);
            println!(
                "lifetime: {} bytes reclaimed across {} scans",
                profile.lifetime_reclaimed_bytes(),
                profile.lifetime_scans()
            );
            let granted_count = grants.values().filter(|g| g.granted).count();
            println!(
                "achievements: {}/{} granted{}",
                granted_count,
                entries.len(),
                if all {
                    " (showing all)"
                } else {
                    " (showing granted only; --all for full list)"
                }
            );
            println!();
            println!(
                "{:<28}  {:<6}  {:<22}  {}",
                "ID", "TIER", "GRANTED_AT", "NAME"
            );
            for e in entries {
                let grant = grants.get(e.id.as_str());
                let granted = grant.map(|g| g.granted).unwrap_or(false);
                if !all && !granted {
                    continue;
                }
                let marker = if granted { "✓" } else { " " };
                let at = grant.and_then(|g| g.granted_at.as_deref()).unwrap_or("-");
                println!(
                    "{marker} {:<26}  {:<6}  {:<22}  {}",
                    e.id, e.tier, at, e.name
                );
            }
        }
    }
}

/// G-track: `superdeduper register` — register this install with the
/// leaderboard backend. Idempotent.
#[cfg(feature = "telemetry")]
fn run_register(args: superdeduper::cli::RegisterArgs) -> anyhow::Result<()> {
    use superdeduper::leaderboard::{install, registration};

    // server_url derives from the active channel by default. The
    // legacy --server-url arg still works as an explicit override
    // (useful for testing against an ad-hoc backend), but the
    // common path is now `superdeduper register --channel dev` →
    // server_url resolves to https://dev-api.superdeduper.io
    // automatically. Per dev-channel-spec.md §5.1.
    let server_url = args.server_url.unwrap_or_else(|| {
        superdeduper::channel::server_url_for(superdeduper::channel::active_channel()).to_string()
    });

    // --reset rotates the install_id. Refuse without explicit opt-in
    // confirmation: prints what's about to happen + requires the
    // env var (CI / scripted) or stdin "y". For simplicity now,
    // require --reset to be a destructive opt-in by environment.
    let existing = install::load()?;
    let mut state = match (existing, args.reset) {
        (Some(s), false) => s,
        (Some(_), true) => {
            eprintln!(
                "warning: --reset wipes the prior install_id. \
                 Submissions linked to the old install_id stay on \
                 the leaderboard but will not be reachable from this \
                 client. Continuing in 3 seconds — Ctrl-C to abort..."
            );
            std::thread::sleep(std::time::Duration::from_secs(3));
            install::new_unregistered(server_url)
        }
        (None, _) => install::new_unregistered(server_url),
    };

    if state.registered {
        println!(
            "Already registered. install_id = {}\nProfile: https://superdeduper.io/profile/{}",
            state.install_id, state.install_id
        );
        return Ok(());
    }

    println!("First-time setup: registering this install. This takes ~1 second of CPU.");
    match registration::register_cli(&mut state) {
        Ok(()) => {
            println!(
                "Registered. install_id = {}\nProfile: https://superdeduper.io/profile/{}\nUse `superdeduper config show` to see current share preference.",
                state.install_id, state.install_id
            );
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("registration failed: {e:?}")),
    }
}

/// G-track: `superdeduper config show` / `superdeduper config set-share`.
#[cfg(feature = "telemetry")]
fn run_config(cmd: superdeduper::cli::ConfigCommand) -> anyhow::Result<()> {
    use superdeduper::cli::{ConfigCommand, ShareValue};
    use superdeduper::leaderboard::install::{self, ShareDefault};

    match cmd {
        ConfigCommand::Show => {
            let path = install::install_path()?;
            match install::load()? {
                Some(state) => {
                    println!("install.json:    {}", path.display());
                    println!("install_id:      {}", state.install_id);
                    println!("registered:      {}", state.registered);
                    println!("server_url:      {}", state.server_url);
                    println!("share_default:   {:?}", state.share_default);
                    println!(
                        "client_version_at_register: {}",
                        state.client_version_at_register
                    );
                }
                None => {
                    println!("install.json:    {} (not yet created)", path.display());
                    println!("status:          unregistered");
                    println!("Run `superdeduper register` to enroll this install.");
                }
            }
            // Per #38 v1 testrunner Gap 1 — surface the scan-history
            // directory so cross-platform path-resolution tests have
            // one canonical query for "where does this install write
            // scan records?" The dir might not exist yet (first scan
            // creates it), but the resolved path is computable
            // unconditionally.
            match superdeduper::scan_history::history_dir() {
                Ok(p) => println!("scan_history:    {}", p.display()),
                Err(e) => println!("scan_history:    <resolution error: {e}>"),
            }
            Ok(())
        }
        ConfigCommand::SetShare { value } => {
            let mut state = match install::load()? {
                Some(s) => s,
                None => {
                    return Err(anyhow::anyhow!(
                        "install.json not found — run `superdeduper register` first"
                    ))
                }
            };
            let new_value = match value {
                ShareValue::AlwaysAsk => ShareDefault::AlwaysAsk,
                ShareValue::AutoOptIn => ShareDefault::AutoOptIn,
                ShareValue::Never => ShareDefault::Never,
            };
            state.share_default = new_value;
            install::save(&state)?;
            println!("share_default = {:?}", state.share_default);
            Ok(())
        }
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
        use superdeduper::winapi_wrappers::{bus_type_name, query_storage_device, volume_for_path};
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

    // #25 T1.2 Tier-4 — perceptual image similarity. Threaded
    // through directly rather than added to ScanConfig because
    // Tier-4 runs on the inventory after Tier-3, separate from
    // the byte-identical pipeline state.
    let mode = args.mode;
    let image_similarity_threshold = args.image_similarity_threshold;

    let scan_started = std::time::Instant::now();
    // Wall-clock UNIX seconds for the scan_history record — same
    // pattern as `gui::live::run()` so CLI + GUI scans both persist
    // history rows with comparable timestamps.
    let started_at_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
    let cache = match superdeduper::cache::default_cache_path().and_then(|p| Cache::open(&p)) {
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
    let (inventory, skipped) =
        inventory::enumerate_with_skipped(&cfg, cache.as_ref()).context("inventory failed")?;
    let inventory_ms = t_inventory.elapsed().as_millis();
    // Capture #38 v1 history totals before `inventory` is consumed
    // by `pipeline::grouping::group_by_size(inventory)` further down.
    let history_total_files = inventory.len() as u64;
    let history_total_bytes_read: u64 = inventory.iter().map(|f| f.size).sum();
    // #25 T1.2 — clone the inventory ONLY in image mode so Tier-4
    // has the full file list to filter image-extensions out of.
    // Default mode (exact) skips the clone — no perf penalty for
    // the byte-identical pipeline.
    #[cfg(feature = "similar-images")]
    let inventory_for_tier4 = if matches!(mode, superdeduper::cli::ScanMode::Image) {
        Some(inventory.clone())
    } else {
        None
    };
    tracing::info!(
        count = inventory.len(),
        skipped = skipped.len(),
        elapsed_ms = inventory_ms as u64,
        "stage 1: inventory complete"
    );

    // T2.1 phase 7: --placeholders-only short-circuits stages 2-4.
    // User just wants the placeholder inventory; no hashing happens,
    // groups[] is empty, skipped[] is the payload.
    if args.placeholders_only {
        tracing::info!(
            placeholders = skipped.len(),
            "stage 2-4 skipped: --placeholders-only set"
        );
        let mut writer: Box<dyn Write> = match &cfg.output {
            Some(p) => Box::new(BufWriter::new(
                std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?,
            )),
            None => Box::new(BufWriter::new(io::stdout().lock())),
        };
        output::write(writer.as_mut(), cfg.format, &[], &skipped)?;
        writer.flush()?;
        return Ok(());
    }

    // Block Q: --force-hash bypasses stages 2-3 and runs Tier 3 on
    // every file regardless of size-grouping. Diagnostic mode for
    // measuring hash + Tier 3 IO throughput on corpora where most
    // files have unique sizes (videos, archives) and would otherwise
    // never enter Tier 3 under the standard dup-detection pipeline.
    if args.force_hash {
        run_force_hash_mode(&cfg, &inventory, &skipped, scan_started)?;
        return Ok(());
    }

    let t_group = std::time::Instant::now();
    let mut size_groups = pipeline::grouping::group_by_size(inventory);
    let group_ms = t_group.elapsed().as_millis();
    tracing::info!(
        groups = size_groups.len(),
        elapsed_ms = group_ms as u64,
        "stage 2: size grouping complete"
    );

    // Resolve NTFS file-id + volume-serial for entries that survived
    // size grouping. Files in singleton size groups never get this
    // syscall — that's the optimisation. See
    // `pipeline::grouping::resolve_file_ids` for rationale.
    let t_ids = std::time::Instant::now();
    pipeline::grouping::resolve_file_ids(&mut size_groups);
    tracing::info!(
        elapsed_ms = t_ids.elapsed().as_millis() as u64,
        "stage 2b: inode-id resolution complete"
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

    // T2.1 phase 7: surface placeholder skip counts so a smaller-
    // than-expected dup-group count has a visible explanation.
    let placeholders_recall = counters.placeholders_blocked_recall.load(Ordering::Relaxed);
    let placeholders_other = counters
        .placeholders_blocked_other_reparse
        .load(Ordering::Relaxed);
    let placeholders_total = placeholders_recall.saturating_add(placeholders_other);
    if placeholders_total > 0 {
        // Only suggest the recall flag when there's actually a recall
        // placeholder to unlock — suggesting it when only other-reparse
        // fired is misleading (the flag wouldn't change behaviour).
        let hint = if placeholders_recall > 0 {
            " — rerun with --allow-recall-on-read to include cloud stubs"
        } else {
            ""
        };
        let _ = writeln!(
            stderr,
            "tier guard skipped:    {placeholders_total} placeholder file(s) \
             ({placeholders_recall} cloud-recall, {placeholders_other} other reparse){hint}"
        );
    }

    let duplicates = if cfg.paranoid {
        pipeline::confirm::paranoid_verify(duplicates).context("paranoid verification failed")?
    } else {
        duplicates
    };

    // #25 T1.2 Tier-4 — perceptual image similarity. Runs ONLY when
    // `--mode image` was set + the `similar-images` feature is on.
    // Concatenates Tier-4 groups onto the byte-identical duplicates;
    // each gets a `similarity_kind: PerceptualImage` marker so
    // consumers can discriminate.
    #[cfg(feature = "similar-images")]
    let duplicates = {
        use superdeduper::cli::ScanMode;
        use superdeduper::pipeline::image_hash::{tier4, Algorithm};
        let mut all = duplicates;
        if matches!(mode, ScanMode::Image) {
            if let Some(inv) = inventory_for_tier4.as_deref() {
                let t_tier4 = std::time::Instant::now();
                let groups = tier4::find_similar_groups(
                    inv,
                    Algorithm::default(),
                    image_similarity_threshold,
                );
                let _ = writeln!(
                    io::stderr(),
                    "stage 4 perceptual: {} group(s) within {} bits ({} ms)",
                    groups.len(),
                    image_similarity_threshold,
                    t_tier4.elapsed().as_millis(),
                );
                all.extend(groups);
            }
        }
        all
    };
    #[cfg(not(feature = "similar-images"))]
    {
        if matches!(mode, superdeduper::cli::ScanMode::Image) {
            eprintln!(
                "warning: this binary was built without the `similar-images` \
                 feature — `--mode image` falls through to byte-identical \
                 dedup. Rebuild with `cargo build --features similar-images` \
                 (or use a release binary that ships with it on) to enable \
                 Tier-4."
            );
        }
        let _ = (mode, image_similarity_threshold);
    }

    let mut writer: Box<dyn Write> = match &cfg.output {
        Some(p) => Box::new(BufWriter::new(
            std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?,
        )),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };
    output::write(writer.as_mut(), cfg.format, &duplicates, &skipped)?;
    writer.flush()?;

    // #38 v1 — persist a scan_history record so CLI scans show up
    // in the same History tab the GUI populates. Best-effort: a
    // failure to write the JSON file doesn't fail the scan (the
    // user already got the results above). Same shape as the
    // gui::live::run() hook so both paths emit identical records.
    {
        let total_dups = duplicates.len() as u64;
        let reclaimable_bytes: u64 = duplicates
            .iter()
            .filter(|g| !g.link_equivalent)
            .map(|g| g.unique_inodes.saturating_sub(1) * g.size)
            .sum();
        let channel_slug = superdeduper::channel::active_channel().as_slug();
        let roots: Vec<String> = cfg
            .roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let record = superdeduper::scan_history::ScanRecord::new_finished(
            superdeduper::scan_history::new_scan_id(),
            started_at_unix,
            channel_slug,
            roots,
            history_total_files,
            history_total_bytes_read,
            total_dups,
            reclaimable_bytes,
        );
        if let Err(e) = superdeduper::scan_history::record_completed(&record) {
            tracing::warn!(error = %e, "scan_history: record_completed failed (non-fatal)");
        }
    }

    Ok(())
}

/// Block Q: diagnostic mode. Bypasses dup-detection grouping entirely
/// and runs a Tier-3 (full-content) hash on every inventoried file in
/// parallel. Reports throughput numbers but doesn't try to find
/// duplicates. Use to measure the hash + Tier-3-IO pipeline on real
/// corpora where most files have unique sizes (and would therefore
/// never enter Tier 3 under the standard pipeline).
fn run_force_hash_mode(
    cfg: &ScanConfig,
    inventory: &[inventory::FileEntry],
    skipped: &[superdeduper::pipeline::SkippedFile],
    scan_started: std::time::Instant,
) -> anyhow::Result<()> {
    use rayon::prelude::*;
    use std::sync::atomic::AtomicU64;

    let bytes_hashed = Arc::new(AtomicU64::new(0));
    let files_hashed = Arc::new(AtomicU64::new(0));
    let failures = Arc::new(AtomicU64::new(0));

    let t_hash = std::time::Instant::now();

    // Filter out tier-guard placeholders so we don't trigger cloud
    // hydration here either. Block J's tier guard normally handles
    // this inside the tier pipeline; force-hash mode replicates the
    // check.
    let candidates: Vec<&inventory::FileEntry> = inventory
        .iter()
        .filter(|e| {
            !e.placeholder
                .blocks_content_read_under_policy(cfg.allow_recall_on_read)
        })
        .collect();
    let skipped_for_placeholder = inventory.len() - candidates.len();

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(cfg.io_threads.max(1))
        .thread_name(|i| format!("superdeduper-force-hash-{i}"))
        .build()
        .map_err(|e| anyhow::anyhow!("io thread pool build: {e}"))?;
    // Block Q fix (post large-dups-r1-baseline observation): stream
    // each file through a fixed-size buffer instead of read_to_end.
    // The previous read_to_end approach grew the Vec to the full file
    // size, so 8 × 2 GiB files in concurrent workers gave a peak RSS
    // of 16 GiB — exactly the corpus size, and would OOM on real
    // workloads. The streaming path is what tier3_hash_cancellable
    // already uses for files above its 1 MiB oneshot threshold; we
    // replicate that here so --force-hash is safe at any corpus size.
    const STREAM_BUF: usize = 1 << 20; // 1 MiB, matches TIER3_BUF
    pool.install(|| {
        candidates.par_iter().for_each(|entry| {
            let path = &entry.path;
            let mut hasher = superdeduper::pipeline::hash::ContentHasher::new(cfg.hash_algo);
            let mut buf = vec![0u8; STREAM_BUF];
            match std::fs::File::open(path) {
                Ok(mut f) => {
                    let mut total = 0u64;
                    let mut had_error = false;
                    loop {
                        match std::io::Read::read(&mut f, &mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                hasher.update(&buf[..n]);
                                total += n as u64;
                            }
                            Err(e) => {
                                tracing::warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "force-hash read failed"
                                );
                                had_error = true;
                                break;
                            }
                        }
                    }
                    if had_error {
                        failures.fetch_add(1, Ordering::Relaxed);
                    } else {
                        // Finalize even if total bytes wasn't exactly
                        // entry.size — short reads can happen on
                        // concurrent modification. Counters reflect
                        // actual bytes hashed.
                        let _digest = hasher.finalize();
                        bytes_hashed.fetch_add(total, Ordering::Relaxed);
                        files_hashed.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::warn!(path = %path.display(), error = %e, "force-hash open failed");
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
    });

    let hash_wall = t_hash.elapsed();
    let total_wall = scan_started.elapsed();
    let bytes = bytes_hashed.load(Ordering::Relaxed);
    let files = files_hashed.load(Ordering::Relaxed);
    let fails = failures.load(Ordering::Relaxed);
    let throughput_mbps = if hash_wall.as_secs_f64() > 0.0 {
        (bytes as f64 / hash_wall.as_secs_f64()) / 1_048_576.0
    } else {
        0.0
    };

    let mut stderr = io::stderr().lock();
    let _ = writeln!(
        stderr,
        "\n--- force-hash mode ({}) ---\n\
         files candidates: {}  (skipped {} placeholders, {} read failures)\n\
         bytes hashed:     {}\n\
         hash wall:        {} ms\n\
         total wall:       {} ms\n\
         throughput:       {:.2} MB/s aggregate ({:.2} MB/s/thread)",
        cfg.hash_algo.tag(),
        files,
        skipped_for_placeholder,
        fails,
        humansize::format_size(bytes, humansize::BINARY),
        hash_wall.as_millis(),
        total_wall.as_millis(),
        throughput_mbps,
        throughput_mbps / cfg.io_threads.max(1) as f64,
    );

    // Write empty groups[] JSON for compatibility.
    let mut writer: Box<dyn Write> = match &cfg.output {
        Some(p) => Box::new(BufWriter::new(
            std::fs::File::create(p).with_context(|| format!("creating {}", p.display()))?,
        )),
        None => Box::new(BufWriter::new(io::stdout().lock())),
    };
    output::write(writer.as_mut(), cfg.format, &[], skipped)?;
    writer.flush()?;
    Ok(())
}

fn run_dedupe(args: DedupeArgs) -> anyhow::Result<()> {
    // #25 / #26 — mode dropdown stub. CLI accepts `image` and `audio`
    // values today but the Tier-4 (perceptual) pipeline integration
    // hasn't shipped; warn loudly + fall through to exact behaviour
    // so the user knows their mode wasn't honoured. Removing the
    // warning is part of the integration sub-deliverable (per spec
    // §3.3 + §3.7).
    use superdeduper::cli::ScanMode;
    match args.mode {
        ScanMode::Exact => {}
        ScanMode::Image => {
            eprintln!(
                "warning: --mode image is parsed but not yet wired \
                 — falling through to exact (byte-identical) dedup. \
                 Track #25 for the Tier-4 perceptual-image pipeline."
            );
        }
        ScanMode::Audio => {
            eprintln!(
                "warning: --mode audio is parsed but not yet wired \
                 — falling through to exact (byte-identical) dedup. \
                 Track #26 for acoustic fingerprinting."
            );
        }
    }

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
    let path = superdeduper::cache::default_cache_path()?;
    let cache = Cache::open(&path).context("opening cache database")?;
    match cmd {
        CacheCommand::Info => {
            let stats = cache.stats(&path).context("reading cache stats")?;
            println!("path:           {}", stats.path.display());
            println!("hash rows:      {}", stats.rows);
            println!("snapshot rows:  {}", stats.snapshot_rows);
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
    } else if let Ok(env) = std::env::var("SUPERDEDUPER_LOG") {
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
