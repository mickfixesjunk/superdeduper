//! `superdupe-gui` — desktop window for the engine.
//!
//! By default launches into an idle "pick a folder to scan" state.
//! Pass `--demo` to replay the synthetic engine for screenshots; pass
//! `--live <PATH>…` to start a real scan immediately on those paths.

use std::path::PathBuf;

use clap::Parser;

use superdupe::gui::SuperdupeApp;

#[derive(Debug, Parser)]
#[command(name = "superdupe-gui", version, about = "superdupe desktop GUI")]
struct Args {
    /// Run a real scan against these paths instead of waiting for the
    /// user to click "Scan".
    #[arg(long)]
    live: bool,
    /// Replay the synthetic demo engine. Useful for screenshots.
    #[arg(long, conflicts_with = "live")]
    demo: bool,
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
            .with_title("superdupe"),
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
    let start_demo = args.demo;

    eframe::run_native(
        "superdupe",
        native_options,
        Box::new(move |cc| {
            let mut app = SuperdupeApp::new(cc, start_demo);
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
