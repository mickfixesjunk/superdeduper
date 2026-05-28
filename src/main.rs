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

    let result = dispatch(cli.command, cli.quiet);

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

fn dispatch(command: Command, quiet: bool) -> anyhow::Result<()> {
    match command {
        Command::Scan(args) => run_scan(args, quiet),
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
        #[cfg(feature = "telemetry")]
        Command::SubmitPending(args) => run_submit_pending(args),
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
        #[cfg(feature = "telemetry")]
        ScanHistoryCommand::Resubmit { scan_id, pending } => {
            run_scan_history_resubmit(scan_id, pending)
        }
        ScanHistoryCommand::Prune { days } => {
            // 0 = "forever — never prune" sentinel; the underlying
            // helper returns 0 in that case without touching the
            // filesystem. Print the no-op result so scripted callers
            // see expected output regardless of input.
            let retention_secs = (days as u64).saturating_mul(86_400);
            let pruned = scan_history::prune_older_than(retention_secs)?;
            if days == 0 {
                println!("scan-history: prune is a no-op (days=0 means retain forever).");
            } else {
                println!("scan-history: pruned {pruned} record(s) older than {days} day(s).");
            }
            Ok(())
        }
    }
}

/// #56 — Implementation for `scan-history resubmit`. Either replays
/// one scan_id or drains every Pending row. Single-row path calls
/// `submit_recorded_payload` against the row's recorded channel +
/// stored install; multi-row path defers to `submit-pending` which
/// already does the right thing.
#[cfg(feature = "telemetry")]
fn run_scan_history_resubmit(scan_id: Option<String>, pending: bool) -> anyhow::Result<()> {
    use anyhow::Context;
    use superdeduper::channel::{self, Channel};
    use superdeduper::leaderboard::install;
    use superdeduper::leaderboard::submission::{self, SubmitOutcome};
    use superdeduper::scan_history::{self, SubmissionState};

    match (scan_id, pending) {
        (Some(_), true) => {
            // clap's `conflicts_with` should already catch this; defensive.
            anyhow::bail!("pass either <SCAN_ID> or --pending, not both")
        }
        (None, false) => {
            anyhow::bail!("pass <SCAN_ID> or --pending to specify what to resubmit")
        }
        (None, true) => {
            // Multi-row drain — delegate to `submit-pending`'s
            // implementation (same channel partitioning + retry cap
            // semantics, all already battle-tested). Empty channel
            // filter, non-dry-run.
            let args = superdeduper::cli::SubmitPendingArgs {
                channel: None,
                dry_run: false,
                // #109 F25: this internal resubmit-drain keeps the
                // human-readable text stream (it's not feeding a JSON
                // consumer); --format json is for the `submit-pending`
                // CLI surface, not this delegated path.
                format: superdeduper::cli::OutputFormat::Text,
            };
            run_submit_pending(args)
        }
        (Some(id), false) => {
            let record = scan_history::load(&id)?
                .ok_or_else(|| anyhow::anyhow!("no scan-history row matches scan_id {id}"))?;
            let payload = record
                .submission_payload
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!(
                    "row {id} has no captured submission payload (likely a v1/v2 row from before #41)"
                ))?;
            let built_with = record
                .built_with_install_id
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("row {id} has no captured install_id"))?;
            let chan: Channel = record
                .channel
                .parse()
                .with_context(|| format!("parse channel slug `{}`", record.channel))?;
            let server_url = channel::server_url_for(chan);
            let install_state = install::load_for(chan)?.ok_or_else(|| {
                anyhow::anyhow!(
                    "no install state for channel `{}` — run `superdeduper register --channel {}` first",
                    chan.as_slug(),
                    chan.as_slug()
                )
            })?;
            if !install_state.registered {
                anyhow::bail!(
                    "install for channel `{}` not registered — run `superdeduper register --channel {}` first",
                    chan.as_slug(),
                    chan.as_slug()
                );
            }
            let outcome = submission::submit_recorded_payload(
                &install_state,
                payload,
                built_with,
                server_url,
            );
            match outcome {
                SubmitOutcome::Accepted { submission_id, .. } => {
                    scan_history::mark_submitted(&id, submission_id.clone())?;
                    println!("✓ {id} accepted (submission_id={submission_id})");
                    Ok(())
                }
                SubmitOutcome::DuplicateNoChange => {
                    scan_history::update_submission_state(&id, SubmissionState::Submitted, true)?;
                    println!("• {id} already-on-file (409) — marked submitted");
                    Ok(())
                }
                SubmitOutcome::Rejected { status, reason } => {
                    scan_history::update_submission_state(&id, SubmissionState::Failed, true)?;
                    anyhow::bail!("✗ {id} rejected (status={status}, reason={reason})")
                }
                SubmitOutcome::Transient { reason } => {
                    let prior_attempts = scan_history::load(&id)?
                        .map(|r| r.attempt_count)
                        .unwrap_or(0);
                    let new_state = scan_history::transient_outcome_state(prior_attempts);
                    scan_history::update_submission_state(&id, new_state, true)?;
                    let suffix = match new_state {
                        SubmissionState::Failed => " — retry cap reached, marked Failed",
                        _ => " — will retry on next run",
                    };
                    anyhow::bail!("· {id} transient (reason={reason}){suffix}")
                }
                SubmitOutcome::FlaggedForReview { .. } => {
                    scan_history::update_submission_state(&id, SubmissionState::Submitted, true)?;
                    println!("• {id} queued for review — marked submitted");
                    Ok(())
                }
            }
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
            println!("{:<28}  {:<6}  {:<22}  NAME", "ID", "TIER", "GRANTED_AT");
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

    // #58 follow-up — `--print-captcha-url` short-circuit. Resolve
    // the channel-aware captcha URL, print it, exit. No install
    // mutation, no PoW, no browser, no loopback. Uses the existing
    // install_id if one is on disk; falls back to a placeholder so
    // testdesign's AT-captcha-* tests can pattern-match the channel
    // routing without needing a registered install.
    if args.print_captcha_url {
        let frontend =
            superdeduper::channel::frontend_url_for(superdeduper::channel::active_channel());
        let install_id = install::load()?
            .map(|s| s.install_id)
            .unwrap_or_else(|| "<unregistered>".to_string());
        println!("{frontend}/setup/{install_id}");
        return Ok(());
    }

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

    // #58 — route the profile URL through the active channel's
    // frontend so dev/local installs print the right host.
    let frontend = superdeduper::channel::frontend_url_for(superdeduper::channel::active_channel());
    if state.registered {
        println!(
            "Already registered. install_id = {}\nProfile: {frontend}/profile/{}",
            state.install_id, state.install_id
        );
        return Ok(());
    }

    println!("First-time setup: registering this install. This takes ~1 second of CPU.");
    match registration::register_cli(&mut state) {
        Ok(()) => {
            println!(
                "Registered. install_id = {}\nProfile: {frontend}/profile/{}\nUse `superdeduper config show` to see current share preference.",
                state.install_id, state.install_id
            );
            Ok(())
        }
        Err(e) => Err(anyhow::anyhow!("registration failed: {e:?}")),
    }
}

