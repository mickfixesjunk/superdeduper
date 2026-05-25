//! `superdeduper-gui` — desktop window for the engine.
//!
//! Default: idle "pick a folder to scan" state. Pass `--live <PATH>…`
//! to start a real scan immediately on those paths.
//!
//! `windows_subsystem = "windows"` keeps the GUI binary from spawning
//! a black console window before the eframe viewport appears.
//! Debugging stdout/stderr still works if you launch it from an
//! existing terminal (PowerShell / cmd) — Windows only hides the
//! *auto-allocated* console.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::PathBuf;

use clap::Parser;

use superdeduper::gui::SuperdeduperApp;

#[derive(Debug, Parser)]
#[command(name = "superdeduper-gui", version, about = "superdeduper desktop GUI")]
struct Args {
    /// Run a real scan against these paths instead of waiting for the
    /// user to click "Scan".
    #[arg(long)]
    live: bool,
    /// Paths to scan when --live is set, or to seed the roots panel.
    #[arg(value_name = "PATHS")]
    paths: Vec<PathBuf>,
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    // Channel resolution: ENV var > [network] channel in persisted
    // config > default `prod`. The GUI binary doesn't expose a
    // --channel flag (the Settings → Network panel handles switching
    // mid-session); a fresh GUI launch otherwise inherits the same
    // precedence chain as the CLI per dev-channel-spec.md §3.3.
    let active = superdeduper::channel::resolve_active_channel(None).unwrap_or_else(|e| {
        eprintln!("channel resolution failed: {e}; defaulting to prod");
        superdeduper::channel::Channel::Prod
    });
    superdeduper::channel::set_active_channel(active);
    // Persistence diagnostic for the "linked status not showing
    // after restart" bug (Mick 2026-05-25T01:35Z). Logs which
    // channel was resolved + whether the corresponding oauth
    // token file exists. Helps localise whether the bug is on
    // the save side, the channel-resolution side, or the read
    // side.
    #[cfg(feature = "telemetry")]
    {
        let oauth_path =
            superdeduper::leaderboard::oauth::oauth_path_for(active).ok();
        let install_path =
            superdeduper::leaderboard::install::install_path_for(active).ok();
        superdeduper::leaderboard::oauth::log_oauth_event(&format!(
            "startup: channel={active} oauth_path={} oauth_exists={} install_path={} install_exists={}",
            oauth_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<error>".to_string()),
            oauth_path.as_ref().map(|p| p.exists()).unwrap_or(false),
            install_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<error>".to_string()),
            install_path.as_ref().map(|p| p.exists()).unwrap_or(false),
        ));
    }

    // Window title carries version + git SHA so multi-window users
    // can disambiguate which build is which without us shipping
    // uniquely-named EXEs. SD_BUILD_SHA is set at compile-time by
    // build.rs ("dev" if git isn't available).
    let window_title = format!(
        "SuperDeDuper v{} · {}",
        env!("CARGO_PKG_VERSION"),
        env!("SD_BUILD_SHA"),
    );
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1100.0, 720.0])
            .with_title(&window_title),
        vsync: true,
        ..Default::default()
    };

    let live_paths = if args.live && !args.paths.is_empty() {
        Some(args.paths.clone())
    } else {
        None
    };
    let seed_roots = if !args.live {
        args.paths.clone()
    } else {
        Vec::new()
    };

    eframe::run_native(
        "superdeduper",
        native_options,
        Box::new(move |cc| {
            let mut app = SuperdeduperApp::new(cc);
            for p in &seed_roots {
                app.add_root(p.clone(), false);
            }
            if let Some(paths) = live_paths.clone() {
                for p in &paths {
                    app.add_root(p.clone(), false);
                }
                app.start_live();
            }
            Ok(Box::new(app))
        }),
    )
}
