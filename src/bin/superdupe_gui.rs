//! `superdupe-gui` — desktop window for the engine.
//!
//! Modes:
//!
//! * `superdupe-gui` (no args) — demo mode. A synthetic engine streams
//!   realistic events so the GUI is usable on any host without a real
//!   scan target. Great for screenshots and design iteration.
//! * `superdupe-gui --live <PATH>…` — wires the real engine to the
//!   same event channel. Identical UI, real data.

use std::path::PathBuf;

use clap::Parser;

use superdupe::gui::SuperdupeApp;

#[derive(Debug, Parser)]
#[command(name = "superdupe-gui", version, about = "superdupe desktop GUI")]
struct Args {
    /// Run a real scan against these paths instead of the demo.
    #[arg(long, conflicts_with = "demo")]
    live: bool,
    /// Force demo mode even with paths. Default when no --live flag.
    #[arg(long)]
    demo: bool,
    /// Paths to scan when --live is set.
    #[arg(value_name = "PATHS")]
    paths: Vec<PathBuf>,
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1100.0, 720.0])
            .with_title("superdupe"),
        vsync: true,
        ..Default::default()
    };

    let live_paths = if args.live { Some(args.paths) } else { None };
    let demo_mode = live_paths.is_none() || args.demo;

    eframe::run_native(
        "superdupe",
        native_options,
        Box::new(move |cc| {
            let app = SuperdupeApp::new(cc, demo_mode);
            if let Some(paths) = live_paths.clone() {
                if !paths.is_empty() {
                    superdupe::gui::live::spawn(app.sender(), paths);
                }
            }
            Ok(Box::new(app))
        }),
    )
}