/// #94 — `superdeduper submit-pending`. Drains every scan-history
/// row with `submission_state = Pending` to the leaderboard. The
/// payload + signing key are already captured in the row (via
/// `submission_payload` + `built_with_install_id` per #41); this
/// subcommand resolves the channel, loads the matching install,
/// and POSTs via the existing `submit_recorded_payload` flow that
/// the GUI Resubmit button already uses. Closes the auto-submit
/// gap testrunner surfaced in v0.2.8 #79 empirical testing.
#[cfg(feature = "telemetry")]
fn run_submit_pending(args: superdeduper::cli::SubmitPendingArgs) -> anyhow::Result<()> {
    use anyhow::Context;
    use superdeduper::channel::{self, Channel};
    use superdeduper::cli::OutputFormat;
    use superdeduper::leaderboard::install;
    use superdeduper::leaderboard::submission::{self, SubmitOutcome};
    use superdeduper::scan_history::{self, SubmissionState};

    // #109 F25 — JSON output gates EVERYTHING through the
    // accumulator. Text output still streams per-row to stdout/err
    // for tail-friendliness; the accumulator drives the closing
    // summary either way.
    let json_mode = matches!(args.format, OutputFormat::Json);
    let say = |line: &str| {
        if !json_mode {
            println!("{line}");
        }
    };
    let warn = |line: &str| {
        if !json_mode {
            eprintln!("{line}");
        }
    };

    // threshold_secs = 0 → return ALL pending rows regardless of age.
    let pending = scan_history::list_pending_older_than(0)?;

    let want_channel = args.channel.as_deref();
    let (eligible, unsubmittable): (Vec<_>, Vec<_>) = pending
        .into_iter()
        .filter(|r| want_channel.is_none_or(|c| r.channel == c))
        .partition(|r| r.submission_payload.is_some() && r.built_with_install_id.is_some());

    if !unsubmittable.is_empty() {
        warn(&format!(
            "{} pending row(s) with no captured payload — these were recorded \
             before payload-persistence (#41) landed; skipping:",
            unsubmittable.len()
        ));
        for r in &unsubmittable {
            warn(&format!("  - {} (channel: {})", r.scan_id, r.channel));
        }
    }

    let mut report = SubmitPendingReport {
        unsubmittable: unsubmittable
            .iter()
            .map(|r| UnsubmittableRow {
                scan_id: r.scan_id.clone(),
                channel: r.channel.clone(),
                reason: "no_captured_payload".to_string(),
            })
            .collect(),
        ..Default::default()
    };

    if eligible.is_empty() {
        if json_mode {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            say("No drainable pending submissions.");
        }
        return Ok(());
    }

    say(&format!(
        "Found {} pending submission(s) to drain:",
        eligible.len()
    ));
    for r in &eligible {
        let reclaim = humansize::format_size(r.reclaimable_bytes, humansize::BINARY);
        say(&format!(
            "  {} channel={:<6} reclaim={:>10}  ({} group(s), {} file(s))",
            r.scan_id, r.channel, reclaim, r.total_dups, r.total_files,
        ));
    }

    if args.dry_run {
        report.dry_run = true;
        report.records = eligible
            .iter()
            .map(|r| RecordOutcome {
                scan_id: r.scan_id.clone(),
                channel: r.channel.clone(),
                outcome: "dry_run".to_string(),
                detail: None,
                submission_id: None,
            })
            .collect();
        if json_mode {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            say("\n(dry-run — no POST sent)");
        }
        return Ok(());
    }

    say("");
    // #109 F27 — local state-update failures used to `?`-propagate out
    // and abort the remaining drain. The POST already succeeded
    // (server has the row); next run's `submit_recorded_payload`
    // returns DuplicateNoChange and self-corrects. Log + continue
    // so other rows still drain.
    let log_state_err = |action: &str, scan_id: &str, e: &std::io::Error| {
        tracing::warn!(
            error = %e,
            scan_id = %scan_id,
            action = %action,
            "scan_history: local state update failed; next run will reconcile",
        );
    };
    for record in eligible {
        let chan: Channel = record
            .channel
            .parse()
            .with_context(|| format!("parse channel slug `{}`", record.channel))?;
        let server_url = channel::server_url_for(chan);
        let install_state = match install::load_for(chan)? {
            Some(s) if s.registered => s,
            Some(_) | None => {
                let detail = format!(
                    "install for channel `{}` is not registered — run `superdeduper register --channel {}` first",
                    chan.as_slug(),
                    chan.as_slug()
                );
                warn(&format!("  ✗ {} skipped — {detail}", record.scan_id));
                report.records.push(RecordOutcome {
                    scan_id: record.scan_id.clone(),
                    channel: record.channel.clone(),
                    outcome: "skipped_not_registered".to_string(),
                    detail: Some(detail),
                    submission_id: None,
                });
                report.rejected = report.rejected.saturating_add(1);
                continue;
            }
        };
        let payload = record.submission_payload.as_ref().expect("filtered above");
        let built_with = record
            .built_with_install_id
            .as_ref()
            .expect("filtered above");
        let outcome =
            submission::submit_recorded_payload(&install_state, payload, built_with, server_url);
        match outcome {
            SubmitOutcome::Accepted { submission_id, .. } => {
                if let Err(e) = scan_history::mark_submitted(&record.scan_id, submission_id.clone())
                {
                    log_state_err("mark_submitted", &record.scan_id, &e);
                }
                report.submitted = report.submitted.saturating_add(1);
                report.records.push(RecordOutcome {
                    scan_id: record.scan_id.clone(),
                    channel: record.channel.clone(),
                    outcome: "accepted".to_string(),
                    detail: None,
                    submission_id: Some(submission_id.clone()),
                });
                say(&format!(
                    "  ✓ {} accepted (submission_id={})",
                    record.scan_id, submission_id
                ));
            }
            SubmitOutcome::DuplicateNoChange => {
                if let Err(e) = scan_history::update_submission_state(
                    &record.scan_id,
                    SubmissionState::Submitted,
                    true,
                ) {
                    log_state_err("duplicate_no_change", &record.scan_id, &e);
                }
                report.duplicate = report.duplicate.saturating_add(1);
                report.records.push(RecordOutcome {
                    scan_id: record.scan_id.clone(),
                    channel: record.channel.clone(),
                    outcome: "duplicate_no_change".to_string(),
                    detail: None,
                    submission_id: None,
                });
                say(&format!(
                    "  • {} already-on-file (409 DuplicateNoChange) — marking submitted",
                    record.scan_id
                ));
            }
            SubmitOutcome::Rejected { status, reason } => {
                if let Err(e) = scan_history::update_submission_state(
                    &record.scan_id,
                    SubmissionState::Failed,
                    true,
                ) {
                    log_state_err("rejected", &record.scan_id, &e);
                }
                report.rejected = report.rejected.saturating_add(1);
                report.records.push(RecordOutcome {
                    scan_id: record.scan_id.clone(),
                    channel: record.channel.clone(),
                    outcome: "rejected".to_string(),
                    detail: Some(format!("status={status} reason={reason}")),
                    submission_id: None,
                });
                warn(&format!(
                    "  ✗ {} rejected (status={status}, reason={reason})",
                    record.scan_id
                ));
            }
            SubmitOutcome::Transient { reason } => {
                let prior_attempts = scan_history::load(&record.scan_id)?
                    .map(|r| r.attempt_count)
                    .unwrap_or(0);
                let new_state = scan_history::transient_outcome_state(prior_attempts);
                if let Err(e) =
                    scan_history::update_submission_state(&record.scan_id, new_state, true)
                {
                    log_state_err("transient", &record.scan_id, &e);
                }
                report.transient = report.transient.saturating_add(1);
                let cap_reached = matches!(new_state, SubmissionState::Failed);
                let suffix = if cap_reached {
                    " — retry cap reached, marked Failed"
                } else {
                    " — will retry on next run"
                };
                report.records.push(RecordOutcome {
                    scan_id: record.scan_id.clone(),
                    channel: record.channel.clone(),
                    outcome: if cap_reached {
                        "transient_cap_reached".to_string()
                    } else {
                        "transient".to_string()
                    },
                    detail: Some(reason.clone()),
                    submission_id: None,
                });
                warn(&format!(
                    "  · {} transient (reason={reason}){suffix}",
                    record.scan_id
                ));
            }
            SubmitOutcome::FlaggedForReview { .. } => {
                if let Err(e) = scan_history::update_submission_state(
                    &record.scan_id,
                    SubmissionState::Submitted,
                    true,
                ) {
                    log_state_err("flagged_for_review", &record.scan_id, &e);
                }
                report.submitted = report.submitted.saturating_add(1);
                report.records.push(RecordOutcome {
                    scan_id: record.scan_id.clone(),
                    channel: record.channel.clone(),
                    outcome: "flagged_for_review".to_string(),
                    detail: None,
                    submission_id: None,
                });
                say(&format!("  ✓ {} queued for review", record.scan_id));
            }
        }
    }

    let total = report.submitted + report.duplicate + report.rejected + report.transient;
    report.total = total;
    if json_mode {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        say(&format!(
            "\n{} drained: {} accepted, {} already-on-file, {} rejected, {} transient.",
            total, report.submitted, report.duplicate, report.rejected, report.transient
        ));
    }
    if report.rejected > 0 || report.transient > 0 {
        std::process::exit(2);
    }
    Ok(())
}

