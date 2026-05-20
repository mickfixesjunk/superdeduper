//! `superdupe-gui` — desktop window for the engine.
//!
//! By default starts in **demo mode** so the GUI is visible without a
//! Windows host or a real scan target. Pass `--live` once the engine
//! integration is wired up.

use clap::Parser;

use superdupe::gui::SuperdupeApp;

#[derive(Debug, Parser)]
#[command(name = "superdupe-gui", version, about = "superdupe desktop GUI")]
struct Args {
    /// Run a real scan instead of the synthetic demo. (Not yet wired up.)
    #[arg(long, default_value_t = false)]
    live: bool,
}

fn main() -> eframe::Result<()> {
    let args = Args::parse();
    let demo_mode = !args.live;

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1100.0, 720.0])
            .with_title("superdupe"),
        vsync: true,
        ..Default::default()
    };

    eframe::run_native(
        "superdupe",
        native_options,
        Box::new(move |cc| Ok(Box::new(SuperdupeApp::new(cc, demo_mode)))),
    )
}
