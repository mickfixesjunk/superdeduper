//! The `eframe::App` that lays out and drives the GUI.
//!
//! Layout:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │  header (status, totals)                                         │
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

use crate::gui::events::EngineEvent;
use crate::gui::state::UiState;
use crate::gui::widgets::{drive_scope, funnel, groups_table, header, treemap};
use crate::gui::{demo, theme};

#[derive(Copy, Clone, PartialEq, Eq)]
enum ResultsTab {
    Treemap,
    Groups,
}

pub struct SuperdupeApp {
    state: UiState,
    rx: Receiver<EngineEvent>,
    tx: Sender<EngineEvent>,
    demo_mode: bool,
    results_tab: ResultsTab,
    groups_state: groups_table::GroupsTableState,
}

impl SuperdupeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, demo_mode: bool) -> Self {
        theme::install(&cc.egui_ctx);

        let (tx, rx) = crossbeam_channel::bounded::<EngineEvent>(4096);
        if demo_mode {
            demo::spawn(tx.clone());
        }
        Self {
            state: UiState::default(),
            rx,
            tx,
            demo_mode,
            results_tab: ResultsTab::Treemap,
            groups_state: groups_table::GroupsTableState::default(),
        }
    }

    /// Engine-event sender for callers who want to feed the GUI directly
    /// (e.g. wiring a real scan in instead of demo mode).
    pub fn sender(&self) -> Sender<EngineEvent> {
        self.tx.clone()
    }

    fn drain_events(&mut self) {
        // Cap per-frame drain so we never starve the render loop on a
        // backlog. The bounded channel will apply backpressure.
        for _ in 0..512 {
            match self.rx.try_recv() {
                Ok(ev) => self.state.apply(ev),
                Err(_) => break,
            }
        }
        let now = Instant::now();
        for drive in self.state.drives.values_mut() {
            drive.roll_throughput(now);
        }
    }
}

impl eframe::App for SuperdupeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        // Request continuous repaints while a scan is in flight.
        if self.state.scan_finished_at.is_none() && self.state.scan_started_at.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        TopBottomPanel::top("header")
            .frame(Frame::default().fill(theme::BG).inner_margin(8.0))
            .show(ctx, |ui| header::show(ui, &self.state, self.demo_mode));

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

                match self.results_tab {
                    ResultsTab::Treemap => treemap::show(ui, &self.state),
                    ResultsTab::Groups => {
                        groups_table::show(ui, &self.state, &mut self.groups_state)
                    }
                }
            });
    }
}
