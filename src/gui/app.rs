//! The `eframe::App` that lays out and drives the GUI.
//!
//! Layout:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │  header (Scan button, Demo button, status, totals)               │
//! ├──────────────┬───────────────────────────────────────────────────┤
//! │              │  drive scope (per-physical-drive)                 │
//! │  pipeline    │                                                   │
//! │  funnel      ├───────────────────────────────────────────────────┤
//! │              │  results — Treemap | Groups table                 │
//! └──────────────┴───────────────────────────────────────────────────┘
//! ```

use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use egui::{CentralPanel, Frame, SidePanel, TopBottomPanel};

use crate::cli::DedupeAction;
use crate::gui::events::EngineEvent;
use crate::gui::state::UiState;
use crate::gui::widgets::groups_table::GroupAction;
use crate::gui::widgets::header::HeaderAction;
use crate::gui::widgets::{drive_scope, funnel, groups_table, header, treemap};
use crate::gui::{demo, live, theme};

#[derive(Copy, Clone, PartialEq, Eq)]
enum ResultsTab {
    Treemap,
    Groups,
}

pub struct SuperdupeApp {
    state: UiState,
    rx: Receiver<EngineEvent>,
    tx: Sender<EngineEvent>,
    /// True if a demo or live engine thread is currently producing
    /// events. Set when the user clicks Scan/Demo (or when launched
    /// with `--live`), cleared on `ScanFinished`.
    is_scanning: bool,
    /// Most recent scan was the synthetic demo. Used for the "DEMO"
    /// header badge.
    demo_mode: bool,
    results_tab: ResultsTab,
    groups_state: groups_table::GroupsTableState,
}

impl SuperdupeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, start_in_demo: bool) -> Self {
        theme::install(&cc.egui_ctx);
        let (tx, rx) = crossbeam_channel::bounded::<EngineEvent>(4096);
        let mut app = Self {
            state: UiState::default(),
            rx,
            tx,
            is_scanning: false,
            demo_mode: false,
            results_tab: ResultsTab::Treemap,
            groups_state: groups_table::GroupsTableState::default(),
        };
        if start_in_demo {
            app.start_demo();
        }
        app
    }

    pub fn sender(&self) -> Sender<EngineEvent> {
        self.tx.clone()
    }

    /// Mark the app as scanning (so the user can't kick off another)
    /// and let the engine know to reset state via `ScanStarted` (the
    /// demo / live thread sends it before doing any work).
    pub fn start_live(&mut self, paths: Vec<std::path::PathBuf>) {
        if self.is_scanning || paths.is_empty() {
            return;
        }
        self.is_scanning = true;
        self.demo_mode = false;
        live::spawn(self.tx.clone(), paths);
    }

    pub fn start_demo(&mut self) {
        if self.is_scanning {
            return;
        }
        self.is_scanning = true;
        self.demo_mode = true;
        demo::spawn(self.tx.clone());
    }

    fn drain_events(&mut self) {
        for _ in 0..512 {
            match self.rx.try_recv() {
                Ok(ev) => {
                    match &ev {
                        EngineEvent::ScanStarted { .. } => self.is_scanning = true,
                        EngineEvent::ScanFinished { .. } => {
                            self.is_scanning = false;
                            // Surface the candidates as soon as the
                            // scan completes — that's the entire
                            // point of running the tool.
                            self.results_tab = ResultsTab::Groups;
                            self.groups_state = groups_table::GroupsTableState::default();
                        }
                        _ => {}
                    }
                    self.state.apply(ev);
                }
                Err(_) => break,
            }
        }
        let now = Instant::now();
        for drive in self.state.drives.values_mut() {
            drive.roll_throughput(now);
        }
    }

    fn dispatch_group_action(&mut self, action: GroupAction) {
        match action {
            GroupAction::RecycleOthers { keeper, dupes } => {
                self.run_action_threaded(DedupeAction::Recycle, keeper, dupes);
            }
            GroupAction::HardlinkOthers { keeper, dupes } => {
                self.run_action_threaded(DedupeAction::Hardlink, keeper, dupes);
            }
            GroupAction::Reveal(path) => reveal_in_explorer(&path),
        }
    }

    /// Run a destructive dedupe action on a background thread so the UI
    /// stays responsive. Status updates are pushed back over the same
    /// event channel that drives the rest of the UI.
    fn run_action_threaded(
        &self,
        action: DedupeAction,
        keeper: std::path::PathBuf,
        dupes: Vec<std::path::PathBuf>,
    ) {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("superdupe-action".into())
            .spawn(move || {
                let mut done = 0u64;
                let mut failed = 0u64;
                let _ = tx.send(EngineEvent::Status(format!(
                    "{:?}: {} file(s) → keeper {}",
                    action,
                    dupes.len(),
                    keeper.display()
                )));
                for d in &dupes {
                    let r = match action {
                        DedupeAction::Recycle => crate::dedupe::action_recycle(d),
                        DedupeAction::Hardlink => {
                            crate::dedupe::action_hardlink(d, &keeper)
                        }
                        DedupeAction::Remove => crate::dedupe::action_remove(d),
                        DedupeAction::Reflink => crate::dedupe::action_reflink(d, &keeper),
                    };
                    match r {
                        Ok(()) => done += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::error!(path = %d.display(), error = %e, "action failed");
                        }
                    }
                }
                let _ = tx.send(EngineEvent::Status(format!(
                    "Action complete · {} done, {} failed.",
                    done, failed
                )));
            })
            .expect("spawn dedupe thread");
    }
}

