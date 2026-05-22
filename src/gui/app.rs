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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use egui::{CentralPanel, Frame, SidePanel, TopBottomPanel};

use crate::cli::DedupeAction;
use crate::gui::events::EngineEvent;
use crate::gui::state::{RootEntry, ScanSettings, UiState};
use crate::gui::widgets::groups_table::GroupAction;
use crate::gui::widgets::resume_modal::ResumeChoice;
use crate::gui::widgets::roots_panel::RootsAction;
use crate::gui::widgets::{
    drive_scope, funnel, groups_table, header, log_panel, overall_bar, resume_modal, roots_panel,
    settings_modal, treemap,
};
use crate::gui::{live, theme};

#[derive(Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
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

pub struct SuperdeduperApp {
    state: UiState,
    rx: Receiver<EngineEvent>,
    tx: Sender<EngineEvent>,
    is_scanning: bool,
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
    /// Launch-time Resume/Start-Fresh modal state. `Some` ⇒ a usable
    /// scan-checkpoint was found on disk and we're waiting for the
    /// user to pick. While this is `Some`, the rest of the UI stays
    /// behind the modal so Start Fresh can safely wipe state.
    pending_resume: Option<crate::gui::checkpoint::CheckpointSummary>,
    /// Filesystem path of the currently-open .superdeduper project
    /// folder. `None` ⇒ no project loaded (default on launch, and
    /// after File → New). Save Project writes here when present;
    /// when `None` it falls through to Save As behaviour.
    current_project_path: Option<PathBuf>,
    /// Unix seconds of the open project's first save, kept so a
    /// re-save preserves `created_at_unix`. Zero ⇒ no project.
    current_project_created_at: u64,
    /// Archive manifest the user just opened, waiting for explicit
    /// confirm before we start moving files back. `None` ⇒ no
    /// pending restore. The confirmation modal in `update()`
    /// renders summary + Restore / Cancel buttons.
    pending_archive_restore: Option<crate::gui::archive::ArchiveManifest>,
    /// Destructive group-action the user clicked, waiting on the
    /// "type DELETE" confirmation. `None` ⇒ no action pending.
    /// Bypassed when `settings.bypass_destructive_confirmation` is
    /// true; non-destructive Reveal-in-Explorer never lands here.
    pending_destructive: Option<groups_table::GroupAction>,
    /// Text the user has typed into the confirmation prompt. Must
    /// equal `"DELETE"` exactly before the Confirm button enables.
    /// Cleared every time the modal opens or closes.
    destructive_confirm_input: String,
}

/// Top-level File-menu actions the menubar can request. Dispatched
/// once per frame after rendering the menu so we don't mutate
/// `self` while drawing.
#[derive(Debug, Clone)]
enum MenuAction {
    /// Wipe the current project. Roots cleared, results cleared,
    /// settings kept. Cache untouched.
    New,
    /// Folder picker → load that `.superdeduper` bundle.
    OpenProject,
    /// Write to `current_project_path` if Some, else prompt.
    Save,
    /// Always prompt for a destination folder.
    SaveAs,
    /// Load an `archive-manifest.json` for display. Restore-to-
    /// originals loader not yet wired.
    OpenArchiveManifest,
    /// Open a specific bundle from the Recent Projects submenu.
    OpenRecent(PathBuf),
}

