//! The `eframe::App` that lays out and drives the GUI.
//!
//! Layout (v0.1.3):
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────┐
//! │  header (logo, ⚙ settings, status, totals)                       │
//! ├──────────────┬───────────────────────────────────────────────────┤
//! │              │  drive scope                                      │
//! │  Roots       │                                                   │
//! │              │                                                   │
//! │  ──────      │                                                   │
//! │              ├───────────────────────────────────────────────────┤
//! │  Pipeline    │  Treemap | Groups | Log                           │
//! │              │                                                   │
//! └──────────────┴───────────────────────────────────────────────────┘
//! ```

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use egui::{CentralPanel, Frame, SidePanel, TopBottomPanel};

use crate::cli::DedupeAction;
use crate::gui::events::EngineEvent;
use crate::gui::state::{RootEntry, ScanSettings, UiState};
use crate::gui::widgets::groups_table::GroupAction;
use crate::gui::widgets::roots_panel::RootsAction;
use crate::gui::widgets::{
    drive_scope, funnel, groups_table, header, log_panel, overall_bar, roots_panel,
    settings_modal, treemap,
};
use crate::gui::{demo, live, theme};

#[derive(Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(Default)]
enum ResultsTab {
    #[default]
    Treemap,
    Groups,
    Log,
}


#[derive(Default, serde::Serialize, serde::Deserialize)]
struct PersistedAppState {
    roots: Vec<RootEntry>,
    settings: ScanSettings,
    results_tab: ResultsTab,
}

pub struct SuperdupeApp {
    state: UiState,
    rx: Receiver<EngineEvent>,
    tx: Sender<EngineEvent>,
    is_scanning: bool,
    demo_mode: bool,
    settings_open: bool,
    persisted: PersistedAppState,
    groups_state: groups_table::GroupsTableState,
    /// Cancel-token; the engine checks it cooperatively to honour
    /// Pause / Cancel from the UI.
    cancel: Arc<AtomicBool>,
    /// True iff a checkpoint from a prior (paused or crashed) scan
    /// is sitting on disk and the current roots+settings match it.
    /// Drives the "Resume" label on the primary scan button.
    can_resume: bool,
    /// Drive scope filter: when the user clicks a drive panel, only
    /// duplicate groups whose files live on that drive (plus any
    /// reference paths) are shown in Groups / Treemap.
    selected_drive: Option<u32>,
    /// Per-drive manual override of the auto-detected HDD/SSD render.
    /// `Some(true)` = force SSD render (scatter via hashed LCN);
    /// `Some(false)` = force HDD render (diagonal). `None` = use the
    /// detected `has_seek_penalty` value. Lives outside `persisted`
    /// because drive ids change between scans.
    drive_render_overrides: hashbrown::HashMap<u32, bool>,
}

impl SuperdupeApp {
    pub fn new(cc: &eframe::CreationContext<'_>, start_in_demo: bool) -> Self {
        theme::install(&cc.egui_ctx);
        let persisted: PersistedAppState = cc
            .storage
            .and_then(|s| eframe::get_value::<PersistedAppState>(s, "superdupe.app.v1"))
            .unwrap_or_default();
        let (tx, rx) = crossbeam_channel::bounded::<EngineEvent>(4096);
        let can_resume = check_resumable(&persisted);
        // If a prior scan's results match the currently-configured
        // roots+settings AND the folder fingerprint still matches
        // what we saw, restore the duplicate list straight into the
        // UI so the user can act on it without re-scanning.
        let saved_results = crate::gui::results_store::load_matching(
            &persisted.roots,
            &persisted.settings,
        )
        .ok()
        .flatten();
        let mut app = Self {
            state: UiState::default(),
            rx,
            tx,
            is_scanning: false,
            demo_mode: false,
            settings_open: false,
            persisted,
            groups_state: groups_table::GroupsTableState::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            can_resume,
            selected_drive: None,
            drive_render_overrides: hashbrown::HashMap::new(),
        };
        if can_resume {
            app.state.push_log(
                crate::gui::events::LogLevel::Info,
                "A paused scan was found on disk. Click Resume to continue.".into(),
            );
        }
        // Restore prior scan results, if they're still valid.
        if let Some(saved) = saved_results {
            let dup_count = saved.duplicates.len();
            for g in saved.duplicates {
                let savings = g.size.saturating_mul(g.files.len().saturating_sub(1) as u64);
                app.state.totals.duplicates =
                    app.state.totals.duplicates.saturating_add(1);
                app.state.totals.reclaimable_bytes =
                    app.state.totals.reclaimable_bytes.saturating_add(savings);
                app.state.duplicates.push(g);
            }
            app.state.push_log(
                crate::gui::events::LogLevel::Info,
                format!(
                    "Restored {} duplicate group(s) from a prior scan — folders haven't changed. Safe-rename / Unsuperdupe pick up where you left off.",
                    dup_count
                ),
            );
            // Land the user on Groups so the restored content is visible.
            app.persisted.results_tab = ResultsTab::Groups;
        }
        if start_in_demo {
            app.start_demo();
        }
        app
    }