#[cfg(windows)]
fn reveal_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

#[cfg(not(windows))]
fn reveal_in_explorer(_path: &std::path::Path) {
    // No-op on non-Windows; the GUI's production target is Windows.
}

impl eframe::App for SuperdupeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        if self.is_scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        let mut header_action = HeaderAction::None;
        TopBottomPanel::top("header")
            .frame(Frame::default().fill(theme::BG).inner_margin(8.0))
            .show(ctx, |ui| {
                header_action = header::show(ui, &self.state, self.demo_mode, self.is_scanning);
            });
        match header_action {
            HeaderAction::PickAndScan => {
                if let Some(folder) = rfd::FileDialog::new()
                    .set_title("Pick a folder to scan for duplicates")
                    .pick_folder()
                {
                    self.start_live(vec![folder]);
                }
            }
            HeaderAction::StartDemo => self.start_demo(),
            HeaderAction::None => {}
        }

        SidePanel::left("pipeline")
            .resizable(true)
            .default_width(280.0)
            .min_width(220.0)
            .frame(Frame::default().fill(theme::PANEL).inner_margin(10.0))
            .show(ctx, |ui| funnel::show(ui, &self.state));

        CentralPanel::default()
            .frame(Frame::default().fill(theme::PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                let avail = ui.available_height();
                let scope_h = (avail * 0.55).clamp(440.0, 620.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), scope_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_source("drive-scope")
                            .show(ui, |ui| drive_scope::show(ui, &self.state));
                    },
                );

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let mut pick = |label, value| {
                        let selected = self.results_tab == value;
                        if ui.selectable_label(selected, label).clicked() {
                            self.results_tab = value;
                        }
                    };
                    pick("Treemap", ResultsTab::Treemap);
                    pick("Groups", ResultsTab::Groups);
                });
                ui.add_space(2.0);

                let mut group_action: Option<GroupAction> = None;
                match self.results_tab {
                    ResultsTab::Treemap => treemap::show(ui, &self.state),
                    ResultsTab::Groups => {
                        group_action =
                            groups_table::show(ui, &self.state, &mut self.groups_state);
                    }
                }
                if let Some(a) = group_action {
                    self.dispatch_group_action(a);
                }
            });
    }
}
