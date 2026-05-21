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

pub struct SuperdupeApp {
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
}

impl SuperdupeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let persisted: PersistedAppState = cc
            .storage
            .and_then(|s| eframe::get_value::<PersistedAppState>(s, "superdupe.app.v1"))
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

        let mut app = Self {
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
        };

        // The previous auto-restore behaviour (load results_state.json
        // + replay duplicates) only kicks in when there's no
        // checkpoint asking for an explicit decision. Otherwise the
        // modal handles the choice and the restore happens inside
        // `accept_resume`.
        if app.pending_resume.is_none() {
            app.auto_restore_results_state();
        }
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
                    "Restored {} duplicate group(s) from a prior scan — folders haven't changed. Safe-rename / Unsuperdupe pick up where you left off.",
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
            RootsAction::Unsuperdupe => self.run_unsuperdupe_threaded(),
            RootsAction::ArchiveDupes => self.pick_archive_dest_and_run(),
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
            .name("superdupe-archive".into())
            .spawn(move || {
                let _ = tx.send(EngineEvent::Status(format!(
                    "Archiving {} file(s) from {} group(s) → {}",
                    total,
                    groups.len(),
                    dest.display()
                )));
                let mut moved = 0u64;
                let mut failed = 0u64;
                let mut manifest_entries: Vec<ArchiveManifestEntry> = Vec::new();
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
                                manifest_entries.push(ArchiveManifestEntry {
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
                                    message: format!("archive move failed · {} · {e}", src.display()),
                                });
                            }
                        }
                    }
                }
                // Write the manifest. Filename includes a timestamp
                // so multiple archive runs into the same folder
                // produce distinct manifests instead of overwriting.
                let manifest_path = dest.join(format!(
                    "superdupe-archive-manifest-{}.json",
                    iso_timestamp_for_filename()
                ));
                let manifest = ArchiveManifest {
                    schema: "superdupe.archive.v1".into(),
                    created_at_unix: now_unix(),
                    destination: dest.clone(),
                    entries: manifest_entries,
                };
                if let Err(e) = std::fs::write(
                    &manifest_path,
                    serde_json::to_vec_pretty(&manifest)
                        .expect("serialise archive manifest"),
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
                        message: format!(
                            "archive manifest written · {}",
                            manifest_path.display()
                        ),
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

    fn run_action_threaded(&self, action: DedupeAction, keeper: PathBuf, dupes: Vec<PathBuf>) {
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
        TopBottomPanel::top("header")
            .frame(Frame::default().fill(theme::BG).inner_margin(8.0))
            .show(ctx, |ui| {
                let action = header::show(ui, &self.state, self.is_scanning);
                if action == header::HeaderAction::OpenSettings {
                    want_settings = true;
                }
            });
        if want_settings {
            self.settings_open = true;
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
        eframe::set_value(storage, "superdupe.app.v1", &self.persisted);
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

#[cfg(windows)]
fn reveal_in_explorer(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

#[cfg(not(windows))]
fn reveal_in_explorer(_path: &std::path::Path) {}

/// On-disk shape of the archive manifest. Written next to the moved
/// files when "Archive dupes" runs, consumed by a future
/// "restore-from-manifest" loader (out of scope for this commit).
/// `schema` lets the loader pin a parser version so we can evolve
/// fields without silently misreading old files.
#[derive(serde::Serialize, serde::Deserialize)]
struct ArchiveManifest {
    schema: String,
    created_at_unix: u64,
    destination: PathBuf,
    entries: Vec<ArchiveManifestEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ArchiveManifestEntry {
    /// Where the file lived before the archive run.
    original_path: PathBuf,
    /// Where the file is now (under `destination`).
    archived_path: PathBuf,
    /// The "keeper" sibling that was left in place. Recorded so the
    /// loader can verify the keeper still exists before offering to
    /// restore (you probably don't want to put a file back next to
    /// a keeper that's since been deleted).
    keeper_path: PathBuf,
    /// Content hash from the scan that produced this duplicate set.
    /// Survives algo switches because the cache keys on `hash_algo`.
    content_hash: String,
    /// Bytes; redundant with the file on disk but handy for "are you
    /// sure you want to restore X GiB?" prompts.
    size: u64,
}

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
