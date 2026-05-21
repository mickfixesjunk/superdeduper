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

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1100.0, 720.0])
            .with_title("superdeduper"),
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