/// #109 F25 — JSON-shape output for `submit-pending`. Mirrors the
/// human-readable text stream + adds machine-parseable per-row
/// records so integration tests don't have to string-match the
/// per-row print lines.
#[cfg(feature = "telemetry")]
#[derive(Debug, Default, serde::Serialize)]
struct SubmitPendingReport {
    /// True when --dry-run was passed; records is populated with
    /// the planned set but no POSTs fired.
    #[serde(default)]
    dry_run: bool,
    /// Sum of submitted + duplicate + rejected + transient. Zero
    /// before any work happens (dry-run or no-eligible-rows).
    total: u64,
    submitted: u64,
    duplicate: u64,
    rejected: u64,
    transient: u64,
    /// Per-row drain outcome. Order matches the order of submission
    /// (newest-first per `list_pending_older_than`'s sort).
    records: Vec<RecordOutcome>,
    /// Rows that were Pending but couldn't be drained (no captured
    /// payload — v1/v2 rows pre-#41). Local-only; never sent.
    unsubmittable: Vec<UnsubmittableRow>,
}

#[cfg(feature = "telemetry")]
#[derive(Debug, serde::Serialize)]
struct RecordOutcome {
    scan_id: String,
    channel: String,
    /// One of: `accepted`, `duplicate_no_change`, `rejected`,
    /// `transient`, `transient_cap_reached`, `flagged_for_review`,
    /// `skipped_not_registered`, `dry_run`.
    outcome: String,
    /// Free-text detail (rejection reason, transient error, etc.).
    /// `None` when no extra detail applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    detail: Option<String>,
    /// Server-issued submission_id on `accepted` outcomes only.
    #[serde(skip_serializing_if = "Option::is_none")]
    submission_id: Option<String>,
}