impl SuperdeduperApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let persisted: PersistedAppState = cc
            .storage
            .and_then(|s| eframe::get_value::<PersistedAppState>(s, "superdeduper.app.v1"))
            .unwrap_or_default();
        let (tx, rx) = crossbeam_channel::bounded::<EngineEvent>(4096);

        // Probe the scan-checkpoint file BEFORE any state restore so
        // we can show the launch-time Resume / Start Fresh modal.
        // The summary is cheap; the full restore only happens if the
        // user picks Resume.
        let pending_resume = match crate::gui::checkpoint::default_checkpoint_path() {
            Ok(path) => match crate::gui::checkpoint::summary(&path) {
                Ok(summary) => summary,
                Err(e) => {
                    // Corrupt or unparseable checkpoint: rename it
                    // with a .corrupt suffix so the user can inspect
                    // and we don't trip over it again. Proceed as if
                    // there was no checkpoint.
                    tracing::warn!(
                        path = %path.display(),
                        error = %e,
                        "checkpoint unparseable; renaming with .corrupt suffix"
                    );
                    let _ = crate::gui::checkpoint::mark_corrupt(&path);
                    None
                }
            },
            Err(_) => None,
        };

        let app = Self {
            state: UiState::default(),
            rx,
            tx,
            is_scanning: false,
            settings_open: false,
            persisted,
            groups_state: groups_table::GroupsTableState::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            can_resume: false,
            selected_drive: None,
            drive_render_overrides: hashbrown::HashMap::new(),
            pending_resume,
            current_project_path: None,
            current_project_created_at: 0,
            pending_archive_restore: None,
            pending_destructive: None,
            destructive_confirm_input: String::new(),
        };

        // Intentionally NO auto-load of prior scan results on launch.
        // Projects are now explicit — File → Open Project loads one;
        // File → New / a fresh launch starts empty. The Resume modal
        // still triggers for *interrupted* scans (paused checkpoints
        // are separate from saved projects) and the user can pick
        // Start Fresh from that modal to clear it.
        app
    }

    /// Apply the existing "load results_state.json if folders match"
    /// flow. Extracted so both the no-checkpoint launch path AND the
    /// post-modal Start Fresh path can reuse it (the latter just
    /// doesn't get here because Start Fresh wipes results_state out
    /// of view).
    fn auto_restore_results_state(&mut self) {
        let saved_results = crate::gui::results_store::load_matching(
            &self.persisted.roots,
            &self.persisted.settings,
        )
        .ok()
        .flatten();
        let can_resume = check_resumable(&self.persisted);
        self.can_resume = can_resume;
        if can_resume {
            self.state.push_log(
                crate::gui::events::LogLevel::Info,
                "A paused scan was found on disk. Click Resume to continue.".into(),
            );
        }
        if let Some(saved) = saved_results {
            let dup_count = saved.duplicates.len();
            for g in saved.duplicates {
                let savings = g
                    .size
                    .saturating_mul(g.files.len().saturating_sub(1) as u64);
                self.state.totals.duplicates = self.state.totals.duplicates.saturating_add(1);
                self.state.totals.reclaimable_bytes =
                    self.state.totals.reclaimable_bytes.saturating_add(savings);
                self.state.duplicates.push(g);
            }
            self.state.push_log(
                crate::gui::events::LogLevel::Info,
                format!(
                    "Restored {} duplicate group(s) from a prior scan — folders haven't changed. Safe-rename / Unsuperdeduper pick up where you left off.",
                    dup_count
                ),
            );
            self.persisted.results_tab = ResultsTab::Groups;
        }
    }

    /// User clicked Resume on the launch-time modal. Hydrate state
    /// from the on-disk checkpoint so the funnel/groups/log all show
    /// what the prior session ended with; the engine isn't auto-
    /// started — the user clicks the "Resume scan" button in the
    /// roots panel when they're ready to actually continue.
    fn accept_resume(&mut self) {
        let path = match crate::gui::checkpoint::default_checkpoint_path() {
            Ok(p) => p,
            Err(_) => return,
        };
        let cp = match crate::gui::checkpoint::load(&path) {
            Ok(Some(c)) => c,
            _ => return,
        };
        // Adopt the checkpoint's roots + settings so the Roots panel
        // matches the paused state.
        self.persisted.roots = cp.roots.clone();
        self.persisted.settings = cp.settings.clone();
        // Replay every confirmed duplicate so the Groups + Treemap
        // come back populated.
        let dup_count = cp.previous_duplicates.len();
        for g in &cp.previous_duplicates {
            let savings = g
                .size
                .saturating_mul(g.files.len().saturating_sub(1) as u64);
            self.state.totals.duplicates = self.state.totals.duplicates.saturating_add(1);
            self.state.totals.reclaimable_bytes =
                self.state.totals.reclaimable_bytes.saturating_add(savings);
            self.state.duplicates.push(g.clone());
        }
        self.state.push_log(
            crate::gui::events::LogLevel::Info,
            format!(
                "Resumed previous scan: {} duplicate group(s) restored{}",
                dup_count,
                if cp.saved_inventory.is_some() {
                    "; saved inventory present (next scan skips Stage 1)"
                } else {
                    ""
                },
            ),
        );
        self.persisted.results_tab = ResultsTab::Groups;
        self.can_resume = true;
        // Pull the regular results_store path back in too — if its
        // fingerprint also matches it's a freebie.
        self.auto_restore_results_state();
    }

    /// User clicked Start Fresh. Rename the checkpoint to a
    /// timestamped `.bak` (never delete!), wipe in-memory state, and
    /// leave the rusqlite hash cache untouched so the next scan
    /// silently benefits from prior work.
    fn accept_start_fresh(&mut self) {
        if let Ok(path) = crate::gui::checkpoint::default_checkpoint_path() {
            match crate::gui::checkpoint::archive(&path) {
                Ok(Some(archived)) => {
                    self.state.push_log(
                        crate::gui::events::LogLevel::Info,
                        format!(
                            "Previous checkpoint archived to {} — recover with a manual rename.",
                            archived.display()
                        ),
                    );
                }
                Ok(None) => {} // File vanished between summary() and now.
                Err(e) => {
                    self.state.push_log(
                        crate::gui::events::LogLevel::Warn,
                        format!("Couldn't archive previous checkpoint: {e}"),
                    );
                }
            }
        }
        // Wipe everything the user would consider "current results".
        // Settings + drive_render_overrides + persisted.results_tab
        // are intentionally kept — they're user preferences, not
        // results.
        self.state = UiState::default();
        self.persisted.roots = Vec::new();
        self.groups_state = groups_table::GroupsTableState::default();
        self.selected_drive = None;
        self.can_resume = false;
        self.state.push_log(
            crate::gui::events::LogLevel::Info,
            "Started fresh. Hash cache preserved — overlapping files will hit the cache on the next scan.".into(),
        );
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

    /// Route a File-menu action through the right handler. Pulled
    /// out of `update()` so the menu rendering and the state
    /// mutation aren't tangled. All handlers are synchronous —
    /// dialogs block the UI thread but that's expected (and matches
    /// every other "Pick a folder" path in this app).
    fn dispatch_menu_action(&mut self, action: MenuAction) {
        match action {
            MenuAction::New => self.menu_new(),
            MenuAction::OpenProject => self.menu_open_project(),
            MenuAction::Save => self.menu_save(),
            MenuAction::SaveAs => self.menu_save_as(),
            MenuAction::OpenArchiveManifest => self.menu_open_archive_manifest(),
            MenuAction::OpenRecent(path) => self.load_project_from(&path),
        }
    }

    /// File → New scan. Clears everything you'd consider current
    /// work, keeps settings + drive overrides (preferences). Cache
    /// is untouched.
    fn menu_new(&mut self) {
        if self.is_scanning {
            self.state.push_log(
                crate::gui::events::LogLevel::Warn,
                "Stop the running scan before starting a new project.".into(),
            );
            return;
        }
        self.state = UiState::default();
        self.persisted.roots = Vec::new();
        self.groups_state = groups_table::GroupsTableState::default();
        self.selected_drive = None;
        self.can_resume = false;
        self.current_project_path = None;
        self.current_project_created_at = 0;
        self.state.push_log(
            crate::gui::events::LogLevel::Info,
            "New scan — project cleared. Hash cache preserved.".into(),
        );
    }

    fn menu_open_project(&mut self) {
        let dir = match rfd::FileDialog::new()
            .set_title("Open superdeduper project — pick the .superdeduper folder")
            .pick_folder()
        {
            Some(p) => p,
            None => return,
        };
        self.load_project_from(&dir);
    }

    fn load_project_from(&mut self, dir: &Path) {
        match crate::gui::project::load(dir) {
            Ok((proj, duplicates)) => {
                // Replace state — opening a project should feel like
                // a clean slate, not an additive merge.
                self.state = UiState::default();
                self.persisted.roots = proj.roots.clone();
                self.persisted.settings = proj.settings.clone();
                self.groups_state = groups_table::GroupsTableState::default();
                self.selected_drive = None;
                self.can_resume = false;
                let n = duplicates.len();
                for g in duplicates {
                    let savings = g
                        .size
                        .saturating_mul(g.files.len().saturating_sub(1) as u64);
                    self.state.totals.duplicates = self.state.totals.duplicates.saturating_add(1);
                    self.state.totals.reclaimable_bytes =
                        self.state.totals.reclaimable_bytes.saturating_add(savings);
                    self.state.duplicates.push(g);
                }
                self.current_project_path = Some(dir.to_path_buf());
                self.current_project_created_at = proj.created_at_unix;
                self.persisted.results_tab = if n > 0 {
                    ResultsTab::Groups
                } else {
                    self.persisted.results_tab
                };
                self.state.push_log(
                    crate::gui::events::LogLevel::Info,
                    format!(
                        "Opened project {} — {} root(s), {} duplicate group(s).",
                        proj.name,
                        proj.roots.len(),
                        n
                    ),
                );
                let _ = crate::gui::project::touch_recent(dir, &proj.name);
            }
            Err(e) => {
                self.state.push_log(
                    crate::gui::events::LogLevel::Error,
                    format!("Open project failed: {e}"),
                );
            }
        }
    }

    fn menu_save(&mut self) {
        let dir = match self.current_project_path.clone() {
            Some(p) => p,
            None => return self.menu_save_as(), // no project open ⇒ Save As
        };
        self.save_project_to(&dir);
    }

    fn menu_save_as(&mut self) {
        let default_name = crate::gui::project::default_bundle_name(&self.persisted.roots);
        let dir = match rfd::FileDialog::new()
            .set_title(
                "Save superdeduper project — choose where to create the .superdeduper folder",
            )
            .set_file_name(&default_name)
            .save_file()
        {
            Some(p) => p,
            None => return,
        };
        // rfd's save_file returns a file path even when we want a
        // folder name — appending the .superdeduper suffix if missing
        // turns it into a bundle name. e.g. user types "weekly-scan"
        // → "weekly-scan.superdeduper/".
        let bundle = if dir.extension().and_then(|s| s.to_str()) == Some("superdeduper")
            || dir
                .file_name()
                .and_then(|s| s.to_str())
                .map(|s| s.ends_with(crate::gui::project::PROJECT_SUFFIX))
                .unwrap_or(false)
        {
            dir
        } else {
            let mut s = dir.into_os_string();
            s.push(crate::gui::project::PROJECT_SUFFIX);
            PathBuf::from(s)
        };
        self.save_project_to(&bundle);
    }

    fn save_project_to(&mut self, dir: &Path) {
        let name = dir
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("untitled")
            .to_string();
        match crate::gui::project::save(
            dir,
            &name,
            self.current_project_created_at,
            &self.persisted.roots,
            &self.persisted.settings,
            &self.state.duplicates,
        ) {
            Ok(()) => {
                self.current_project_path = Some(dir.to_path_buf());
                if self.current_project_created_at == 0 {
                    self.current_project_created_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                }
                self.state.push_log(
                    crate::gui::events::LogLevel::Info,
                    format!("Project saved · {}", dir.display()),
                );
                let _ = crate::gui::project::touch_recent(dir, &name);
            }
            Err(e) => {
                self.state.push_log(
                    crate::gui::events::LogLevel::Error,
                    format!("Save project failed: {e}"),
                );
            }
        }
    }

    fn menu_open_archive_manifest(&mut self) {
        let file = match rfd::FileDialog::new()
            .set_title("Open archive manifest")
            .add_filter("JSON", &["json"])
            .pick_file()
        {
            Some(p) => p,
            None => return,
        };
        let manifest = match crate::gui::archive::load_manifest(&file) {
            Ok(m) => m,
            Err(e) => {
                self.state.push_log(
                    crate::gui::events::LogLevel::Error,
                    format!("Manifest read failed: {e}"),
                );
                return;
            }
        };
        // Park the loaded manifest in a pending-restore slot. The
        // confirmation modal in `update()` will render summary +
        // Restore / Cancel buttons — we deliberately don't kick off
        // the move immediately so the user gets one explicit "yes"
        // step before files start moving.
        let n = manifest.entries.len();
        let total_bytes: u64 = manifest.entries.iter().map(|e| e.size).sum();
        self.state.push_log(
            crate::gui::events::LogLevel::Info,
            format!(
                "Archive manifest loaded · {} entries · {} · click Restore in the confirmation dialog.",
                n,
                humansize::format_size(total_bytes, humansize::BINARY)
            ),
        );
        self.pending_archive_restore = Some(manifest);
    }

    /// Kick off the actual move-back operation against the
    /// previously-loaded manifest. Runs on a worker thread; reports
    /// progress via the event channel.
    fn run_archive_restore_threaded(&self, manifest: crate::gui::archive::ArchiveManifest) {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("superdeduper-archive-restore".into())
            .spawn(move || {
                let total = manifest.entries.len() as u64;
                let _ = tx.send(EngineEvent::Status(format!(
                    "Restoring {total} archived file(s) from manifest…"
                )));
                let mut summary = crate::gui::archive::RestoreSummary::default();
                for entry in &manifest.entries {
                    match crate::gui::archive::restore_one(entry) {
                        crate::gui::archive::RestoreOutcome::Restored => {
                            summary.restored += 1;
                        }
                        crate::gui::archive::RestoreOutcome::ArchivedMissing => {
                            summary.archived_missing += 1;
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Warn,
                                message: format!(
                                    "restore: archived file missing · {} (expected at {})",
                                    entry.original_path.display(),
                                    entry.archived_path.display()
                                ),
                            });
                        }
                        crate::gui::archive::RestoreOutcome::OriginalExists => {
                            summary.original_exists += 1;
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Warn,
                                message: format!(
                                    "restore: target already exists, skipped · {}",
                                    entry.original_path.display()
                                ),
                            });
                        }
                        crate::gui::archive::RestoreOutcome::IoError(e) => {
                            summary.io_errors += 1;
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Error,
                                message: format!("restore: I/O error · {e}"),
                            });
                        }
                    }
                }
                let _ = tx.send(EngineEvent::Status(format!(
                    "Restore complete · {} of {} restored ({} missing, {} conflicts, {} I/O errors).",
                    summary.restored,
                    total,
                    summary.archived_missing,
                    summary.original_exists,
                    summary.io_errors
                )));
                let _ = tx.send(EngineEvent::Log {
                    level: crate::gui::events::LogLevel::Info,
                    message: format!(
                        "archive restore · restored={} missing={} conflicts={} errors={}",
                        summary.restored,
                        summary.archived_missing,
                        summary.original_exists,
                        summary.io_errors
                    ),
                });
            })
            .expect("spawn archive-restore thread");
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
                        EngineEvent::DriveDiscovered(info) => {
                            // Restore any saved override for this
                            // volume into the live HashMap so the
                            // user's previous "this is actually an
                            // SSD" decision survives across runs.
                            //
                            // Also log the resolved render decision —
                            // detected vs override vs effective — so
                            // a "my NVMe shows as HDD" mystery (the
                            // user has reported this exact one) is
                            // one-grep away in the run log instead
                            // of needing source inspection.
                            let mut override_value: Option<bool> = None;
                            if !info.volume_guid.is_empty() {
                                if let Ok(saved) = crate::gui::drive_overrides::load() {
                                    if let Some(&v) = saved.overrides.get(&info.volume_guid) {
                                        self.drive_render_overrides.insert(info.id, v);
                                        override_value = Some(v);
                                    }
                                }
                            }
                            let detected_hdd = info.has_seek_penalty;
                            let effective_hdd = match override_value {
                                Some(true) => false,
                                Some(false) => true,
                                None => detected_hdd,
                            };
                            tracing::info!(
                                drive_id = info.id,
                                volume_label = %info.volume_label,
                                volume_guid = %info.volume_guid,
                                model = %info.model,
                                detected = if detected_hdd { "HDD" } else { "SSD" },
                                manual_override = ?override_value
                                    .map(|v| if v { "force-SSD" } else { "force-HDD" }),
                                effective_render = if effective_hdd { "HDD" } else { "SSD" },
                                "drive discovered"
                            );
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
            // so safe-rename / Unsuperdeduper pick up where we left off
            // after a restart.
            self.persist_results_after_scan();
        }
        // Freeze the per-drive sparkline + LCN trace once the scan
        // finishes. Without this guard the throughput window keeps
        // ticking forward after we're done — old samples expire off
        // the left edge and the line "decays" while showing no real
        // activity, which makes it look like the engine is still
        // doing something. We want the graph to lock at the last
        // observed state when is_scanning flips false.
        if self.is_scanning {
            let now = Instant::now();
            for drive in self.state.drives.values_mut() {
                drive.roll_throughput(now);
            }
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
            RootsAction::Unsuperdeduper => self.run_unsuperdeduper_threaded(),
        }
    }

    /// Prompt for a destination folder and, if the user picks one,
    /// kick off `run_archive_dupes_threaded`. The folder picker call
    /// is blocking but cheap (a few hundred ms), and is on the UI
    /// thread on purpose so the dialog parents to the app window.
    fn pick_archive_dest_and_run(&mut self) {
        if self.state.duplicates.is_empty() {
            self.state.push_log(
                crate::gui::events::LogLevel::Warn,
                "Archive dupes: no duplicates in the current results — run a scan first.".into(),
            );
            return;
        }
        let dest = match rfd::FileDialog::new()
            .set_title("Pick a folder to archive duplicates into")
            .pick_folder()
        {
            Some(p) => p,
            None => return, // user cancelled the dialog
        };
        self.run_archive_dupes_threaded(dest);
    }

    /// Move every non-keeper, non-reference duplicate into `dest`,
    /// preserving its original drive-letter + folder hierarchy under
    /// `dest`. Writes a JSON manifest beside the archived files so a
    /// future restore can move them back. Runs off the UI thread.
    fn run_archive_dupes_threaded(&self, dest: std::path::PathBuf) {
        let tx = self.tx.clone();
        let reference_roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .filter(|r| r.is_reference)
            .map(|r| r.path.clone())
            .collect();
        // (group_size, content_hash, keeper, dupes-to-move). Keeper
        // is recorded only for the manifest's "what was this a copy
        // of" field; we never move or modify it.
        let groups: Vec<(u64, String, PathBuf, Vec<PathBuf>)> = self
            .state
            .duplicates
            .iter()
            .filter_map(|g| {
                if g.files.len() < 2 {
                    return None;
                }
                let keeper = g.files[0].clone();
                let dupes: Vec<PathBuf> = g.files[1..]
                    .iter()
                    .filter(|p| !reference_roots.iter().any(|r| p.starts_with(r)))
                    .cloned()
                    .collect();
                if dupes.is_empty() {
                    None
                } else {
                    Some((g.size, g.content_hash.clone(), keeper, dupes))
                }
            })
            .collect();
        let total: u64 = groups.iter().map(|(_, _, _, d)| d.len() as u64).sum();
        std::thread::Builder::new()
            .name("superdeduper-archive".into())
            .spawn(move || {
                let _ = tx.send(EngineEvent::Status(format!(
                    "Archiving {} file(s) from {} group(s) → {}",
                    total,
                    groups.len(),
                    dest.display()
                )));
                let mut moved = 0u64;
                let mut failed = 0u64;
                let mut manifest_entries: Vec<crate::gui::archive::ArchiveManifestEntry> =
                    Vec::new();
                for (size, hash, keeper, dupes) in &groups {
                    for src in dupes {
                        // Build the destination path: dest +
                        // drive-letter folder (e.g. "C") + the rest
                        // of the source path. Preserves the tree so
                        // a restore is unambiguous.
                        let archived = compose_archive_path(&dest, src);
                        if let Some(parent) = archived.parent() {
                            if let Err(e) = std::fs::create_dir_all(parent) {
                                failed += 1;
                                let _ = tx.send(EngineEvent::Log {
                                    level: crate::gui::events::LogLevel::Warn,
                                    message: format!(
                                        "archive mkdir failed · {} · {e}",
                                        parent.display()
                                    ),
                                });
                                continue;
                            }
                        }
                        // Try rename first (fast, atomic on same
                        // volume); if it fails with cross-device we
                        // fall back to copy+remove.
                        let move_result = std::fs::rename(src, &archived).or_else(|_| {
                            std::fs::copy(src, &archived)
                                .and_then(|_| std::fs::remove_file(src))
                                .map(|_| ())
                        });
                        match move_result {
                            Ok(()) => {
                                moved += 1;
                                manifest_entries.push(crate::gui::archive::ArchiveManifestEntry {
                                    original_path: src.clone(),
                                    archived_path: archived.clone(),
                                    keeper_path: keeper.clone(),
                                    content_hash: hash.clone(),
                                    size: *size,
                                });
                            }
                            Err(e) => {
                                failed += 1;
                                let _ = tx.send(EngineEvent::Log {
                                    level: crate::gui::events::LogLevel::Warn,
                                    message: format!(
                                        "archive move failed · {} · {e}",
                                        src.display()
                                    ),
                                });
                            }
                        }
                    }
                }
                // Write the manifest. Filename includes a timestamp
                // so multiple archive runs into the same folder
                // produce distinct manifests instead of overwriting.
                let manifest_path = dest.join(format!(
                    "superdeduper-archive-manifest-{}.json",
                    iso_timestamp_for_filename()
                ));
                let manifest = crate::gui::archive::ArchiveManifest {
                    schema: crate::gui::archive::ARCHIVE_SCHEMA.into(),
                    created_at_unix: now_unix(),
                    destination: dest.clone(),
                    entries: manifest_entries,
                };
                if let Err(e) = std::fs::write(
                    &manifest_path,
                    serde_json::to_vec_pretty(&manifest).expect("serialise archive manifest"),
                ) {
                    let _ = tx.send(EngineEvent::Log {
                        level: crate::gui::events::LogLevel::Warn,
                        message: format!(
                            "archive manifest write failed · {} · {e}",
                            manifest_path.display()
                        ),
                    });
                } else {
                    let _ = tx.send(EngineEvent::Log {
                        level: crate::gui::events::LogLevel::Info,
                        message: format!("archive manifest written · {}", manifest_path.display()),
                    });
                }
                let _ = tx.send(EngineEvent::Status(format!(
                    "Archive complete · {moved} moved, {failed} failed."
                )));
                let _ = tx.send(EngineEvent::Log {
                    level: crate::gui::events::LogLevel::Info,
                    message: format!(
                        "archive · moved={moved} failed={failed} dest={}",
                        dest.display()
                    ),
                });
            })
            .expect("spawn archive thread");
    }

    /// Gate destructive group actions on the "type DELETE" modal
    /// (unless the user has explicitly opted to bypass via Settings →
    /// Safety). Reveal-in-Explorer is non-destructive and fires
    /// immediately; everything else stashes into `pending_destructive`
    /// for the modal in `update()` to handle.
    fn dispatch_group_action(&mut self, action: GroupAction) {
        // Reveal touches nothing — bypass the modal unconditionally.
        if matches!(action, GroupAction::Reveal(_)) {
            return self.dispatch_group_action_unchecked(action);
        }
        if self.persisted.settings.bypass_destructive_confirmation {
            return self.dispatch_group_action_unchecked(action);
        }
        // Stash for the modal to confirm or cancel.
        self.pending_destructive = Some(action);
        self.destructive_confirm_input.clear();
    }

    fn dispatch_group_action_unchecked(&mut self, action: GroupAction) {
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
            GroupAction::ArchiveAllVisible => {
                // Mirror the old roots-panel Archive button flow:
                // prompt for a destination folder, then dispatch the
                // archive worker. `pick_archive_dest_and_run` handles
                // the picker + the threaded run.
                self.pick_archive_dest_and_run();
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
            .name("superdeduper-safe-rename-all".into())
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
    /// `%LOCALAPPDATA%\superdeduper\results-state.json` on a background
    /// thread. Used right after a scan finishes so the next launch
    /// can restore the duplicate list without re-scanning, provided
    /// the folders haven't drifted.
    fn persist_results_after_scan(&self) {
        let duplicates = self.state.duplicates.clone();
        let roots = self.persisted.roots.clone();
        let settings = self.persisted.settings.clone();
        std::thread::Builder::new()
            .name("superdeduper-results-save".into())
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
    /// `*.superdeduper` file back to its original. No prior scan needed.
    fn run_unsuperdeduper_threaded(&self) {
        let tx = self.tx.clone();
        let roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .map(|r| r.path.clone())
            .collect();
        std::thread::Builder::new()
            .name("superdeduper-unsuperdeduper".into())
            .spawn(move || {
                let _ = tx.send(EngineEvent::Status(format!(
                    "Unsuperduping {} root(s)…",
                    roots.len()
                )));
                let mut total_renamed = 0u64;
                let mut total_skipped = 0u64;
                let mut total_errors = 0u64;
                for r in &roots {
                    match crate::dedupe::unsuperdeduper_root(r) {
                        Ok((renamed, skipped, errors)) => {
                            total_renamed += renamed;
                            total_skipped += skipped;
                            total_errors += errors;
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Info,
                                message: format!(
                                    "unsuperdeduper · {} · renamed={renamed} skipped={skipped} errors={errors}",
                                    r.display()
                                ),
                            });
                        }
                        Err(e) => {
                            let _ = tx.send(EngineEvent::Log {
                                level: crate::gui::events::LogLevel::Error,
                                message: format!(
                                    "unsuperdeduper failed · {} · {e}",
                                    r.display()
                                ),
                            });
                            total_errors += 1;
                        }
                    }
                }
                // Renamed_paths in the saved state no longer reflects
                // reality — every `.superdeduper` file just got restored.
                // Clear the renamed list (but keep the duplicates so
                // the user can act on them again if they want).
                if let Ok(Some(mut state)) = crate::gui::results_store::load() {
                    state.renamed_paths.clear();
                    let _ = crate::gui::results_store::save(&state);
                }
                let _ = tx.send(EngineEvent::Status(format!(
                    "Unsuperdeduper complete · {} renamed, {} skipped, {} errors.",
                    total_renamed, total_skipped, total_errors,
                )));
            })
            .expect("spawn unsuperdeduper thread");
    }

    fn run_action_threaded(&self, action: DedupeAction, keeper: PathBuf, dupes: Vec<PathBuf>) {
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("superdeduper-action".into())
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

impl eframe::App for SuperdeduperApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();
        if self.is_scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        // Launch-time Resume / Start Fresh modal. While
        // `pending_resume` is Some we paint a dimmed background and
        // ONLY the modal. The rest of the UI is skipped entirely so
        // there's no way to interact with stale state before the user
        // has chosen — that's the contract Start Fresh relies on.
        if let Some(summary) = self.pending_resume.clone() {
            CentralPanel::default()
                .frame(Frame::default().fill(theme::BG).inner_margin(0.0))
                .show(ctx, |_ui| { /* empty backdrop */ });
            if let Some(choice) = resume_modal::show(ctx, &summary) {
                self.pending_resume = None;
                match choice {
                    ResumeChoice::Resume => self.accept_resume(),
                    ResumeChoice::StartFresh => self.accept_start_fresh(),
                }
            }
            return;
        }

        // Archive-restore confirmation: rendered as a non-blocking
        // window over the regular UI (vs the Resume modal which
        // gates everything). The user needs to be able to see what
        // they're about to undo while reading the prompt.
        if let Some(manifest) = self.pending_archive_restore.clone() {
            let mut accept = false;
            let mut cancel = false;
            egui::Window::new("Restore archived duplicates?")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .default_width(560.0)
                .show(ctx, |ui| {
                    let n = manifest.entries.len();
                    let total_bytes: u64 = manifest.entries.iter().map(|e| e.size).sum();
                    ui.label(
                        egui::RichText::new(format!(
                            "Move {n} file(s) ({}) from",
                            humansize::format_size(total_bytes, humansize::BINARY)
                        ))
                        .color(theme::TEXT_HI),
                    );
                    ui.label(
                        egui::RichText::new(format!("  {}", manifest.destination.display()))
                            .color(theme::TEXT_LO)
                            .monospace(),
                    );
                    ui.label(egui::RichText::new("back to their original paths.").color(theme::TEXT_HI));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(
                            "If a file already exists at the original path we'll skip it (no overwrite). Cross-volume restores fall back to copy + remove automatically.",
                        )
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                    );
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("↩  Restore")
                                        .color(theme::PANEL_DEEP)
                                        .strong(),
                                )
                                .fill(theme::ACCENT)
                                .min_size(egui::vec2(120.0, 30.0)),
                            )
                            .clicked()
                        {
                            accept = true;
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    egui::RichText::new("Cancel").color(theme::TEXT_HI),
                                )
                                .min_size(egui::vec2(100.0, 30.0)),
                            )
                            .clicked()
                        {
                            cancel = true;
                        }
                    });
                });
            if accept {
                self.pending_archive_restore = None;
                self.run_archive_restore_threaded(manifest);
            } else if cancel {
                self.pending_archive_restore = None;
                self.state.push_log(
                    crate::gui::events::LogLevel::Info,
                    "Archive restore cancelled by user.".into(),
                );
            }
        }

        // Destructive-action confirmation modal — "type DELETE".
        // Gates Recycle / SafeRename / Hardlink / SafeRenameAll;
        // Reveal-in-Explorer and Unsuperdeduper bypass this. The
        // bypass-for-all setting in Settings → Safety skips the
        // modal entirely on dispatch (so we never reach this
        // branch when bypass is on).
        if let Some(action) = self.pending_destructive.clone() {
            let mut confirm = false;
            let mut cancel = false;
            let description = describe_destructive_action(&action);
            egui::Window::new(
                egui::RichText::new("⚠ Confirm destructive action")
                    .color(theme::HOT)
                    .strong(),
            )
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label(
                    egui::RichText::new(description)
                        .color(theme::TEXT_HI)
                        .size(14.0),
                );
                ui.add_space(8.0);
                ui.label(
                    egui::RichText::new(
                        "This cannot be auto-undone from the app. \
                         Recycle is reversible from the Recycle Bin; \
                         Safe-rename is reversible via the Unsuperdeduper button; \
                         Hardlink is NOT reversible without the original data still being elsewhere.",
                    )
                    .color(theme::TEXT_LO)
                    .small(),
                );
                ui.add_space(10.0);
                ui.label(
                    egui::RichText::new("Type DELETE to confirm:")
                        .color(theme::TEXT_HI)
                        .strong(),
                );
                ui.text_edit_singleline(&mut self.destructive_confirm_input);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let can_confirm = self.destructive_confirm_input == "DELETE";
                    if ui
                        .add_enabled(
                            can_confirm,
                            egui::Button::new(
                                egui::RichText::new("Confirm")
                                    .color(if can_confirm { theme::PANEL_DEEP } else { theme::TEXT_LO })
                                    .strong(),
                            )
                            .fill(if can_confirm { theme::HOT } else { theme::PANEL }),
                        )
                        .clicked()
                    {
                        confirm = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    ui.add_space(20.0);
                    ui.label(
                        egui::RichText::new(
                            "(Tip: Settings → Safety can bypass this prompt for future actions.)",
                        )
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                    );
                });
            });
            if confirm {
                self.pending_destructive = None;
                self.destructive_confirm_input.clear();
                self.dispatch_group_action_unchecked(action);
            } else if cancel {
                self.pending_destructive = None;
                self.destructive_confirm_input.clear();
            }
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

        // File menubar — owns project lifecycle (New / Open / Save /
        // Save As / Open Archive Manifest). Rendered as a thin strip
        // above the header so it doesn't intrude on the always-visible
        // status bar.
        let mut want_settings = false;
        let mut menu_action: Option<MenuAction> = None;
        TopBottomPanel::top("menubar")
            .frame(Frame::default().fill(theme::PANEL_DEEP).inner_margin(egui::vec2(8.0, 2.0)))
            .show(ctx, |ui| {
                egui::menu::bar(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui
                            .button("New scan")
                            .on_hover_text("Clear the current project so you can start fresh. Doesn't touch your hash cache.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::New);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui
                            .button("Open Project…")
                            .on_hover_text("Pick a .superdeduper folder previously written with Save Project. Restores roots, settings, and the confirmed-duplicates list.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::OpenProject);
                            ui.close_menu();
                        }
                        let save_label = match &self.current_project_path {
                            Some(p) => format!(
                                "Save Project   ({})",
                                p.file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("project")
                            ),
                            None => "Save Project…".to_string(),
                        };
                        if ui
                            .button(save_label)
                            .on_hover_text("Write the current roots, settings, and results to the open .superdeduper folder. If no project is open, prompts for a folder.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::Save);
                            ui.close_menu();
                        }
                        if ui
                            .button("Save Project As…")
                            .on_hover_text("Write a copy of the current project to a new .superdeduper folder.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::SaveAs);
                            ui.close_menu();
                        }
                        ui.separator();
                        if ui
                            .button("Open Archive Manifest…")
                            .on_hover_text("Future: load a manifest produced by a previous Archive Dupes run, then restore the moved files to their original locations. (Restore loader: not implemented yet — manifest opens read-only.)")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::OpenArchiveManifest);
                            ui.close_menu();
                        }
                        ui.separator();
                        // Recent projects submenu — most-recently-
                        // opened first, capped at the index limit.
                        ui.menu_button("Recent projects", |ui| {
                            match crate::gui::project::load_recents() {
                                Ok(recents) if !recents.entries.is_empty() => {
                                    for r in &recents.entries {
                                        let label = format!(
                                            "{}    ({})",
                                            r.name,
                                            r.path.display()
                                        );
                                        if ui.button(label).clicked() {
                                            menu_action =
                                                Some(MenuAction::OpenRecent(r.path.clone()));
                                            ui.close_menu();
                                        }
                                    }
                                }
                                _ => {
                                    ui.label(
                                        egui::RichText::new("(none yet)")
                                            .color(theme::TEXT_LO)
                                            .italics(),
                                    );
                                }
                            }
                        });
                        ui.separator();
                        if ui.button("Quit").clicked() {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                            ui.close_menu();
                        }
                    });
                });
            });

        TopBottomPanel::top("header")
            .frame(Frame::default().fill(theme::BG).inner_margin(8.0))
            .show(ctx, |ui| {
                let action = header::show(
                    ui,
                    &self.state,
                    self.persisted.settings.hash_algo,
                    self.is_scanning,
                );
                if action == header::HeaderAction::OpenSettings {
                    want_settings = true;
                }
            });
        if want_settings {
            self.settings_open = true;
        }
        if let Some(act) = menu_action {
            self.dispatch_menu_action(act);
        }

        // Overall progress strip sits directly under the header and
        // spans the full window so the user always sees "what stage,
        // how much, how long" without hunting.
        TopBottomPanel::top("overall-progress")
            .frame(
                Frame::default()
                    .fill(theme::BG)
                    .inner_margin(egui::vec2(8.0, 4.0)),
            )
            .show(ctx, |ui| {
                overall_bar::show(ui, &self.state);
            });

        SidePanel::left("sidebar")
            .resizable(true)
            .default_width(300.0)
            .min_width(240.0)
            .frame(Frame::default().fill(theme::PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                let roots_action =
                    roots_panel::show(ui, &self.persisted.roots, self.is_scanning, self.can_resume);
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
                                let frozen = (!self.is_scanning)
                                    .then_some(self.state.scan_finished_at)
                                    .flatten();
                                drive_clicked = drive_scope::show(
                                    ui,
                                    &self.state,
                                    self.selected_drive,
                                    &mut self.drive_render_overrides,
                                    frozen,
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
                            .map(|d| format!("  ✕ filtered to: {}", d.info.volume_label))
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
                    self.persisted
                        .roots
                        .get(id as usize)
                        .map(|r| r.path.clone())
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
        eframe::set_value(storage, "superdeduper.app.v1", &self.persisted);
    }
}

fn check_resumable(persisted: &PersistedAppState) -> bool {
    use crate::gui::checkpoint;
    let Ok(path) = checkpoint::default_checkpoint_path() else {
        return false;
    };
    match checkpoint::load(&path) {
        Ok(Some(cp)) => cp.roots == persisted.roots && cp.settings == persisted.settings,
        _ => false,
    }
}

/// One-line summary of a pending destructive action — shown at the
/// top of the "type DELETE" confirmation modal. Keep it factual:
/// what, how many, target paths. The detail/reversibility paragraph
/// is rendered separately by the modal itself.
fn describe_destructive_action(action: &GroupAction) -> String {
    match action {
        GroupAction::RecycleOthers { keeper, dupes } => format!(
            "Move {} file(s) to the Recycle Bin, keeping:\n  {}",
            dupes.len(),
            keeper.display()
        ),
        GroupAction::HardlinkOthers { keeper, dupes } => format!(
            "Replace {} file(s) with hardlinks to:\n  {}\n(Both copies share the same on-disk bytes after this — \
             editing one will silently affect the other on filesystems where hardlinks share content.)",
            dupes.len(),
            keeper.display()
        ),
        GroupAction::SafeRenameOthers { keeper, dupes } => format!(
            "Append .superdeduper to {} non-keeper file(s) in this group, keeping:\n  {}\n\
             (Safe-mode: nothing is deleted; the Unsuperdeduper button on the Roots panel reverts the rename.)",
            dupes.len(),
            keeper.display()
        ),
        GroupAction::SafeRenameAllVisible => {
            "Append .superdeduper to EVERY non-keeper across EVERY currently visible duplicate group. \
             Safe-mode: nothing is deleted. Reversible via Unsuperdeduper. Reference paths are never touched."
                .to_string()
        }
        GroupAction::ArchiveAllVisible => {
            "Move EVERY non-keeper across EVERY currently visible duplicate group into the chosen \
             archive folder. Original directory tree is preserved under the destination; a manifest \
             JSON is written so the move can be restored later. Reference paths are never touched."
                .to_string()
        }
        // Reveal should never reach this code path — it bypasses the
        // modal in `dispatch_group_action` — but document it
        // anyway in case future refactors widen the dispatcher.
        GroupAction::Reveal(_) => {
            "(internal: Reveal-in-Explorer reached the destructive modal — this is a bug)".into()
        }
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

/// Map a source path like `C:\Users\X\foo.bin` to its archived
/// position under `dest`, preserving the drive letter as a folder
/// name. Drive `C:` becomes `dest/C/Users/X/foo.bin`. On non-Windows
/// hosts we just join the path components after the root.
fn compose_archive_path(dest: &Path, src: &Path) -> PathBuf {
    use std::path::{Component, Prefix};
    let mut out = dest.to_path_buf();
    for c in src.components() {
        match c {
            Component::Prefix(p) => {
                if let Prefix::Disk(letter) = p.kind() {
                    out.push(format!("{}", letter as char));
                } else {
                    // Verbatim or UNC — flatten under a literal name
                    // so we don't try to push `\\?\` segments.
                    out.push("verbatim");
                }
            }
            Component::RootDir => { /* skip */ }
            Component::Normal(s) => out.push(s),
            Component::CurDir | Component::ParentDir => { /* ignore — abs paths only */ }
        }
    }
    out
}

/// Filename-safe ISO-8601 in UTC: 2026-05-20T14-22-43Z.
fn iso_timestamp_for_filename() -> String {
    let secs = now_unix();
    let days = (secs / 86_400) as i64;
    let h = ((secs % 86_400) / 3600) as u32;
    let m = ((secs % 3600) / 60) as u32;
    let s = (secs % 60) as u32;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = (y + if mo <= 2 { 1 } else { 0 }) as i32;
    format!("{year:04}-{mo:02}-{day:02}T{h:02}-{m:02}-{s:02}Z")
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