    pub fn sender(&self) -> Sender<EngineEvent> {
        self.tx.clone()
    }

    pub fn add_root(&mut self, path: PathBuf, is_reference: bool) {
        if self.persisted.roots.iter().any(|r| r.path == path) {
            return;
        }
        self.persisted.roots.push(RootEntry { path, is_reference });
    }

    pub fn start_live(&mut self) {
        if self.is_scanning || self.persisted.roots.is_empty() {
            return;
        }
        self.is_scanning = true;
        self.demo_mode = false;
        self.cancel.store(false, Ordering::Relaxed);
        // Once the engine completes successfully, it deletes the
        // checkpoint file; clear our flag so the next launch doesn't
        // see a stale Resume.
        self.can_resume = false;
        live::spawn_with_settings(
            self.tx.clone(),
            self.persisted.roots.clone(),
            self.persisted.settings.clone(),
            self.cancel.clone(),
        );
    }

    pub fn start_demo(&mut self) {
        if self.is_scanning {
            return;
        }
        self.is_scanning = true;
        self.demo_mode = true;
        demo::spawn(self.tx.clone());
    }

    fn request_pause(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    fn drain_events(&mut self) {
        let mut scan_just_finished = false;
        for _ in 0..512 {
            match self.rx.try_recv() {
                Ok(ev) => {
                    match &ev {
                        EngineEvent::ScanStarted { .. } => self.is_scanning = true,
                        EngineEvent::ScanFinished { .. } => {
                            self.is_scanning = false;
                            self.persisted.results_tab = ResultsTab::Groups;
                            self.groups_state = groups_table::GroupsTableState::default();
                            scan_just_finished = true;
                        }
                        EngineEvent::ScanPaused { .. } => {
                            self.is_scanning = false;
                        }
                        _ => {}
                    }
                    self.state.apply(ev);
                }
                Err(_) => break,
            }
        }
        if scan_just_finished {
            // Persist results + per-root fingerprint in the background
            // so safe-rename / Unsuperdupe pick up where we left off
            // after a restart.
            self.persist_results_after_scan();
        }
        let now = Instant::now();
        for drive in self.state.drives.values_mut() {
            drive.roll_throughput(now);
        }
    }

    fn dispatch_root_action(&mut self, action: RootsAction) {
        match action {
            RootsAction::PickFolder => {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("Pick a folder to scan")
                    .pick_folder()
                {
                    self.add_root(p, false);
                }
            }
            RootsAction::PickReferenceFolder => {
                if let Some(p) = rfd::FileDialog::new()
                    .set_title("Pick a reference folder (never deleted from)")
                    .pick_folder()
                {
                    self.add_root(p, true);
                }
            }
            RootsAction::Remove(i) => {
                if i < self.persisted.roots.len() {
                    self.persisted.roots.remove(i);
                }
            }
            RootsAction::ToggleReference(i) => {
                if let Some(r) = self.persisted.roots.get_mut(i) {
                    r.is_reference = !r.is_reference;
                }
            }
            RootsAction::StartScan => self.start_live(),
            RootsAction::Pause => self.request_pause(),
            RootsAction::Cancel => {
                self.request_pause();
                self.persisted.results_tab = ResultsTab::Log;
            }
            RootsAction::Unsuperdupe => self.run_unsuperdupe_threaded(),
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
            GroupAction::SafeRenameOthers { keeper, dupes } => {
                self.run_action_threaded(DedupeAction::SafeRename, keeper, dupes);
            }
            GroupAction::SafeRenameAllVisible => {
                self.run_safe_rename_all_threaded();
            }
        }
    }

    /// Iterate every duplicate group currently in `self.state` and
    /// safe-rename every non-keeper that isn't a reference path. Runs
    /// on a worker thread so the UI keeps responding.
    fn run_safe_rename_all_threaded(&self) {
        let tx = self.tx.clone();
        let reference_roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .filter(|r| r.is_reference)
            .map(|r| r.path.clone())
            .collect();
        let groups: Vec<(PathBuf, Vec<PathBuf>)> = self
            .state
            .duplicates
            .iter()
            .filter_map(|g| {
                if g.files.len() < 2 {
                    return None;
                }
                let keeper = g.files[0].clone();
                // Drop any file under a reference root: those are
                // keepers by definition and must never be renamed.
                let dupes: Vec<PathBuf> = g.files[1..]
                    .iter()
                    .filter(|p| !reference_roots.iter().any(|r| p.starts_with(r)))
                    .cloned()
                    .collect();
                if dupes.is_empty() {
                    None
                } else {
                    Some((keeper, dupes))
                }
            })
            .collect();
        let total: u64 = groups.iter().map(|(_, d)| d.len() as u64).sum();
        std::thread::Builder::new()
            .name("superdupe-safe-rename-all".into())
            .spawn(move || {
                let _ = tx.send(EngineEvent::Status(format!(
                    "Safe-renaming {} file(s) across {} group(s)…",
                    total,
                    groups.len()
                )));
                let mut done = 0u64;
                let mut failed = 0u64;
                let mut skipped = 0u64;
                let mut renamed_paths: Vec<PathBuf> = Vec::new();
                for (_keeper, dupes) in &groups {
                    for d in dupes {
                        match crate::dedupe::action_safe_rename(d) {
                            Ok(()) => {
                                done += 1;
                                renamed_paths.push(d.clone());
                            }
                            Err(e) => {
                                let msg = e.to_string();
                                if msg.contains("already exists") {
                                    skipped += 1;
                                } else {
                                    failed += 1;
                                    let _ = tx.send(EngineEvent::Log {
                                        level: crate::gui::events::LogLevel::Warn,
                                        message: format!(
                                            "safe-rename failed · {} · {e}",
                                            d.display()
                                        ),
                                    });
                                }
                            }
                        }
                    }
                }
                // Persist the renamed set so a restart picks up where
                // we left off without re-scanning.
                if !renamed_paths.is_empty() {
                    if let Ok(Some(mut state)) = crate::gui::results_store::load() {
                        state.renamed_paths.extend(renamed_paths);
                        if let Err(e) = crate::gui::results_store::save(&state) {
                            tracing::warn!(
                                error = %e,
                                "safe-rename-all: results-state save failed"
                            );
                        }
                    }
                }
                let _ = tx.send(EngineEvent::Status(format!(
                    "Safe-rename complete · {} renamed, {} skipped, {} failed.",
                    done, skipped, failed
                )));
                let _ = tx.send(EngineEvent::Log {
                    level: crate::gui::events::LogLevel::Info,
                    message: format!(
                        "safe-rename · renamed={done} skipped={skipped} failed={failed}"
                    ),
                });
            })
            .expect("spawn safe-rename-all thread");
    }

    /// Snapshot the current duplicate list + roots + settings and
    /// compute a per-root fingerprint, then write the whole bundle to
    /// `%LOCALAPPDATA%\superdupe\results-state.json` on a background
    /// thread. Used right after a scan finishes so the next launch
    /// can restore the duplicate list without re-scanning, provided
    /// the folders haven't drifted.
    fn persist_results_after_scan(&self) {
        let duplicates = self.state.duplicates.clone();
        let roots = self.persisted.roots.clone();
        let settings = self.persisted.settings.clone();
        std::thread::Builder::new()
            .name("superdupe-results-save".into())
            .spawn(move || {
                let fingerprints = roots
                    .iter()
                    .map(|r| crate::gui::results_store::fingerprint_root(&r.path))
                    .collect();
                let state = crate::gui::results_store::ResultsState::new(
                    roots,
                    settings,
                    duplicates,
                    fingerprints,
                );
                if let Err(e) = crate::gui::results_store::save(&state) {
                    tracing::warn!(error = %e, "results-state save failed");
                }
            })
            .expect("spawn results-save thread");
    }

    /// Walk every root (incl. reference) and rename any
    /// `*.superdupe` file back to its original. No prior scan needed.
    fn run_unsuperdupe_threaded(&self) {
        let tx = self.tx.clone();
        let roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .map(|r| r.path.clone())
            .collect();
        std::thread::Builder::new()
            .name("superdupe-unsuperdupe".into())
            .spawn(move || {
                let _ = tx.send(EngineEvent::Status(format!(
                    "Unsuperduping {} root(s)…",
                    roots.len()
                )));
                let mut total_renamed = 0u64;
                let mut total_skipped = 0u64;
                let mut total_errors = 0u64;
                for r in &roots {
                    match crate::dedupe::unsuperdupe_root(r) {
                        Ok((renamed, skipped, errors)) => {
                            total_renamed += renamed;
                            total_skipped += skipped;
                            total_errors += errors;
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Info,
                                message: format!(
                                    "unsuperdupe · {} · renamed={renamed} skipped={skipped} errors={errors}",
                                    r.display()
                                ),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Error,
                                message: format!(
                                    "unsuperdupe failed · {} · {e}",
                                    r.display()
                                ),
                            });
                            total_errors += 1;
                        }
                    }
                }
                // Renamed_paths in the saved state no longer reflects
                // reality — every `.superdupe` file just got restored.
                // Clear the renamed list (but keep the duplicates so
                // the user can act on them again if they want).
                if let Ok(Some(mut state)) = crate::gui::results_store::load() {
                    state.renamed_paths.clear();
                    let _ = crate::gui::results_store::save(&state);
                }
                let _ = tx.send(EngineEvent::Status(format!(
                    "Unsuperdupe complete · {} renamed, {} skipped, {} errors.",
                    total_renamed, total_skipped, total_errors,
                )));
            })
            .expect("spawn unsuperdupe thread");
    }

    fn run_action_threaded(
        &self,
        action: DedupeAction,
        keeper: PathBuf,
        dupes: Vec<PathBuf>,
    ) {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("superdupe-action".into())
            .spawn(move || {
                let mut done = 0u64;
                let mut failed = 0u64;
                let mut renamed_paths: Vec<PathBuf> = Vec::new();
                let _ = tx.send(EngineEvent::Status(format!(
                    "{:?}: {} file(s) → keeper {}",
                    action,
                    dupes.len(),
                    keeper.display()
                )));
                for d in &dupes {
                    let r = match action {
                        DedupeAction::Recycle => crate::dedupe::action_recycle(d),
                        DedupeAction::Hardlink => crate::dedupe::action_hardlink(d, &keeper),
                        DedupeAction::Remove => crate::dedupe::action_remove(d),
                        DedupeAction::Reflink => crate::dedupe::action_reflink(d, &keeper),
                        DedupeAction::SafeRename => crate::dedupe::action_safe_rename(d),
                    };
                    match r {
                        Ok(()) => {
                            done += 1;
                            if matches!(action, DedupeAction::SafeRename) {
                                renamed_paths.push(d.clone());
                            }
                        }
                        Err(e) => {
                            failed += 1;
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Error,
                                message: format!("{}: {e}", d.display()),
                            });
                        }
                    }
                }
                if !renamed_paths.is_empty() {
                    if let Ok(Some(mut state)) = crate::gui::results_store::load() {
                        state.renamed_paths.extend(renamed_paths);
                        let _ = crate::gui::results_store::save(&state);
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

impl eframe::App for SuperdupeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        if self.is_scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // Settings modal first; it doesn't claim screen real estate.
        if self.settings_open {
            let mut open = self.settings_open;
            if settings_modal::show(ctx, &mut open, &mut self.persisted.settings) {
                self.settings_open = false;
            } else {
                self.settings_open = open;
            }
        }

        let mut want_settings = false;
        let mut want_demo = false;
        TopBottomPanel::top("header")
            .frame(Frame::default().fill(theme::BG).inner_margin(8.0))
            .show(ctx, |ui| {
                let action = header::show(ui, &self.state, self.demo_mode, self.is_scanning);
                match action {
                    header::HeaderAction::OpenSettings => want_settings = true,
                    header::HeaderAction::StartDemo => want_demo = true,
                    _ => {}
                }
            });
        if want_settings {
            self.settings_open = true;
        }
        if want_demo {
            self.start_demo();
        }

        // Overall progress strip sits directly under the header and
        // spans the full window so the user always sees "what stage,
        // how much, how long" without hunting.
        TopBottomPanel::top("overall-progress")
            .frame(Frame::default().fill(theme::BG).inner_margin(egui::vec2(8.0, 4.0)))
            .show(ctx, |ui| {
                overall_bar::show(ui, &self.state);
            });

        SidePanel::left("sidebar")
            .resizable(true)
            .default_width(300.0)
            .min_width(240.0)
            .frame(Frame::default().fill(theme::PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                let roots_action = roots_panel::show(
                    ui,
                    &self.persisted.roots,
                    self.is_scanning,
                    self.can_resume,
                );
                if let Some(a) = roots_action {
                    self.dispatch_root_action(a);
                }
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);
                funnel::show(ui, &self.state, self.persisted.settings.hash_algo);
            });

        CentralPanel::default()
            .frame(Frame::default().fill(theme::PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                let avail = ui.available_height();
                let scope_h = (avail * 0.50).clamp(380.0, 580.0);
                let mut drive_clicked: Option<u32> = None;
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), scope_h),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        egui::ScrollArea::vertical()
                            .id_source("drive-scope")
                            .show(ui, |ui| {
                                drive_clicked = drive_scope::show(
                                    ui,
                                    &self.state,
                                    self.selected_drive,
                                    &mut self.drive_render_overrides,
                                );
                            });
                    },
                );
                if let Some(id) = drive_clicked {
                    self.selected_drive = if self.selected_drive == Some(id) {
                        None
                    } else {
                        Some(id)
                    };
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let mut pick = |label, value| {
                        let selected = self.persisted.results_tab == value;
                        if ui.selectable_label(selected, label).clicked() {
                            self.persisted.results_tab = value;
                        }
                    };
                    pick("Treemap", ResultsTab::Treemap);
                    pick("Groups", ResultsTab::Groups);
                    pick("Log", ResultsTab::Log);
                    // Filter chip — visible whenever a drive is selected.
                    if let Some(id) = self.selected_drive {
                        let label = self
                            .state
                            .drives
                            .get(&id)
                            .map(|d| {
                                format!(
                                    "  ✕ filtered to: {}",
                                    d.info.volume_label
                                )
                            })
                            .unwrap_or_else(|| "  ✕ clear filter".into());
                        if ui
                            .selectable_label(true, label)
                            .on_hover_text("Click to clear the drive filter.")
                            .clicked()
                        {
                            self.selected_drive = None;
                        }
                    }
                });
                ui.add_space(2.0);

                let reference_roots: Vec<PathBuf> = self
                    .persisted
                    .roots
                    .iter()
                    .filter(|r| r.is_reference)
                    .map(|r| r.path.clone())
                    .collect();
                let drive_root = self.selected_drive.and_then(|id| {
                    self.persisted.roots.get(id as usize).map(|r| r.path.clone())
                });
                let mut group_action: Option<GroupAction> = None;
                match self.persisted.results_tab {
                    ResultsTab::Treemap => treemap::show_filtered(
                        ui,
                        &self.state,
                        drive_root.as_deref(),
                        &reference_roots,
                    ),
                    ResultsTab::Groups => {
                        group_action = groups_table::show_filtered(
                            ui,
                            &self.state,
                            &mut self.groups_state,
                            drive_root.as_deref(),
                            &reference_roots,
                        );
                    }
                    ResultsTab::Log => log_panel::show(ui, &self.state),
                }
                if let Some(a) = group_action {
                    self.dispatch_group_action(a);
                }
            });
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "superdupe.app.v1", &self.persisted);
    }
}

fn check_resumable(persisted: &PersistedAppState) -> bool {
    use crate::gui::checkpoint;
    let Ok(path) = checkpoint::default_checkpoint_path() else {
        return false;
    };
    match checkpoint::load(&path) {
        Ok(Some(cp)) => {
            cp.roots == persisted.roots && cp.settings == persisted.settings
        }
        _ => false,
    }
}

#[cfg(windows)]
fn reveal_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

#[cfg(not(windows))]
fn reveal_in_explorer(_path: &std::path::Path) {}