#[cfg(feature = "telemetry")]
#[derive(Debug, serde::Serialize)]
struct UnsubmittableRow {
    scan_id: String,
    channel: String,
    reason: String,
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
/// #81 — Helper for `--list-exclusion-packs`. Prints every preset
/// pack with its content so the user can decide which to enable
/// or disable. Format: kebab-case ID (the CLI value), label,
/// counts, then each extension + path pattern indented.
fn print_exclusion_packs() {
    use superdeduper::exclusions::{presets::BuiltinPresets, PresetPackId, PresetSource};
    let presets = BuiltinPresets;
    println!("Preset packs (CLI value: kebab-case ID):\n");
    for id in PresetPackId::ALL {
        let pack = presets.get(id);
        // Re-derive the kebab-case form via serde's representation.
        let kebab = serde_json::to_string(&id)
            .ok()
            .and_then(|s| s.trim_matches('"').to_string().into())
            .unwrap_or_else(|| format!("{:?}", id).to_lowercase());
        let safe = PresetPackId::SAFE_DEFAULTS.contains(&id);
        let marker = if safe { " (safe-defaults ON)" } else { "" };
        println!("  {kebab}  —  {}{marker}", id.label(),);
        println!(
            "    {} extension{}, {} path pattern{}",
            pack.extensions.len(),
            if pack.extensions.len() == 1 { "" } else { "s" },
            pack.paths.len(),
            if pack.paths.len() == 1 { "" } else { "s" },
        );
        if !pack.extensions.is_empty() {
            println!("    extensions: {}", pack.extensions.join(", "));
        }
        for p in pack.paths {
            println!("      path: {p}");
        }
        println!();
    }
    println!("Usage:");
    println!("  --exclusions on|off                         master toggle (default on)");
    println!("  --exclusion-pack <id>                       enable an additional pack");
    println!("  --exclusion-pack-disable <id>               disable a safe-defaults pack");
}

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

/// F-CLI-2 — stdout writer for scan output, honoring `--quiet`. Under
/// quiet, human-readable console output (Text / Report) is "non-error
/// status" and is silenced to a sink. Machine formats (JSON / CSV — the
/// requested deliverable, e.g. `-q --format json | jq`) still emit;
/// explicit `--output FILE` is handled by the caller and never routed
/// here. Shared by both scan output paths so the gating can't drift.
fn scan_console_writer(format: superdeduper::cli::OutputFormat, quiet: bool) -> Box<dyn Write> {
    use superdeduper::cli::OutputFormat;
    if quiet && matches!(format, OutputFormat::Text | OutputFormat::Report) {
        Box::new(io::sink())
    } else {
        Box::new(BufWriter::new(io::stdout().lock()))
    }
}

/// F-CLI-7 — group-member files that fall under a `--reference` root,
/// resolved at scan time so the scan→dedupe-file two-step can honor
/// `--strategy in-reference`. The under-root prefix-compare normalizes
/// the Windows verbatim `\\?\` prefix on BOTH sides first (S15) so a
/// verbatim scanned path matches a (possibly verbatim) reference root;
/// `Path::starts_with` is component-wise so `C:\refs` doesn't falsely
/// prefix `C:\refs-backup`. Returns the ORIGINAL file paths (as they
/// appear in `groups[].files`); dedupe canonicalizes both these and the
/// group members before matching, so the stored representation only has
/// to round-trip, not match byte-for-byte.
fn resolve_reference_paths(
    reference_roots: &[std::path::PathBuf],
    groups: &[pipeline::DuplicateGroup],
) -> Vec<std::path::PathBuf> {
    if reference_roots.is_empty() {
        return Vec::new();
    }
    let norm_roots: Vec<std::path::PathBuf> = reference_roots
        .iter()
        .map(|r| std::path::PathBuf::from(superdeduper::path_display::for_user_display(r)))
        .collect();
    let mut refs = Vec::new();
    for g in groups {
        for f in &g.files {
            let nf = std::path::PathBuf::from(superdeduper::path_display::for_user_display(f));
            if norm_roots.iter().any(|root| nf.starts_with(root)) {
                refs.push(f.clone());
            }
        }
    }
    refs
}

fn run_scan(args: ScanArgs, quiet: bool) -> anyhow::Result<()> {
    // #81 — `--list-exclusion-packs` short-circuits the scan and
    // prints every preset pack with its content so the user can
    // see exactly what each pack covers before deciding whether to
    // enable / disable it.
    if args.list_exclusion_packs {
        print_exclusion_packs();
        return Ok(());
    }
    let cfg = ScanConfig::from_args(&args).context("invalid scan configuration")?;

    // #25 T1.2 Tier-4 — perceptual image similarity. Threaded
    // through directly rather than added to ScanConfig because
    // Tier-4 runs on the inventory after Tier-3, separate from
    // the byte-identical pipeline state.
    let mode = args.mode;
    let image_similarity_threshold = args.image_similarity_threshold;
    let image_hash_algorithm = args.image_hash_algorithm;
    let audio_similarity_threshold = args.audio_similarity_threshold;

    // chore/97-build-gotcha — close the silent-fall-through to
    // byte-identical when `--mode {image,audio}` is requested on a
    // binary built without the matching feature. Previously emitted
    // a warning + ran exact-only matching; the user clicked Recycle
    // on a Tier-0-3 result thinking they got perceptual matching.
    // Same anti-pattern that bit #97 v1 (the chromaprint silent-
    // drop of S32). Hard-error before any scan work happens.
    #[cfg(not(feature = "similar-images"))]
    if matches!(mode, superdeduper::cli::ScanMode::Image) {
        anyhow::bail!(
            "this binary was built without the `similar-images` feature, \
             but `--mode image` was requested. Rebuild with \
             `cargo build --features similar-images` (or use a release \
             binary that ships with it on) to enable Tier-4 perceptual \
             image similarity. Refusing to fall through to byte-identical \
             dedup."
        );
    }
    #[cfg(not(feature = "similar-audio"))]
    if matches!(mode, superdeduper::cli::ScanMode::Audio) {
        anyhow::bail!(
            "this binary was built without the `similar-audio` feature, \
             but `--mode audio` was requested. Rebuild with \
             `cargo build --features similar-audio` (or use a release \
             binary that ships with it on) to enable Tier-4 acoustic \
             similarity. Refusing to fall through to byte-identical \
             dedup."
        );
    }

    let scan_started = std::time::Instant::now();
    // Wall-clock UNIX seconds for the scan_history record — same
    // pattern as `gui::live::run()` so CLI + GUI scans both persist
    // history rows with comparable timestamps.
    let started_at_unix = superdeduper::time::now_unix_secs();
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

    // #15 L2 — surface mount-info warnings per scan root on Linux.
    // Pool-dedup-capable filesystems (zfs / btrfs), network mounts
    // (nfs / cifs / sshfs), and dm-mapped volumes (LUKS) each have
    // their own gotchas — warn the user once at scan-start so they
    // can read the reclaim numbers with the right frame.
    #[cfg(target_os = "linux")]
    {
        use std::io::Write as _;
        let mut stderr = io::stderr().lock();
        for root in &cfg.roots {
            if let Some(info) = superdeduper::platform::linux::mount_info::for_path(root) {
                let _ = writeln!(stderr, "mount: {}", info.summary_line());
                for w in info.warnings() {
                    let _ = writeln!(stderr, "  ⚠ {w}");
                }
            }
        }
    }
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
    // #149 — compute client-claimed easter-egg hits HERE, while the
    // inventory is still owned (group_by_size moves it below). The CLI
    // payload previously hardcoded an empty vec, so no client-claimed
    // achievement could grant via a CLI scan. Shared helper with the
    // GUI live-scan so the two can't drift.
    #[cfg(feature = "telemetry")]
    let easter_egg_hits =
        superdeduper::leaderboard::predicates::compute_easter_egg_hits(&inventory);
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
    // #26 v2 — same clone-for-tier4 dance for the audio-similarity
    // mode. Separate variable + cfg gate (similar-audio is its own
    // feature) so a binary with only one of the two features built
    // in still works.
    #[cfg(feature = "similar-audio")]
    let inventory_for_tier4_audio = if matches!(mode, superdeduper::cli::ScanMode::Audio) {
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
            // #137 — file branch via the shared output::open_writer; None keeps
            // the quiet-aware scan_console_writer.
            Some(p) => output::open_writer(Some(p))
                .with_context(|| format!("creating {}", p.display()))?,
            None => scan_console_writer(cfg.format, quiet),
        };
        output::write(writer.as_mut(), cfg.format, &[], &skipped, &[])?;
        writer.flush()?;
        return Ok(());
    }

    // Block Q: --force-hash bypasses stages 2-3 and runs Tier 3 on
    // every file regardless of size-grouping. Diagnostic mode for
    // measuring hash + Tier 3 IO throughput on corpora where most
    // files have unique sizes (videos, archives) and would otherwise
    // never enter Tier 3 under the standard dup-detection pipeline.
    if args.force_hash {
        run_force_hash_mode(&cfg, &inventory, &skipped, scan_started, quiet)?;
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

    // #131 — `--paranoid` byte-by-byte verification was deleted; the
    // flag claimed a safety feature that didn't exist (no-op stub
    // since v0; never implemented). Real byte-by-byte verification
    // is a v0.3.x feature scope behind its own design. Until then,
    // post-Tier-3 duplicates flow through unchanged.

    // #25 T1.2 Tier-4 — perceptual image similarity. Runs ONLY when
    // `--mode image` was set + the `similar-images` feature is on.
    // Concatenates Tier-4 groups onto the byte-identical duplicates;
    // each gets a `similarity_kind: PerceptualImage` marker so
    // consumers can discriminate.
    #[cfg(feature = "similar-images")]
    let duplicates = {
        use superdeduper::cli::ScanMode;
        use superdeduper::pipeline::image_hash::tier4::is_image_file;
        use superdeduper::pipeline::image_hash::{tier4, Algorithm};
        let mut all = duplicates;
        if matches!(mode, ScanMode::Image) {
            if let Some(inv) = inventory_for_tier4.as_deref() {
                let algo: Algorithm = image_hash_algorithm.into();
                // E3 (#78): resolve auto-threshold using the count
                // of image-extension files in the inventory. Exact
                // n is "files that pass is_image_file"; this is the
                // count seen by the tier-4 hash step (a slight
                // overestimate of decoded n when a few images fail
                // to decode, but close enough for the log10 scaling
                // that auto uses).
                let n_images = inv.iter().filter(|f| is_image_file(&f.path)).count() as u64;
                let threshold =
                    image_similarity_threshold.resolve(tier4::DEFAULT_THRESHOLD, n_images);
                let t_tier4 = std::time::Instant::now();
                let groups = tier4::find_similar_groups(inv, algo, threshold);
                let _ = writeln!(
                    io::stderr(),
                    "stage 4 perceptual ({}): {} group(s) within {} bits ({} ms; n_images={})",
                    algo.as_slug(),
                    groups.len(),
                    threshold,
                    t_tier4.elapsed().as_millis(),
                    n_images,
                );
                all.extend(groups);
            }
        }
        all
    };
    // #26 T1.3 Tier-4 — acoustic audio similarity. Parallel to the
    // image branch above; runs ONLY when `--mode audio` was set +
    // the `similar-audio` feature is on. czkawka's default 5-bits-
    // per-chunk threshold is now user-tunable via the
    // `--audio-similarity-threshold` flag (GH #53).
    #[cfg(feature = "similar-audio")]
    let duplicates = {
        use superdeduper::cli::ScanMode;
        use superdeduper::pipeline::audio_hash::tier4;
        let mut all = duplicates;
        if matches!(mode, ScanMode::Audio) {
            if let Some(inv) = inventory_for_tier4_audio.as_deref() {
                let t_tier4 = std::time::Instant::now();
                let result = tier4::find_similar_groups(inv, audio_similarity_threshold);
                let _ = writeln!(
                    io::stderr(),
                    "stage 4 acoustic: {} group(s) within {} bits/chunk avg ({} ms)",
                    result.groups.len(),
                    audio_similarity_threshold,
                    t_tier4.elapsed().as_millis(),
                );
                // #102 — surface <30s perceptual-skip count so CLI
                // users understand why their short voice memos / sound
                // effects didn't cluster perceptually. Byte-identical
                // matching still ran in Tier 0-3.
                if result.short_skipped_count > 0 {
                    let _ = writeln!(
                        io::stderr(),
                        "stage 4 acoustic: {} audio file(s) too short for perceptual matching (<30s); processed via byte-identical tier only",
                        result.short_skipped_count,
                    );
                }
                all.extend(result.groups);
            }
        }
        all
    };
    // chore/97-build-gotcha — the `--mode {image,audio}` feature-
    // missing case is hard-errored at the top of run_scan() before
    // any scan work happens. These `let _` lines silence the
    // unused-variable warning in the `not(feature = "…")` build of
    // the rest of run_scan().
    #[cfg(not(feature = "similar-images"))]
    let _ = (mode, image_similarity_threshold, image_hash_algorithm);
    #[cfg(not(feature = "similar-audio"))]
    let _ = audio_similarity_threshold;

    // F-CLI-7 — resolve which group-member files fall under a
    // --reference root + persist them so `dedupe --strategy in-reference`
    // works through the scan→dedupe-file two-step.
    let reference_paths = resolve_reference_paths(&cfg.reference_roots, &duplicates);
    let mut writer: Box<dyn Write> = match &cfg.output {
        // #137 — file branch via the shared output::open_writer; None keeps
        // the quiet-aware scan_console_writer.
        Some(p) => output::open_writer(Some(p))
            .with_context(|| format!("creating {}", p.display()))?,
        None => scan_console_writer(cfg.format, quiet),
    };
    output::write(writer.as_mut(), cfg.format, &duplicates, &skipped, &reference_paths)?;
    writer.flush()?;

    // #38 v1 + #142 — persist a scan_history record so CLI scans
    // show up in the same History tab the GUI populates AND so
    // `submit-pending` can flush them to the leaderboard. Pre-#142
    // the CLI write skipped the submission_payload (only GUI built
    // one), which made every CLI-scanned row invisible to
    // submit-pending. Best-effort throughout: a failure to write
    // the JSON file doesn't fail the scan; a failure to build the
    // payload (telemetry off, no install state) leaves the row
    // without a payload but the row still writes so the History
    // tab surfaces the scan.
    {
        let total_dups = duplicates.len() as u64;
        let reclaimable_bytes: u64 = duplicates
            .iter()
            .filter(|g| !g.link_equivalent)
            .map(|g| g.unique_inodes.saturating_sub(1) * g.size)
            .sum();
        let channel_slug = superdeduper::channel::active_channel().as_slug();
        let roots_strings: Vec<String> = cfg
            .roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let groups_by_similarity_kind =
            superdeduper::scan_history::similarity_kind_breakdown(&duplicates);
        let scan_id = superdeduper::scan_history::new_scan_id();
        #[cfg_attr(not(feature = "telemetry"), allow(unused_mut))]
        let mut record = superdeduper::scan_history::ScanRecord::new_finished(
            scan_id.clone(),
            started_at_unix,
            channel_slug,
            roots_strings,
            history_total_files,
            history_total_bytes_read,
            total_dups,
            reclaimable_bytes,
            groups_by_similarity_kind,
        );

        // #142 — build the submission payload + attach to the
        // scan_history row so submit-pending picks it up. Mirrors
        // gui::live::run()'s pattern; the CLI's pipeline doesn't
        // instrument as many esoteric metrics as the GUI's worker
        // (cache_hit_ratio, hardlink counts, easter-egg hits) so
        // the optional run_shape fields stay None. The locked
        // fields (wall_clock, bytes, files, hash_algorithm, scope,
        // features bitmap, corpus_kind) + result_summary land
        // correctly; web rank + lifetime + pathfinder grants fire
        // from CLI submissions just like GUI submissions.
        #[cfg(feature = "telemetry")]
        if let Ok(Some(install_state)) = superdeduper::leaderboard::install::load() {
            use superdeduper::leaderboard::hardware;
            use superdeduper::leaderboard::payload_meta;
            use superdeduper::leaderboard::submission::{
                self, ResultSummary, RunShape, SubmissionInputs, FEATURE_BIT_ALLOW_RECALL_ON_READ,
                FEATURE_BIT_ALLOW_SYSTEM_PATHS, FEATURE_BIT_CACHE, FEATURE_BIT_EXCLUDE_GLOB,
                FEATURE_BIT_FOLLOW_LINKS, FEATURE_BIT_FORMAT_AWARE, FEATURE_BIT_INCLUDE_GLOB,
                FEATURE_BIT_REFERENCE_ROOTS,
            };
            let wall_clock_seconds = scan_started.elapsed().as_secs_f64();
            let hash_algorithm = match cfg.hash_algo {
                superdeduper::pipeline::hash::HashAlgo::Blake3 => "blake3",
                superdeduper::pipeline::hash::HashAlgo::River5 => "river5-aes-ni",
            }
            .to_string();
            let scope = payload_meta::classify_scope(&cfg.roots);
            let corpus_kind = payload_meta::classify_corpus_kind(&cfg.roots);
            let mut features_bits: u64 = 0;
            if cfg.use_cache {
                features_bits |= FEATURE_BIT_CACHE;
            }
            if cfg.use_format_aware {
                features_bits |= FEATURE_BIT_FORMAT_AWARE;
            }
            if cfg.follow_links {
                features_bits |= FEATURE_BIT_FOLLOW_LINKS;
            }
            if cfg.allow_system_paths {
                features_bits |= FEATURE_BIT_ALLOW_SYSTEM_PATHS;
            }
            if cfg.allow_recall_on_read {
                features_bits |= FEATURE_BIT_ALLOW_RECALL_ON_READ;
            }
            if !cfg.reference_roots.is_empty() {
                features_bits |= FEATURE_BIT_REFERENCE_ROOTS;
            }
            if cfg.include.is_some() {
                features_bits |= FEATURE_BIT_INCLUDE_GLOB;
            }
            if cfg.exclude.is_some() {
                features_bits |= FEATURE_BIT_EXCLUDE_GLOB;
            }
            let share_count = payload_meta::count_distinct_share_roots(&cfg.roots);
            let largest_group_bytes: u64 = duplicates
                .iter()
                .filter(|g| !g.link_equivalent)
                .map(|g| g.size.saturating_mul(g.unique_inodes.saturating_sub(1)))
                .max()
                .unwrap_or(0);
            let inputs = SubmissionInputs {
                client_version: env!("CARGO_PKG_VERSION").to_string(),
                run_uuid: uuid::Uuid::new_v4().to_string(),
                scan_id: Some(scan_id.clone()),
                hardware: hardware::detect_with_root_hint(
                    cfg.roots.first().map(|p| p.as_path()),
                ),
                run_shape: RunShape {
                    wall_clock_seconds,
                    bytes_scanned: history_total_bytes_read,
                    files_scanned: history_total_files,
                    hash_algorithm,
                    walker_variant: "hybrid".to_string(),
                    scope,
                    features_used_bitmap: features_bits,
                    corpus_kind,
                    cache_hit_ratio: None,
                    // #149 — computed above before the inventory was
                    // consumed (was hardcoded empty → CLI never granted
                    // any client-claimed achievement).
                    easter_egg_hits,
                    zero_byte_group_max: None,
                    max_hardlink_count_in_scan: None,
                    name_collision_count: None,
                    share_count_in_scope: if share_count > 0 {
                        Some(share_count)
                    } else {
                        None
                    },
                    dry_run: None,
                    groups_reviewed_count: None,
                },
                result_summary: ResultSummary {
                    duplicate_groups: total_dups,
                    duplicate_bytes_reclaimable: reclaimable_bytes
                        .min(history_total_bytes_read),
                    largest_single_group_bytes: largest_group_bytes
                        .min(history_total_bytes_read),
                    actions_taken_summary: std::collections::BTreeMap::new(),
                    placeholder_skip_count: if skipped.is_empty() {
                        None
                    } else {
                        Some(skipped.len() as u64)
                    },
                    placeholder_skip_bytes: None,
                },
            };
            let payload = submission::build_payload(&inputs, &install_state.install_id);
            record = record.with_submission_payload(payload, install_state.install_id);
        }

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
    quiet: bool,
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
        // #137 — file branch via the shared output::open_writer; None keeps
        // the quiet-aware scan_console_writer.
        Some(p) => output::open_writer(Some(p))
            .with_context(|| format!("creating {}", p.display()))?,
        None => scan_console_writer(cfg.format, quiet),
    };
    output::write(writer.as_mut(), cfg.format, &[], skipped, &[])?;
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
    //
    // `--mode` on `dedupe` is a pass-through hint — the actual
    // similarity grouping happens in `scan`, and the groups in
    // results.json carry their own `similarity_kind`. We accept
    // the flag to keep the CLI surface symmetric with `scan` (so
    // users don't have to remember "mode is only on scan") but
    // don't gate behavior on it. Mismatches between the scan's
    // mode and dedupe's --mode aren't surfaced; the results file's
    // similarity_kind discriminator is the authoritative signal.
    let _ = args.mode;

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
    if outcome.skipped_decode_warning > 0 {
        writeln!(
            stderr,
            "Excluded from permanent removal (decode warning): {} — re-run with --action recycle to remove these reversibly.",
            outcome.skipped_decode_warning,
        )?;
    }
    if outcome.skipped_placeholder > 0 {
        writeln!(
            stderr,
            "Refused (cloud placeholder / reparse): {} — these were left untouched to avoid recalling or corrupting non-resident data.",
            outcome.skipped_placeholder,
        )?;
    }
    if outcome.skipped_keeper_identity > 0 {
        writeln!(
            stderr,
            "Refused (resolves to the keeper): {} — a member was the keeper reached via a path alias; destructive action refused to avoid deleting the keeper.",
            outcome.skipped_keeper_identity,
        )?;
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn group_of(files: &[&str]) -> pipeline::DuplicateGroup {
        pipeline::DuplicateGroup {
            files: files.iter().map(PathBuf::from).collect(),
            ..Default::default()
        }
    }

    // F-CLI-7 — production-side reference resolution decides which files
    // get MARKED reference (→ kept vs deleted), so it's regression-locked
    // here rather than resting on the comment's component-wise claim.
    #[test]
    fn resolve_reference_paths_includes_under_root_excludes_others() {
        let roots = vec![PathBuf::from("/data/refs")];
        let groups = vec![group_of(&["/data/refs/a.jpg", "/data/other/b.jpg"])];
        let got = resolve_reference_paths(&roots, &groups);
        assert_eq!(got, vec![PathBuf::from("/data/refs/a.jpg")]);
    }

    #[test]
    fn resolve_reference_paths_sibling_prefix_is_not_under_root() {
        // The load-bearing component-wise guarantee: `/data/refs` must NOT
        // prefix `/data/refs-backup`. A string-prefix compare would wrongly
        // mark the backup copy as reference and keep it.
        let roots = vec![PathBuf::from("/data/refs")];
        let groups = vec![group_of(&["/data/refs-backup/c.jpg"])];
        assert!(resolve_reference_paths(&roots, &groups).is_empty());
    }

    #[test]
    fn resolve_reference_paths_empty_roots_yields_empty() {
        let groups = vec![group_of(&["/data/refs/a.jpg"])];
        assert!(resolve_reference_paths(&[], &groups).is_empty());
    }

    // The S15 normalize: a verbatim-prefixed scanned path must still match
    // a non-verbatim reference root. Only meaningful on Windows, where the
    // backslash is a path separator (so `starts_with` is component-wise).
    #[cfg(windows)]
    #[test]
    fn resolve_reference_paths_verbatim_member_matches_plain_root() {
        let roots = vec![PathBuf::from(r"C:\refs")];
        let verbatim = r"\\?\C:\refs\a.jpg";
        let groups = vec![group_of(&[verbatim])];
        // Returns the ORIGINAL (verbatim) path, not the normalized form.
        assert_eq!(
            resolve_reference_paths(&roots, &groups),
            vec![PathBuf::from(verbatim)]
        );
    }
}
