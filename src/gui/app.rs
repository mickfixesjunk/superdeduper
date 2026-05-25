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
use crate::gui::widgets::settings_drift_modal::SettingsDriftChoice;
use crate::gui::widgets::{
    drive_scope, funnel, groups_table, header, log_panel, overall_bar, resume_modal, roots_panel,
    settings_drift_modal, settings_modal, treemap,
};
use crate::gui::{live, theme};

#[derive(Copy, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
enum ResultsTab {
    #[default]
    Treemap,
    Groups,
    Log,
    /// #38 v1 — read-only listing of past scans persisted by
    /// `crate::scan_history`. Future v2 will add resubmit + delete
    /// affordances per-row; the tab slot is the same.
    History,
    /// #27 v1 — in-app file preview pane. Set via the 👁 button on a
    /// Groups-table row; falls through to a "no file selected" state
    /// when `app.previewed_file` is None.
    Preview,
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
    /// Sticky tab selection for the Settings modal. Persists across
    /// opens within a session; resets when the app restarts.
    settings_modal_state: crate::gui::widgets::settings_modal::SettingsModalState,
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
    /// #51 — Mid-session "Settings changed since the paused scan"
    /// modal. Populated by `start_live` when a checkpoint exists on
    /// disk but its roots/settings differ from what the user is
    /// about to launch. The user picks ContinueWithNew (archive +
    /// fresh scan) / RevertToPaused (adopt checkpoint roots+settings,
    /// then launch — the real "resume") / Cancel.
    pending_drift_modal: Option<crate::gui::checkpoint::CheckpointSummary>,
    /// #27 v1 — file currently shown in the Preview tab. Set by the
    /// 👁 button on a groups-table row + cleared by the panel's
    /// Close button. `None` ⇒ Preview tab renders an empty state.
    previewed_file: Option<PathBuf>,
    /// Sticky preview-mode override (Text vs Hex) across renders.
    /// Reset to None whenever `previewed_file` changes — handled
    /// at the set site.
    preview_state: crate::gui::preview::PreviewState,
    /// #41 — App-start "Resubmit N pending scans?" modal. Populated
    /// in `new()` with rows whose `submission_state == Pending` AND
    /// whose most-recent activity is older than the 5-minute
    /// threshold (so a row that just finished THIS session doesn't
    /// trigger a nag). `Some(_)` shows the modal; user picks
    /// [Resubmit all] / [Open History] / [Not now]. Telemetry-off
    /// builds don't populate or render this — the field is
    /// cfg-gated to avoid a "never read" warning under that combo.
    #[cfg(feature = "telemetry")]
    pending_resubmit_prompt: Option<Vec<crate::scan_history::ScanRecord>>,
    /// Scan-mode dropdown selection per spec §3.8: Exact (default)
    /// / Image (Tier-4 perceptual) / Audio (T1.3 placeholder).
    /// Lives on `SuperdeduperApp` rather than `PersistedAppState`
    /// because the spec explicitly says "sticky per session; not
    /// persistent across runs."
    scan_mode: crate::cli::ScanMode,
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
    /// `false` on launch; flips to `true` once the user has clicked
    /// either dismiss button on the alpha-warning modal during this
    /// session. Distinct from `persisted.settings.dismissed_alpha_warning`,
    /// which suppresses the modal across launches.
    alpha_warning_acked_session: bool,
    /// Shared cancel flag for in-flight destructive actions
    /// (recycle / hardlink / safe-rename / archive / unsuperdeduper).
    /// Clicking Stop on the progress modal flips it to `true`;
    /// worker threads check it between items and bail. Separate from
    /// `cancel` (the scan-wide cancel) so cancelling an action
    /// doesn't tear down an unrelated scan.
    action_cancel: Arc<AtomicBool>,
    /// Pre-flight modal state. Set to `Probing` when the user clicks
    /// Scan; transitions to `Showing` once `diagnose::run_probes`
    /// returns. `Cancel` resets to `Idle`; `Start` resets to `Idle`
    /// and proceeds to the original `start_live` body.
    preflight: crate::gui::preflight::PreflightState,
    /// Cache-fast-forward sparkle particle state. Fires during the
    /// "magical resume catch-up" phase where the engine replays
    /// cached hashes thousands-per-second; pairs with a synthesized
    /// dystopian synth swell on entry + metallic-hit chime on
    /// catch-up. STRICTLY resume-only — gated on `resume_effect_active`.
    sparkles: crate::gui::particles::Sparkles,
    /// `true` when the most recent scan launch was a Resume (set in
    /// accept_resume → start_live), and the cache has not yet
    /// caught up. Flips to `false` either when the cache catch-up
    /// fires once (Sparkles signals `left_fast_forward`) or when
    /// the scan ends. While `false`, sparkles + sounds are silent
    /// even if the rate spikes.
    resume_effect_active: bool,
    /// Filled progress-bar rect captured from the most recent render
    /// pass. Particles anchor inside it and are clipped to it.
    last_bar_fill: Option<egui::Rect>,
    /// Post-scan leaderboard modal state. Transitions:
    /// `Hidden → Ready` on ScanFinished (with AlwaysAsk + a built
    /// payload); `Ready → Submitting` on Submit click; `Submitting
    /// → Done` when the worker returns; `Done → Hidden` on Close.
    /// `AutoOptIn` bypasses Ready and goes straight to Submitting +
    /// silent toast.
    #[cfg(feature = "telemetry")]
    scan_complete_modal: crate::gui::widgets::scan_complete_modal::ScanCompleteState,
    /// Snapshot of the just-finished scan's headline stats, captured
    /// once on ScanFinished so the modal renders consistent values
    /// even if downstream UI state mutates. Cleared on Close.
    #[cfg(feature = "telemetry")]
    scan_complete_data: Option<crate::gui::widgets::scan_complete_modal::ScanCompleteData>,
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
        // egui_extras' image loaders enable `egui::include_image!`
        // — used by the OAuth provider chooser + the linked-state
        // affordances to embed the 48x48 PNG provider logos. One-
        // time install at app boot.
        egui_extras::install_image_loaders(&cc.egui_ctx);
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
            settings_modal_state: Default::default(),
            persisted,
            groups_state: groups_table::GroupsTableState::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            can_resume: false,
            selected_drive: None,
            drive_render_overrides: hashbrown::HashMap::new(),
            pending_resume,
            pending_drift_modal: None,
            previewed_file: None,
            preview_state: crate::gui::preview::PreviewState::default(),
            #[cfg(feature = "telemetry")]
            pending_resubmit_prompt: None,
            scan_mode: crate::cli::ScanMode::Exact,
            current_project_path: None,
            current_project_created_at: 0,
            pending_archive_restore: None,
            pending_destructive: None,
            destructive_confirm_input: String::new(),
            alpha_warning_acked_session: false,
            action_cancel: Arc::new(AtomicBool::new(false)),
            preflight: crate::gui::preflight::PreflightState::Idle,
            sparkles: Default::default(),
            resume_effect_active: false,
            last_bar_fill: None,
            #[cfg(feature = "telemetry")]
            scan_complete_modal:
                crate::gui::widgets::scan_complete_modal::ScanCompleteState::default(),
            #[cfg(feature = "telemetry")]
            scan_complete_data: None,
        };
        let mut app = app;
        // Populate cache-banner state on first launch — roots may
        // have been seeded from persistence or a CLI argument.
        app.refresh_cache_banner();

        // Spawn the badge-wall data fetch in the background. Catalog
        // is public + cached on a CDN; profile fetch only fires if the
        // install is registered (otherwise the wall renders the
        // greyed-out catalog with the empty-grant overlay). Best-effort:
        // failure is surfaced inline in the widget, doesn't block UI.
        #[cfg(feature = "telemetry")]
        {
            use crate::leaderboard::{catalog, install};
            let (server_url, install_id) = match install::load() {
                Ok(Some(s)) => (s.server_url, Some(s.install_id)),
                _ => ("https://api.superdeduper.io".to_string(), None),
            };
            catalog::spawn_initial_fetch(server_url, install_id);
        }

        // Intentionally NO auto-load of prior scan results on launch.
        // Projects are now explicit — File → Open Project loads one;
        // File → New / a fresh launch starts empty. The Resume modal
        // still triggers for *interrupted* scans (paused checkpoints
        // are separate from saved projects) and the user can pick
        // Start Fresh from that modal to clear it.

        // #41 — App-start scan-history maintenance.
        //
        // 1. Retention pruning. `history_retention_days == 0` means
        //    "forever" (default + v1 behavior); any other value
        //    auto-deletes rows older than the threshold so a
        //    privacy-conscious user can configure "purge after 30
        //    days" once + forget about it.
        let retention_days = app.persisted.settings.history_retention_days;
        if retention_days > 0 {
            let retention_secs = (retention_days as u64).saturating_mul(86_400);
            match crate::scan_history::prune_older_than(retention_secs) {
                Ok(0) => {}
                Ok(n) => tracing::info!(
                    pruned = n,
                    days = retention_days,
                    "scan_history: pruned per retention setting"
                ),
                Err(e) => tracing::warn!(error = %e, "scan_history: prune failed"),
            }
        }

        // 2. Crash-detect modal. Anything still in `Pending` whose
        //    most recent activity is more than 5 minutes old is a
        //    candidate for "resubmit on next launch" — the live
        //    submission flow would have flipped it to Submitted /
        //    Failed if it had reached the server. 5min ≫ a typical
        //    submit RTT, so we won't nag about a row that just
        //    finished this session.
        #[cfg(feature = "telemetry")]
        {
            const STALE_PENDING_SECS: u64 = 300;
            match crate::scan_history::list_pending_older_than(STALE_PENDING_SECS) {
                Ok(rows) if !rows.is_empty() => {
                    app.pending_resubmit_prompt = Some(rows);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "scan_history: pending sweep failed"),
            }
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
                self.state.totals.duplicates = self.state.totals.duplicates.saturating_add(1);
                self.state.totals.reclaimable_bytes = self
                    .state
                    .totals
                    .reclaimable_bytes
                    .saturating_add(crate::gui::state::inode_aware_savings(&g));
                // Keep `duplicate_hashes` in lockstep with the
                // `duplicates` Vec — the state.apply DuplicateFound
                // dedupe path uses this index, so an out-of-band push
                // here must register the hash too. Otherwise the
                // engine's resume-replay would count this group again.
                self.state.duplicate_hashes.insert(g.content_hash.clone());
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
            self.state.totals.duplicates = self.state.totals.duplicates.saturating_add(1);
            self.state.totals.reclaimable_bytes = self
                .state
                .totals
                .reclaimable_bytes
                .saturating_add(crate::gui::state::inode_aware_savings(g));
            // Keep duplicate_hashes synced — see #39/#40 fix in
            // state.rs's DuplicateFound handler.
            self.state.duplicate_hashes.insert(g.content_hash.clone());
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
        // Flag this scan as a resume so the cache-fast-forward
        // sparkles + dystopian synth effects fire (they're gated on
        // resume_effect_active so they never appear on fresh scans).
        self.resume_effect_active = true;
        self.sparkles.reset();
        // Auto-launch the resumed scan. The user's click on Resume
        // in the modal is consent — making them click "Resume scan"
        // again in the roots panel was a pointless second click.
        // can_resume=true is set above, so start_live() will skip
        // pre-flight and call launch_scan() directly.
        self.start_live();
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
        self.refresh_cache_banner();
    }

    /// Recompute `state.cache_volume_summaries` from the current
    /// scan roots. Called whenever roots are added/removed/loaded.
    /// Best-effort — silently leaves the list empty if the cache
    /// file isn't available or any query fails (e.g. fresh install,
    /// schema mismatch, locked file). The banner is informational;
    /// a stale or empty list just hides the banner.
    pub fn refresh_cache_banner(&mut self) {
        self.state.cache_volume_summaries.clear();
        let Ok(cache_path) = crate::cache::default_cache_path() else {
            return;
        };
        if !cache_path.exists() {
            return;
        }
        let Ok(cache) = crate::cache::Cache::open(&cache_path) else {
            return;
        };
        // De-dupe per-volume so a scan with multiple roots on the
        // same drive only contributes one banner entry.
        let mut seen = std::collections::HashSet::new();
        for root in &self.persisted.roots {
            let Some(guid) = live::volume_guid_for(&root.path) else {
                continue;
            };
            if !seen.insert(guid.clone()) {
                continue;
            }
            if let Ok(Some((captured_at, count))) = cache.cache_summary_for_volume(&guid) {
                if count > 0 {
                    self.state
                        .cache_volume_summaries
                        .push(crate::gui::state::CacheVolumeSummary {
                            volume_guid: guid,
                            captured_at_unix: captured_at,
                            record_count: count,
                        });
                }
            }
        }
    }

    pub fn start_live(&mut self) {
        if self.is_scanning || self.persisted.roots.is_empty() {
            return;
        }
        if self.preflight.is_active() {
            return;
        }
        // #51 — guard against silent settings-drift restarts.
        // A checkpoint sitting on disk whose roots+settings don't
        // match what the user is about to launch with would be
        // dropped by the engine's resume filter (`live.rs::run`),
        // silently restarting from scratch. Pop a modal so the user
        // can pick: continue-with-new (archive checkpoint + fresh),
        // revert-to-paused (adopt checkpoint's roots+settings, then
        // launch), or cancel. Skipped when `can_resume` is already
        // true (the launch-time Resume modal path adopted the
        // checkpoint, so settings already match) and when no
        // checkpoint exists at all.
        if !self.can_resume && self.pending_drift_modal.is_none() {
            if let Some(drift) = self.detect_settings_drift() {
                self.pending_drift_modal = Some(drift);
                return;
            }
        }
        // Skip preflight when:
        // * user has flipped the persistent "Skip pre-flight modal" setting
        // * this is a Resume (checkpoint already adopted; user already chose
        //   to continue — the score is the same machine we're already on)
        if self.persisted.settings.skip_preflight || self.can_resume {
            self.launch_scan(None);
            return;
        }
        let roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .map(|r| r.path.clone())
            .collect();
        self.preflight = crate::gui::preflight::spawn_probe(roots);
    }

    /// Returns a `CheckpointSummary` iff a checkpoint exists on disk
    /// whose `(roots, settings)` DIFFER from the user's current
    /// `persisted` state. Used by `start_live` to pop the #51
    /// settings-drift modal before the engine silently throws the
    /// checkpoint away.
    fn detect_settings_drift(&self) -> Option<crate::gui::checkpoint::CheckpointSummary> {
        use crate::gui::checkpoint;
        let path = checkpoint::default_checkpoint_path().ok()?;
        let cp = checkpoint::load(&path).ok().flatten()?;
        if cp.roots == self.persisted.roots && cp.settings == self.persisted.settings {
            return None;
        }
        Some(checkpoint::CheckpointSummary {
            created_at_unix: cp.created_at_unix,
            roots: cp.roots,
            duplicate_count: cp.previous_duplicates.len(),
            has_saved_inventory: cp.saved_inventory.is_some(),
        })
    }

    /// #51 — user picked "Continue with new settings" on the drift
    /// modal. Archive the existing checkpoint (never delete) so the
    /// fresh scan has a clean slate, then route back through
    /// `start_live` to honour preflight / skip-preflight settings.
    fn accept_drift_continue(&mut self) {
        if let Ok(path) = crate::gui::checkpoint::default_checkpoint_path() {
            match crate::gui::checkpoint::archive(&path) {
                Ok(Some(archived)) => self.state.push_log(
                    crate::gui::events::LogLevel::Info,
                    format!(
                        "Previous checkpoint archived to {} — settings changed since pause.",
                        archived.display()
                    ),
                ),
                Ok(None) => {}
                Err(e) => self.state.push_log(
                    crate::gui::events::LogLevel::Warn,
                    format!("Couldn't archive previous checkpoint: {e}"),
                ),
            }
        }
        self.can_resume = false;
        self.start_live();
    }

    /// #51 — user picked "Revert to paused settings" on the drift
    /// modal. Mirrors `accept_resume`: hydrate state from the
    /// checkpoint and auto-launch. Engine's resume filter sees a
    /// match → real resume from prior progress.
    fn accept_drift_revert(&mut self) {
        self.accept_resume();
    }

    /// Spawn the actual scan worker. Called by `start_live` only
    /// AFTER preflight has been dismissed (Cancel branches out, Start
    /// proceeds here). The body is what `start_live` used to do.
    ///
    /// `defender_rtp_pre` is `Some(_)` only when this launch came
    /// through the preflight modal (the Defender probe ran there);
    /// resume / skip-preflight paths pass `None`.
    fn launch_scan(&mut self, defender_rtp_pre: Option<bool>) {
        self.is_scanning = true;
        self.cancel.store(false, Ordering::Relaxed);
        self.can_resume = false;
        let mut effective_settings = self.persisted.settings.clone();
        if !self.persisted.settings.always_use_cache
            && !self.state.cache_volume_summaries.is_empty()
        {
            effective_settings.use_cache = self.state.use_cache_for_next_scan;
        }
        self.state.use_cache_for_next_scan = true;
        live::spawn_with_settings(
            self.tx.clone(),
            self.persisted.roots.clone(),
            effective_settings,
            self.cancel.clone(),
            defender_rtp_pre,
            self.scan_mode,
            5, // image_similarity_threshold — TODO add a Settings input
            // when v3 ships the GUI threshold control.
            crate::cli::ImageHashAlgoArg::default(), // dhash; Settings TODO too.
            5.0, // audio_similarity_threshold — same Settings TODO.
        );
    }

    /// Drain the preflight channel + render the modal. Called once
    /// per frame from `update()`. The modal is a floating window —
    /// the rest of the UI keeps rendering behind it. The natural
    /// gate against starting a scan twice is `start_live`'s early
    /// return when `preflight.is_active()`.
    fn tick_preflight(&mut self, ctx: &egui::Context) {
        use crate::gui::preflight::{self as pf, PreflightAction, PreflightState};
        if let PreflightState::Probing { rx, .. } = &self.preflight {
            if let Ok(probe_result) = rx.try_recv() {
                self.preflight = match probe_result {
                    Ok(report) => {
                        let grade = pf::grade_report(&report);
                        PreflightState::Showing {
                            report: Box::new(report),
                            grade,
                        }
                    }
                    Err(e) => PreflightState::Failed(format!("{:#}", e)),
                };
            }
        }
        if !self.preflight.is_active() {
            return;
        }
        if let Some(action) = crate::gui::widgets::preflight_modal::show(ctx, &self.preflight) {
            // Snapshot the Defender RTP probe result *before* we drop
            // the report — G1 wants pre-scan defender state in the
            // leaderboard payload, and this is the only place the
            // probe runs in the GUI happy path.
            let defender_rtp_pre = if let PreflightState::Showing { report, .. } = &self.preflight {
                report.defender.rtp_enabled
            } else {
                None
            };
            self.preflight = PreflightState::Idle;
            match action {
                PreflightAction::Start => self.launch_scan(defender_rtp_pre),
                PreflightAction::Cancel => {}
            }
        }
    }

    /// Called from `drain_events` on `ScanFinished`. Captures the
    /// run's headline stats, then transitions the post-scan modal
    /// per the user's share preference:
    ///
    /// * `AlwaysAsk` → modal opens in `Ready` state (Submit / Skip
    ///   / Auto-submit / What gets shared? buttons).
    /// * `AutoOptIn` → modal opens in `Submitting` state immediately
    ///   (no user click needed); will fall through to `Done` when
    ///   the worker returns. User still sees rank + achievements but
    ///   doesn't have to click Submit each scan.
    /// * `Never` → no modal; no submission.
    ///
    /// Requires the engine to have called `submission::store_pending`,
    /// which it does unconditionally in `live::run` post-ScanFinished.
    #[cfg(feature = "telemetry")]
    fn on_scan_finished_for_leaderboard(
        &mut self,
        total_files: u64,
        total_bytes_read: u64,
        duplicates: u64,
        reclaimable_bytes: u64,
    ) {
        use crate::gui::widgets::scan_complete_modal::{ScanCompleteData, ScanCompleteState};
        use crate::leaderboard::install;
        use crate::leaderboard::submission;

        // Drop any prior modal's state — a new scan supersedes it.
        self.scan_complete_modal = ScanCompleteState::Hidden;
        self.scan_complete_data = None;

        // No pending payload means the engine's telemetry path didn't
        // build one this run (e.g. cancelled or errored mid-scan).
        // Don't pop the modal with a stale or zero payload.
        if submission::peek_pending().is_none() {
            return;
        }

        let elapsed_secs = self
            .state
            .scan_elapsed()
            .map(|d| d.as_secs_f32())
            .unwrap_or(0.0);
        self.scan_complete_data = Some(ScanCompleteData::from_engine_event(
            elapsed_secs,
            reclaimable_bytes,
            total_files,
            total_bytes_read,
            duplicates,
        ));

        // Decide what to do based on the install's share preference.
        let share = install::load()
            .ok()
            .flatten()
            .map(|s| s.share_default)
            .unwrap_or(install::ShareDefault::AlwaysAsk);

        match share {
            install::ShareDefault::Never => {
                self.scan_complete_modal = ScanCompleteState::Hidden;
            }
            install::ShareDefault::AlwaysAsk => {
                self.scan_complete_modal = ScanCompleteState::Ready;
            }
            install::ShareDefault::AutoOptIn => {
                self.scan_complete_modal = ScanCompleteState::Submitting;
                self.spawn_leaderboard_submit_worker();
            }
        }
    }

    /// Spawn the submit worker thread. Reads `take_pending()` (so a
    /// rapid double-click can't double-submit), POSTs the payload,
    /// stashes the outcome via `store_last_outcome`. The render
    /// loop polls `peek_last_outcome` each frame to flip the modal
    /// from `Submitting` to `Done`.
    #[cfg(feature = "telemetry")]
    fn spawn_leaderboard_submit_worker(&self) {
        std::thread::spawn(|| {
            use crate::leaderboard::{install, registration, submission};
            // Auto-register if needed: clicking Submit means the user
            // has decided to participate (same logic as the OAuth
            // chain). Same worker thread runs the PoW + POST
            // /api/v1/register inline (~1s); user sees the existing
            // "Submitting..." spinner for one extra second instead
            // of a "not registered" error. Per Mick 2026-05-25T03:15Z.
            let state = match install::load() {
                Ok(Some(s)) if s.registered => s,
                Ok(Some(mut s)) => {
                    eprintln!(
                        "submit: install present but not registered on web; \
                         auto-registering before submit"
                    );
                    match registration::register_cli(&mut s) {
                        Ok(()) => s,
                        Err(e) => {
                            submission::store_last_outcome(submission::SubmitOutcome::Rejected {
                                status: 0,
                                reason: format!("auto-register before submit failed: {e:?}"),
                            });
                            return;
                        }
                    }
                }
                Ok(None) => {
                    eprintln!("submit: no install state on disk; auto-registering before submit");
                    let server_url =
                        crate::channel::server_url_for(crate::channel::active_channel())
                            .to_string();
                    let mut s = install::new_unregistered(server_url);
                    match registration::register_cli(&mut s) {
                        Ok(()) => s,
                        Err(e) => {
                            submission::store_last_outcome(submission::SubmitOutcome::Rejected {
                                status: 0,
                                reason: format!("auto-register before submit failed: {e:?}"),
                            });
                            return;
                        }
                    }
                }
                Err(e) => {
                    submission::store_last_outcome(submission::SubmitOutcome::Rejected {
                        status: 0,
                        reason: format!("install state read error: {e}"),
                    });
                    return;
                }
            };
            // PEEK (not take) so the pending payload survives a
            // failed submit — lets the "Submit for review" fallback
            // path on the modal still find inputs to flag. Only
            // clear the slot on Accepted (or DuplicateNoChange, where
            // the server already has it).
            let inputs = match submission::peek_pending() {
                Some(i) => i,
                None => {
                    submission::store_last_outcome(submission::SubmitOutcome::Rejected {
                        status: 0,
                        reason: "no pending submission".into(),
                    });
                    return;
                }
            };
            let outcome = submission::submit(&state, &inputs);
            // Archive every attempt — success, duplicate, rejected,
            // or transient — to the local archive dir so the user
            // has a permanent record they can come back to.
            submission::archive_attempt(&inputs, &state.install_id, &outcome);
            // On Accepted, kick off the ranks poller. Web computes
            // ranks async-but-immediate (~200ms typical); the poller
            // surfaces them via toast + modal-update once the
            // backend's worker lands them.
            //
            // Also refresh the cached profile so any achievements
            // unlocked by THIS submission flip greyed → coloured in
            // the badge wall. Without this, badge tiles only update
            // on app restart.
            if let submission::SubmitOutcome::Accepted { submission_id, .. } = &outcome {
                eprintln!(
                    "submit: Accepted (submission_id={}); spawning ranks-poll + profile-refresh",
                    submission_id
                );
                crate::leaderboard::ranks_poll::spawn_ranks_poll_worker(submission_id.clone());
                crate::leaderboard::catalog::spawn_profile_refresh(
                    state.server_url.clone(),
                    state.install_id.clone(),
                );
            }
            // Clear the pending slot only when the server accepted
            // the payload (or has it on file via 409). Rejected /
            // Transient stay pending so a follow-up Submit-for-
            // review (or retry) has data to work with.
            if matches!(
                &outcome,
                submission::SubmitOutcome::Accepted { .. }
                    | submission::SubmitOutcome::DuplicateNoChange
            ) {
                let _ = submission::take_pending();
            }
            if let submission::SubmitOutcome::Transient { reason } = &outcome {
                eprintln!("leaderboard: submit transient ({reason}); enqueueing");
                let body = crate::leaderboard::hmac_signer::canonical_body(
                    &submission::build_payload(&inputs, &state.install_id),
                );
                let signature = state
                    .install_key()
                    .map(|k| crate::leaderboard::hmac_signer::sign(&k, &body))
                    .unwrap_or_default();
                if let Err(e) = submission::enqueue(&inputs, &state.install_id, &signature) {
                    eprintln!("leaderboard: enqueue failed: {e:?}");
                }
            }
            submission::store_last_outcome(outcome);
        });
    }

    /// Render the post-scan leaderboard modal (if active) + dispatch
    /// its actions. Called once per frame from `update()`.
    #[cfg(feature = "telemetry")]
    fn tick_scan_complete_modal(&mut self, ctx: &egui::Context) {
        use crate::gui::widgets::scan_complete_modal::{
            self as widget, ScanCompleteAction, ScanCompleteState,
        };
        use crate::leaderboard::submission;

        if matches!(self.scan_complete_modal, ScanCompleteState::Hidden) {
            return;
        }
        // While the post-scan modal is up, keep the GUI repainting at
        // 5 Hz so background workers (submit → profile-refresh → ranks-
        // poll) can publish results into static slots and have the
        // badge wall + modal pick them up without waiting for user
        // input. Without this, egui idle-throttles after the modal
        // transitions stop generating events and stale grants linger
        // on the badge wall until a mouse-move wakes the frame loop.
        if matches!(
            self.scan_complete_modal,
            ScanCompleteState::Submitting | ScanCompleteState::Done
        ) {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        // Submit worker stashes outcome via store_last_outcome; flip
        // the state when we observe one in flight from Submitting.
        if matches!(self.scan_complete_modal, ScanCompleteState::Submitting)
            && submission::peek_last_outcome().is_some()
        {
            self.scan_complete_modal = ScanCompleteState::Done;
        }
        let data = match &self.scan_complete_data {
            Some(d) => d.clone(),
            None => {
                self.scan_complete_modal = ScanCompleteState::Hidden;
                return;
            }
        };
        let outcome = submission::peek_last_outcome();
        // Re-build the payload preview on demand only when needed.
        // Cheap (small JSON, no IO); avoids holding state.
        let payload_preview: Option<String> =
            if matches!(self.scan_complete_modal, ScanCompleteState::Preview) {
                let install_id = crate::leaderboard::install::load()
                    .ok()
                    .flatten()
                    .map(|s| s.install_id)
                    .unwrap_or_else(|| "<not-registered>".to_string());
                submission::peek_pending().map(|inputs| {
                    let v = submission::build_payload(&inputs, &install_id);
                    serde_json::to_string_pretty(&v).unwrap_or_default()
                })
            } else {
                None
            };
        let action = widget::show(
            ctx,
            self.scan_complete_modal,
            &data,
            outcome.as_ref(),
            payload_preview.as_deref(),
        );
        if let Some(a) = action {
            match a {
                ScanCompleteAction::Submit => {
                    self.scan_complete_modal = ScanCompleteState::Submitting;
                    self.spawn_leaderboard_submit_worker();
                }
                ScanCompleteAction::AutoSubmit => {
                    self.flip_share_to_auto_opt_in();
                    self.scan_complete_modal = ScanCompleteState::Submitting;
                    self.spawn_leaderboard_submit_worker();
                }
                ScanCompleteAction::Skip => {
                    self.scan_complete_modal = ScanCompleteState::Hidden;
                    self.scan_complete_data = None;
                }
                ScanCompleteAction::OpenPreview => {
                    self.scan_complete_modal = ScanCompleteState::Preview;
                }
                ScanCompleteAction::ClosePreview => {
                    // Return to the prior state — Ready unless an
                    // outcome already landed (then Done).
                    self.scan_complete_modal = if submission::peek_last_outcome().is_some() {
                        ScanCompleteState::Done
                    } else {
                        ScanCompleteState::Ready
                    };
                }
                ScanCompleteAction::SubmitForReview => {
                    self.flag_pending_for_review();
                }
                ScanCompleteAction::Close => {
                    self.scan_complete_modal = ScanCompleteState::Hidden;
                    self.scan_complete_data = None;
                }
            }
        }
    }

    /// Flag the current pending submission + its rejection for admin
    /// review. Writes a local copy to the review-queue dir + best-
    /// effort POST to the backend review endpoint. Spawns off-thread
    /// so the UI doesn't block on the upload. Stashes a synthetic
    /// outcome in the last-outcome slot so the modal updates to
    /// confirm the action.
    #[cfg(feature = "telemetry")]
    fn flag_pending_for_review(&self) {
        std::thread::spawn(|| {
            use crate::leaderboard::{install, submission};
            let state = match install::load() {
                Ok(Some(s)) => s,
                _ => {
                    eprintln!("review: install not loaded; skipping");
                    return;
                }
            };
            let inputs = match submission::peek_pending() {
                Some(i) => i,
                None => {
                    eprintln!("review: no pending payload to flag");
                    return;
                }
            };
            let rejection =
                submission::peek_last_outcome().unwrap_or(submission::SubmitOutcome::Rejected {
                    status: 0,
                    reason: "unknown".into(),
                });
            // Capture the original rejection's status + reason so
            // the confirmation card can surface both "we flagged
            // this for review" AND the error that triggered it,
            // side by side, in the same view.
            let (original_status, original_reason) = match &rejection {
                submission::SubmitOutcome::Rejected { status, reason } => (*status, reason.clone()),
                submission::SubmitOutcome::Transient { reason } => (0, reason.clone()),
                _ => (0, "unknown".to_string()),
            };
            match submission::flag_for_review(&state, &inputs, &rejection, None) {
                Ok((path, review_id)) => {
                    eprintln!(
                        "review: saved to {} (review_id={:?})",
                        path.display(),
                        review_id
                    );
                    submission::store_last_outcome(submission::SubmitOutcome::FlaggedForReview {
                        review_id,
                        local_path: path.display().to_string(),
                        original_status,
                        original_reason,
                    });
                }
                Err(e) => {
                    eprintln!("review: local save failed: {e:?}");
                    submission::store_last_outcome(submission::SubmitOutcome::Rejected {
                        status: 0,
                        reason: format!("Review-save failed: {e}"),
                    });
                }
            }
        });
    }

    /// Route a click from the badge-wall panel. Tile clicks log to
    /// stderr for now (proper tile-detail modal is a follow-up);
    /// header link opens the live profile URL; register CTA pops
    /// the Settings modal at the Leaderboard tab.
    #[cfg(feature = "telemetry")]
    fn dispatch_badge_wall_action(
        &mut self,
        action: crate::gui::widgets::badge_wall::BadgeWallAction,
    ) {
        use crate::gui::widgets::badge_wall::BadgeWallAction;
        use crate::leaderboard::install;
        match action {
            BadgeWallAction::TileClicked(id) => {
                eprintln!("badge-wall: tile clicked: {id}");
            }
            BadgeWallAction::OpenProfile => {
                let url = match install::load() {
                    Ok(Some(s)) => format!("https://superdeduper.io/profile/{}", s.install_id),
                    _ => "https://superdeduper.io/".to_string(),
                };
                open_url_in_browser(&url);
            }
            BadgeWallAction::OpenRegister => {
                self.settings_open = true;
                self.settings_modal_state.tab =
                    crate::gui::widgets::settings_modal::SettingsTab::Leaderboard;
            }
        }
    }

    /// Persist `ShareDefault::AutoOptIn` on the install. Best-effort:
    /// a save failure logs to stderr but doesn't stop the in-flight
    /// submission. Next launch will re-load and pick up the change.
    #[cfg(feature = "telemetry")]
    fn flip_share_to_auto_opt_in(&self) {
        use crate::leaderboard::install;
        if let Ok(Some(mut s)) = install::load() {
            s.share_default = install::ShareDefault::AutoOptIn;
            if let Err(e) = install::save(&s) {
                eprintln!("leaderboard: failed to persist AutoOptIn: {e:?}");
            }
        }
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
                    self.state.totals.duplicates = self.state.totals.duplicates.saturating_add(1);
                    self.state.totals.reclaimable_bytes = self
                        .state
                        .totals
                        .reclaimable_bytes
                        .saturating_add(crate::gui::state::inode_aware_savings(&g));
                    // Keep duplicate_hashes synced.
                    self.state.duplicate_hashes.insert(g.content_hash.clone());
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
        self.action_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.action_cancel);
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("superdeduper-archive-restore".into())
            .spawn(move || {
                let total = manifest.entries.len() as u64;
                let _ = tx.send(EngineEvent::ActionStarted {
                    name: format!("↪ Archive restore · {total} file(s)"),
                    total: Some(total),
                });
                let mut summary = crate::gui::archive::RestoreSummary::default();
                let mut user_stopped = false;
                for (i, entry) in manifest.entries.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        user_stopped = true;
                        break;
                    }
                    let _ = tx.send(EngineEvent::ActionProgress {
                        done: i as u64,
                        current: Some(entry.original_path.display().to_string()),
                    });
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
                let label = if user_stopped { "stopped" } else { "complete" };
                let _ = tx.send(EngineEvent::Log {
                    level: crate::gui::events::LogLevel::Info,
                    message: format!(
                        "archive restore · restored={} missing={} conflicts={} errors={} stopped={user_stopped}",
                        summary.restored,
                        summary.archived_missing,
                        summary.original_exists,
                        summary.io_errors
                    ),
                });
                let _ = tx.send(EngineEvent::ActionFinished {
                    summary: format!(
                        "Restore {label} · {} of {} restored ({} missing, {} conflicts, {} I/O errors).",
                        summary.restored,
                        total,
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
                        EngineEvent::ScanFinished {
                            total_files,
                            total_bytes_read,
                            duplicates,
                            reclaimable_bytes,
                            ..
                        } => {
                            self.is_scanning = false;
                            self.persisted.results_tab = ResultsTab::Groups;
                            self.groups_state = groups_table::GroupsTableState::default();
                            scan_just_finished = true;
                            // If we were still in fast-forward at
                            // scan-end (every file was cache-cached,
                            // so the rate never dropped to real-
                            // hashing speed), fire the catch-up
                            // burst + metallic hit as a finale.
                            // Otherwise the resume effect would just
                            // vanish with no transition.
                            if self.resume_effect_active && self.sparkles.is_fast_forwarding() {
                                self.sparkles.force_catch_up(self.last_bar_fill);
                            }
                            self.resume_effect_active = false;
                            // Don't reset() here — leave the burst
                            // particles in flight so the user sees
                            // them; they'll age out on their own.
                            #[cfg(feature = "telemetry")]
                            self.on_scan_finished_for_leaderboard(
                                *total_files,
                                *total_bytes_read,
                                *duplicates,
                                *reclaimable_bytes,
                            );
                            // Non-telemetry build: explicitly discard so
                            // the destructure isn't flagged unused.
                            #[cfg(not(feature = "telemetry"))]
                            {
                                let _ =
                                    (total_files, total_bytes_read, duplicates, reclaimable_bytes);
                            }
                        }
                        EngineEvent::ScanPaused { .. } => {
                            self.is_scanning = false;
                            self.resume_effect_active = false;
                            self.sparkles.reset();
                        }
                        EngineEvent::ActionFinished { .. } => {
                            // Action's done — clear the per-group
                            // "acted" badge so the row's buttons come
                            // back. The user may want to re-trigger
                            // (e.g. if the first run had failures);
                            // hiding the buttons forever was a UX
                            // bug. Safe because the modal blocks
                            // overlapping actions, so at most one
                            // group has been touched while the action
                            // was running.
                            self.groups_state.acted.clear();
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
        self.action_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.action_cancel);
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
        let hide_unreclaimable = self.groups_state.hide_unreclaimable;
        let groups: Vec<(u64, String, PathBuf, Vec<PathBuf>)> = self
            .state
            .duplicates
            .iter()
            .filter_map(|g| {
                if g.files.len() < 2 {
                    return None;
                }
                // Respect the hide-unreclaimable toggle: archiving
                // 0-byte-reclaimable groups (hardlinks) is at best
                // pointless + at worst destructive (moving aliases
                // doesn't free space on the source volume).
                if hide_unreclaimable && crate::gui::state::inode_aware_savings(g) == 0 {
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
                let _ = tx.send(EngineEvent::ActionStarted {
                    name: format!(
                        "📦 Archive · {} file(s) from {} group(s) → {}",
                        total,
                        groups.len(),
                        dest.display()
                    ),
                    total: Some(total),
                });
                let mut moved = 0u64;
                let mut failed = 0u64;
                let mut user_stopped = false;
                let mut processed = 0u64;
                let mut manifest_entries: Vec<crate::gui::archive::ArchiveManifestEntry> =
                    Vec::new();
                'outer: for (size, hash, keeper, dupes) in &groups {
                    for src in dupes {
                        if cancel.load(Ordering::Relaxed) {
                            user_stopped = true;
                            break 'outer;
                        }
                        let _ = tx.send(EngineEvent::ActionProgress {
                            done: processed,
                            current: Some(src.display().to_string()),
                        });
                        processed += 1;
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
                let label = if user_stopped { "stopped" } else { "complete" };
                let _ = tx.send(EngineEvent::Log {
                    level: crate::gui::events::LogLevel::Info,
                    message: format!(
                        "archive · moved={moved} failed={failed} stopped={user_stopped} dest={}",
                        dest.display()
                    ),
                });
                let _ = tx.send(EngineEvent::ActionFinished {
                    summary: format!("Archive {label} · {moved} moved, {failed} failed."),
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
        // Reveal / Open* touch nothing — bypass the modal
        // unconditionally. They're navigational, not destructive.
        // PromoteKeeper is also non-destructive (in-memory swap) and
        // skips the modal.
        if matches!(
            action,
            GroupAction::Reveal(_)
                | GroupAction::OpenFile(_)
                | GroupAction::OpenFolder(_)
                | GroupAction::PromoteKeeper { .. }
                | GroupAction::Preview(_)
        ) {
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
            GroupAction::OpenFile(path) => open_file_default_app(&path),
            GroupAction::OpenFolder(path) => open_enclosing_folder(&path),
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
            GroupAction::RecycleAllVisible => {
                self.run_bulk_destructive_threaded(DedupeAction::Recycle, "♻ Recycle");
            }
            GroupAction::NukeAllVisible => {
                self.run_bulk_destructive_threaded(DedupeAction::Remove, "💀 Nuke");
            }
            GroupAction::PromoteKeeper {
                group_idx,
                member_idx,
            } => {
                // Swap files[0] with files[member_idx] so the smart-
                // picked keeper becomes a dupe and the chosen member
                // becomes the protected keeper. In-memory only —
                // doesn't touch disk. The user can flip back by
                // clicking 👑 on the other row.
                if let Some(g) = self.state.duplicates.get_mut(group_idx) {
                    if member_idx > 0 && member_idx < g.files.len() {
                        g.files.swap(0, member_idx);
                    }
                }
            }
            GroupAction::Preview(path) => {
                // Reset the mode-override whenever the previewed
                // file changes so a "Force Hex" choice on file A
                // doesn't leak to file B.
                let path_changed = self
                    .previewed_file
                    .as_ref()
                    .map(|p| p != &path)
                    .unwrap_or(true);
                if path_changed {
                    self.preview_state = crate::gui::preview::PreviewState::default();
                }
                self.previewed_file = Some(path);
                self.persisted.results_tab = ResultsTab::Preview;
            }
        }
    }

    /// Iterate every duplicate group currently in `self.state` and
    /// safe-rename every non-keeper that isn't a reference path. Runs
    /// on a worker thread so the UI keeps responding.
    fn run_safe_rename_all_threaded(&self) {
        // Fresh cancel-flag for this run; ANY stale Stop click from
        // a previous action stays cleared so this thread doesn't
        // bail on its first iteration.
        self.action_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.action_cancel);
        let tx = self.tx.clone();
        let reference_roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .filter(|r| r.is_reference)
            .map(|r| r.path.clone())
            .collect();
        let hide_unreclaimable = self.groups_state.hide_unreclaimable;
        let groups: Vec<(PathBuf, Vec<PathBuf>)> = self
            .state
            .duplicates
            .iter()
            .filter_map(|g| {
                if g.files.len() < 2 {
                    return None;
                }
                // Respect the hide-unreclaimable toggle (same as
                // Recycle/Nuke): if the user has 0-byte-reclaimable
                // groups hidden from the table view, the Go button
                // must not act on them either.
                if hide_unreclaimable && crate::gui::state::inode_aware_savings(g) == 0 {
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
                let _ = tx.send(EngineEvent::ActionStarted {
                    name: format!(
                        "🛡 Safe-rename · {} file(s) across {} group(s)",
                        total,
                        groups.len()
                    ),
                    total: Some(total),
                });
                let mut done = 0u64;
                let mut failed = 0u64;
                let mut skipped = 0u64;
                let mut renamed_paths: Vec<PathBuf> = Vec::new();
                let mut processed = 0u64;
                let mut user_stopped = false;
                'outer: for (_keeper, dupes) in &groups {
                    for d in dupes {
                        if cancel.load(Ordering::Relaxed) {
                            user_stopped = true;
                            break 'outer;
                        }
                        // Emit progress before each file so the
                        // modal's "current" line updates to the
                        // file we're about to touch.
                        let _ = tx.send(EngineEvent::ActionProgress {
                            done: processed,
                            current: Some(d.display().to_string()),
                        });
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
                        processed += 1;
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
                let summary = if user_stopped {
                    format!(
                        "Safe-rename stopped by user · {} renamed, {} skipped, {} failed.",
                        done, skipped, failed
                    )
                } else {
                    format!(
                        "Safe-rename complete · {} renamed, {} skipped, {} failed.",
                        done, skipped, failed
                    )
                };
                let _ = tx.send(EngineEvent::Log {
                    level: crate::gui::events::LogLevel::Info,
                    message: format!(
                        "safe-rename · renamed={done} skipped={skipped} failed={failed} stopped={user_stopped}"
                    ),
                });
                let _ = tx.send(EngineEvent::ActionFinished { summary });
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
        self.action_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.action_cancel);
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
                let _ = tx.send(EngineEvent::ActionStarted {
                    name: format!("↩ Unsuperdeduper · {} root(s)", roots.len()),
                    // Unsuperdeduper walks the tree; we don't know
                    // the count of `.superdeduper` markers upfront.
                    // Spinner is indeterminate, counter shows running
                    // "X renamed so far".
                    total: None,
                });
                let mut total_renamed = 0u64;
                let mut total_skipped = 0u64;
                let mut total_errors = 0u64;
                let mut user_stopped = false;
                for r in &roots {
                    if cancel.load(Ordering::Relaxed) {
                        user_stopped = true;
                        break;
                    }
                    let _ = tx.send(EngineEvent::ActionProgress {
                        done: total_renamed,
                        current: Some(r.display().to_string()),
                    });
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
                let label = if user_stopped { "stopped" } else { "complete" };
                let _ = tx.send(EngineEvent::ActionFinished {
                    summary: format!(
                        "Unsuperdeduper {label} · {} renamed, {} skipped, {} errors.",
                        total_renamed, total_skipped, total_errors,
                    ),
                });
            })
            .expect("spawn unsuperdeduper thread");
    }

    /// Bulk Recycle / Nuke across every visible duplicate group's
    /// non-keepers. Mirrors the per-group worker but iterates across
    /// all groups, sums up totals, and reports a single summary.
    /// Reference paths are filtered out so a "Nuke all" can never
    /// touch a source-of-truth root. Respects the
    /// hide-unreclaimable toggle so a user who explicitly chose
    /// "show only reclaimable" doesn't get hardlink-equivalent
    /// groups blown away.
    fn run_bulk_destructive_threaded(&self, action: DedupeAction, label_emoji: &str) {
        self.action_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.action_cancel);
        let tx = self.tx.clone();
        // Own the label so the spawned thread can outlive the &str.
        let label_emoji: String = label_emoji.to_string();
        let reference_roots: Vec<PathBuf> = self
            .persisted
            .roots
            .iter()
            .filter(|r| r.is_reference)
            .map(|r| r.path.clone())
            .collect();
        let hide_unreclaimable = self.groups_state.hide_unreclaimable;
        let groups: Vec<Vec<PathBuf>> = self
            .state
            .duplicates
            .iter()
            .filter_map(|g| {
                if g.files.len() < 2 {
                    return None;
                }
                // SAFETY GATE: when the user has the
                // "hide unreclaimable" toggle on, skip 0-byte-
                // reclaimable groups (link_equivalent +
                // partial-hardlinks with unique_inodes < 2).
                // Hiding them visually + still acting on them via
                // Go would be a silent-destruction trap.
                if hide_unreclaimable && crate::gui::state::inode_aware_savings(g) == 0 {
                    return None;
                }
                let dupes: Vec<PathBuf> = g.files[1..]
                    .iter()
                    .filter(|p| !reference_roots.iter().any(|r| p.starts_with(r)))
                    .cloned()
                    .collect();
                if dupes.is_empty() {
                    None
                } else {
                    Some(dupes)
                }
            })
            .collect();
        let total: u64 = groups.iter().map(|d| d.len() as u64).sum();
        let action_label = format!(
            "{label_emoji} · {total} file(s) across {} group(s)",
            groups.len()
        );
        std::thread::Builder::new()
            .name("superdeduper-bulk-destructive".into())
            .spawn(move || {
                let _ = tx.send(EngineEvent::ActionStarted {
                    name: action_label,
                    total: Some(total),
                });
                let mut done = 0u64;
                let mut failed = 0u64;
                let mut processed = 0u64;
                let mut user_stopped = false;
                'outer: for dupes in &groups {
                    for d in dupes {
                        if cancel.load(Ordering::Relaxed) {
                            user_stopped = true;
                            break 'outer;
                        }
                        let _ = tx.send(EngineEvent::ActionProgress {
                            done: processed,
                            current: Some(d.display().to_string()),
                        });
                        let r = match action {
                            DedupeAction::Recycle => crate::dedupe::action_recycle(d),
                            DedupeAction::Remove => crate::dedupe::action_remove(d),
                            // Other variants aren't valid for bulk-
                            // destructive here; treat as a no-op +
                            // failure so the caller sees an
                            // explicit error rather than a silent
                            // success.
                            _ => Err(crate::Error::other(format!(
                                "bulk-destructive: unsupported action {action:?}",
                            ))),
                        };
                        match r {
                            Ok(()) => done += 1,
                            Err(e) => {
                                failed += 1;
                                let _ = tx.send(EngineEvent::Log {
                                    level: crate::gui::events::LogLevel::Warn,
                                    message: format!(
                                        "{label_emoji} failed · {} · {e}",
                                        d.display()
                                    ),
                                });
                            }
                        }
                        processed += 1;
                    }
                }
                let label = if user_stopped { "stopped" } else { "complete" };
                let _ = tx.send(EngineEvent::ActionFinished {
                    summary: format!("{label_emoji} {label} · {done} done, {failed} failed.",),
                });
            })
            .expect("spawn bulk-destructive thread");
    }

    fn run_action_threaded(&self, action: DedupeAction, keeper: PathBuf, dupes: Vec<PathBuf>) {
        self.action_cancel.store(false, Ordering::Relaxed);
        let cancel = Arc::clone(&self.action_cancel);
        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("superdeduper-action".into())
            .spawn(move || {
                let mut done = 0u64;
                let mut failed = 0u64;
                let mut renamed_paths: Vec<PathBuf> = Vec::new();
                let total = dupes.len() as u64;
                let _ = tx.send(EngineEvent::ActionStarted {
                    name: format!(
                        "{:?} · {} file(s) → keeper {}",
                        action,
                        total,
                        keeper
                            .file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_else(|| keeper.display().to_string())
                    ),
                    total: Some(total),
                });
                let mut user_stopped = false;
                for (i, d) in dupes.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        user_stopped = true;
                        break;
                    }
                    let _ = tx.send(EngineEvent::ActionProgress {
                        done: i as u64,
                        current: Some(d.display().to_string()),
                    });
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
                let label = if user_stopped { "stopped" } else { "complete" };
                let _ = tx.send(EngineEvent::ActionFinished {
                    summary: format!("Action {label} · {} done, {} failed.", done, failed),
                });
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

        // Alpha-software warning modal — shown on launch unless the
        // user has previously clicked "Don't show again", in which
        // case `persisted.settings.dismissed_alpha_warning` is true
        // and we skip. Once acknowledged this session,
        // `alpha_warning_acked_session` blocks re-render within the
        // same run. We render this BEFORE any other UI so a brand
        // new user sees the warning even before scan controls.
        if !self.persisted.settings.dismissed_alpha_warning && !self.alpha_warning_acked_session {
            CentralPanel::default()
                .frame(Frame::default().fill(theme::BG).inner_margin(0.0))
                .show(ctx, |_ui| { /* dimmed backdrop only */ });
            if let Some(choice) = crate::gui::widgets::alpha_warning::show(ctx) {
                self.alpha_warning_acked_session = true;
                if choice
                    == crate::gui::widgets::alpha_warning::AlphaWarningChoice::AcknowledgeForever
                {
                    self.persisted.settings.dismissed_alpha_warning = true;
                }
            }
            return;
        }

        // Action-progress modal — overlays the GUI while a worker
        // thread is processing a destructive action (recycle,
        // hardlink, safe-rename, archive, unsuperdeduper). Force a
        // 33-ms repaint while it's up so the spinner animates even
        // when no other event is firing. Rendered AFTER the rest of
        // the UI below so it sits on top.
        if self.state.action_in_progress.is_some() {
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

        // #51 — Settings-drift modal. Rendered as a non-blocking
        // window over the regular UI; the user can still see their
        // edited Roots/Settings panel while reading the prompt,
        // which makes "did I really mean to change this?" easier to
        // answer. start_live() refuses to launch while this is Some.
        if let Some(summary) = self.pending_drift_modal.clone() {
            if let Some(choice) = settings_drift_modal::show(ctx, &summary) {
                self.pending_drift_modal = None;
                match choice {
                    SettingsDriftChoice::ContinueWithNew => self.accept_drift_continue(),
                    SettingsDriftChoice::RevertToPaused => self.accept_drift_revert(),
                    SettingsDriftChoice::Cancel => {}
                }
            }
        }

        // #41 — App-start resubmit-prompt modal. Non-blocking. The
        // pending list was populated in `new()`; the user picks
        // [Resubmit all] / [Open History] / [Not now] and the modal
        // dismisses. Telemetry-off builds never reach this code path
        // (field doesn't exist + widget module is cfg-gated).
        #[cfg(feature = "telemetry")]
        if let Some(rows) = self.pending_resubmit_prompt.clone() {
            if let Some(choice) = crate::gui::widgets::resubmit_prompt_modal::show(ctx, &rows) {
                self.pending_resubmit_prompt = None;
                use crate::gui::widgets::resubmit_prompt_modal::ResubmitPromptChoice;
                match choice {
                    ResubmitPromptChoice::ResubmitAll => {
                        // Kick the first row through the worker;
                        // History panel surfaces per-row state +
                        // (when v2.1 lands) auto-chains the rest.
                        // For now, the user can click Resubmit on
                        // the History tab for any rows the worker
                        // didn't get to.
                        if let Some(first) = rows.first() {
                            if let Err(e) = crate::gui::resubmit::request_resubmit(&first.scan_id) {
                                self.state.push_log(
                                    crate::gui::events::LogLevel::Warn,
                                    format!("Couldn't start resubmit: {e}"),
                                );
                            }
                        }
                        self.persisted.results_tab = ResultsTab::History;
                    }
                    ResubmitPromptChoice::OpenHistory => {
                        self.persisted.results_tab = ResultsTab::History;
                    }
                    ResubmitPromptChoice::NotNow => {}
                }
            }
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
            if settings_modal::show(
                ctx,
                &mut open,
                &mut self.persisted.settings,
                &mut self.settings_modal_state,
            ) {
                self.settings_open = false;
            } else {
                self.settings_open = open;
            }
        }

        // Channel banner — always-on 32px coloured strip when the
        // active channel is not prod (per dev-channel-spec.md §3.4).
        // Rendered as the FIRST top panel so it sits at the very top
        // of the window, above the menubar. Reads the active channel
        // from the channel module's process-global cell — set once
        // at GUI startup via channel::set_active_channel.
        crate::gui::widgets::channel_banner::show(ctx, crate::channel::active_channel());

        // File menubar — owns project lifecycle (New / Open / Save /
        // Save As / Open Archive Manifest). Rendered as a thin strip
        // above the header so it doesn't intrude on the always-visible
        // status bar.
        let mut want_settings = false;
        let mut menu_action: Option<MenuAction> = None;
        // Brand strip: shield logo + "SuperDeDuper" name above
        // the menubar (Mick 2026-05-25T03:45Z). Dedicated panel
        // so the menubar stays compact; logo runs ~84px tall, the
        // shield's transparent PNG shows the PANEL_DEEP through
        // its negative space.
        TopBottomPanel::top("brand")
            .frame(
                Frame::default()
                    .fill(theme::PANEL_DEEP)
                    .inner_margin(egui::vec2(12.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::Image::new(egui::include_image!("../../assets/sdd-color-shield.png"))
                            .max_size(egui::vec2(84.0, 84.0)),
                    );
                    ui.add_space(12.0);
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.label(
                            egui::RichText::new("SuperDeDuper")
                                .color(theme::TEXT_HI)
                                .strong()
                                .size(36.0),
                        );
                    });
                });
            });

        TopBottomPanel::top("menubar")
            .frame(Frame::default().fill(theme::PANEL_DEEP).inner_margin(egui::vec2(8.0, 2.0)))
            .show(ctx, |ui| {
                egui::MenuBar::new().ui(ui, |ui| {
                    ui.menu_button("File", |ui| {
                        if ui
                            .button("New scan")
                            .on_hover_text("Clear the current project so you can start fresh. Doesn't touch your hash cache.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::New);
                            ui.close_kind(egui::UiKind::Menu);
                        }
                        ui.separator();
                        if ui
                            .button("Open Project…")
                            .on_hover_text("Pick a .superdeduper folder previously written with Save Project. Restores roots, settings, and the confirmed-duplicates list.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::OpenProject);
                            ui.close_kind(egui::UiKind::Menu);
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
                            ui.close_kind(egui::UiKind::Menu);
                        }
                        if ui
                            .button("Save Project As…")
                            .on_hover_text("Write a copy of the current project to a new .superdeduper folder.")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::SaveAs);
                            ui.close_kind(egui::UiKind::Menu);
                        }
                        ui.separator();
                        if ui
                            .button("Open Archive Manifest…")
                            .on_hover_text("Future: load a manifest produced by a previous Archive Dupes run, then restore the moved files to their original locations. (Restore loader: not implemented yet — manifest opens read-only.)")
                            .clicked()
                        {
                            menu_action = Some(MenuAction::OpenArchiveManifest);
                            ui.close_kind(egui::UiKind::Menu);
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
                                            ui.close_kind(egui::UiKind::Menu);
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
                            ui.close_kind(egui::UiKind::Menu);
                        }
                    });
                });
            });

        let mut stats_rect: Option<egui::Rect> = None;
        TopBottomPanel::top("header")
            .frame(Frame::default().fill(theme::BG).inner_margin(8.0))
            .show(ctx, |ui| {
                let out = header::show(
                    ui,
                    &self.state,
                    self.persisted.settings.hash_algo,
                    self.is_scanning,
                );
                if out.action == header::HeaderAction::OpenSettings {
                    want_settings = true;
                }
                stats_rect = out.stats_rect;
            });
        let _ = stats_rect; // anchor moved to progress-bar fill_rect
                            // Cache-fast-forward effect: STRICTLY resume-only. Only fires
                            // while resume_effect_active is true, the engine is in
                            // Hashing, and the rate exceeds the fast-forward threshold.
                            // After catch-up (Sparkles emits `left_fast_forward`) we
                            // clear resume_effect_active so the effect ends and the bar
                            // returns to its normal render for the rest of the scan.
        if self.resume_effect_active
            && matches!(
                self.state.overall.stage,
                crate::gui::events::OverallStage::Hashing
            )
        {
            let signals = self
                .sparkles
                .tick(self.state.overall.done, self.last_bar_fill);
            // Resume catch-up sounds intentionally removed — the
            // synth attempts didn't land. Scan-finish chime in
            // state.rs is untouched.
            if signals.left_fast_forward {
                self.resume_effect_active = false;
            }
            if self.sparkles.active() {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            }
        }
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
                // While the resume cache-fast-forward is active the
                // bar fills in dystopian red instead of the normal
                // accent teal. Snaps back the very next frame after
                // catch-up.
                let ff = self.resume_effect_active && self.sparkles.is_fast_forwarding();
                let bar_rects = overall_bar::show_with(ui, &self.state, ff);
                self.last_bar_fill = bar_rects.fill;
            });

        SidePanel::left("sidebar")
            .resizable(true)
            .default_width(300.0)
            .min_width(240.0)
            .frame(Frame::default().fill(theme::PANEL).inner_margin(10.0))
            .show(ctx, |ui| {
                // Render the "cached scan available" banner before
                // the scan controls so it's visible above the Start
                // button. The banner is a no-op when no cache exists
                // for the current roots OR when the user has
                // settings.always_use_cache enabled.
                crate::gui::widgets::cache_banner::show(
                    ui,
                    &mut self.state,
                    self.persisted.settings.always_use_cache,
                );
                // #25 v2.5: scan-mode dropdown above the Roots panel
                // per spec §3.8 ("top of scan-config panel"). Selection
                // lives on `self.scan_mode` — session-sticky, NOT
                // persisted across launches per spec.
                crate::gui::widgets::scan_mode_picker::show(
                    ui,
                    &mut self.scan_mode,
                    self.is_scanning,
                );
                ui.add_space(6.0);
                let roots_action =
                    roots_panel::show(ui, &self.persisted.roots, self.is_scanning, self.can_resume);
                if let Some(a) = roots_action {
                    self.dispatch_root_action(a);
                }
                ui.add_space(12.0);
                ui.separator();
                ui.add_space(6.0);
                funnel::show(ui, &self.state, self.persisted.settings.hash_algo);

                // §10.4 badge-wall: bottom-left always-visible
                // achievements grid. Auto-degrades to §10.5 mini-
                // widget when the sidebar is narrower than ~280 px
                // (3 tile-columns won't fit).
                #[cfg(feature = "telemetry")]
                {
                    ui.add_space(12.0);
                    ui.separator();
                    ui.add_space(6.0);
                    let state = crate::leaderboard::catalog::peek_state();
                    let action = if ui.available_width() < 280.0 {
                        crate::gui::widgets::badge_wall::show_mini(ui, &state)
                    } else {
                        crate::gui::widgets::badge_wall::show(ui, &state)
                    };
                    if let Some(a) = action {
                        self.dispatch_badge_wall_action(a);
                    }
                }
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
                            .id_salt("drive-scope")
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
                    pick("History", ResultsTab::History);
                    pick("Preview", ResultsTab::Preview);
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
                            self.is_scanning,
                        );
                    }
                    ResultsTab::Log => log_panel::show(ui, &self.state),
                    ResultsTab::History => {
                        crate::gui::widgets::scan_history_panel::show(ui);
                    }
                    ResultsTab::Preview => {
                        if let Some(path) = self.previewed_file.clone() {
                            if let Some(action) =
                                crate::gui::preview::show(ui, &path, &mut self.preview_state)
                            {
                                use crate::gui::preview::PreviewAction;
                                match action {
                                    PreviewAction::Close => {
                                        self.previewed_file = None;
                                    }
                                    PreviewAction::ForceHex | PreviewAction::ForceText => {
                                        // Mode override already stored by the widget.
                                    }
                                }
                            }
                        } else {
                            ui.add_space(40.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    egui::RichText::new("No file selected")
                                        .color(theme::TEXT_LO)
                                        .heading(),
                                );
                                ui.label(
                                    egui::RichText::new(
                                        "Click 👁 on any duplicate group's keeper row \
                                         to preview it here.",
                                    )
                                    .color(theme::TEXT_LO)
                                    .size(12.0),
                                );
                            });
                        }
                    }
                }
                if let Some(a) = group_action {
                    self.dispatch_group_action(a);
                }
            });

        // Action-progress modal renders LAST so it overlays
        // everything else (CentralPanel, SidePanel, TopBottomPanels).
        // egui Window-with-anchor handles the z-order; we just have
        // to call it after the rest of the UI. Returns true if Stop
        // was clicked → set the shared cancel atomic so the worker
        // bails on its next loop iteration.
        if let Some(action) = self.state.action_in_progress.clone() {
            let stopped = crate::gui::widgets::action_progress::show(ctx, &action);
            if stopped {
                self.action_cancel.store(true, Ordering::Relaxed);
            }
        }

        // Pre-flight score-card modal renders after the action-progress
        // overlay so it sits on top if both are somehow active. In
        // practice they're mutually exclusive (preflight is pre-scan,
        // action-progress is post-results).
        self.tick_preflight(ctx);

        // Post-scan leaderboard "dopamine moment" modal. Pops on
        // ScanFinished with ShareDefault::AlwaysAsk; AutoOptIn
        // submits silently; Never skips both. Mutually exclusive
        // with preflight (both are scan-boundary modals).
        #[cfg(feature = "telemetry")]
        self.tick_scan_complete_modal(ctx);

        // Corner toasts (rank-ready, etc). Renders in the foreground
        // layer over everything; auto-expires + fades. Pushed from
        // background workers via gui::widgets::toast::push().
        crate::gui::widgets::toast::show(ctx);

        // Sparkles render LAST in their own foreground layer +
        // clipped to the progress-bar fill rect. Anything that
        // would have drawn outside the bar's bounds is dropped.
        let fill = self.last_bar_fill;
        egui::Area::new(egui::Id::new("sd-sparkles-overlay"))
            .order(egui::Order::Foreground)
            .interactable(false)
            .fixed_pos(egui::pos2(0.0, 0.0))
            .show(ctx, |ui| {
                self.sparkles.paint(ui, fill);
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
        GroupAction::RecycleAllVisible => {
            "Send EVERY non-keeper across EVERY currently visible duplicate group to the OS Recycle \
             Bin. Recoverable from the recycle bin until you empty it. Reference paths are never \
             touched."
                .to_string()
        }
        GroupAction::NukeAllVisible => {
            "PERMANENTLY DELETE every non-keeper across every currently visible duplicate group. \
             No recycle bin, no .superdeduper rename, no undo. Reference paths are never touched. \
             Only use when you're certain you don't need any of these files."
                .to_string()
        }
        // Reveal / Open* should never reach this code path — they
        // bypass the modal in `dispatch_group_action` — but
        // document it anyway in case future refactors widen the
        // dispatcher.
        GroupAction::Reveal(_) => {
            "(internal: Reveal-in-Explorer reached the destructive modal — this is a bug)".into()
        }
        GroupAction::OpenFile(_) => {
            "(internal: Open-file reached the destructive modal — this is a bug)".into()
        }
        GroupAction::OpenFolder(_) => {
            "(internal: Open-folder reached the destructive modal — this is a bug)".into()
        }
        GroupAction::PromoteKeeper { .. } => {
            "(internal: Promote-keeper reached the destructive modal — this is a bug)".into()
        }
        GroupAction::Preview(_) => {
            "(internal: Preview reached the destructive modal — this is a bug)".into()
        }
    }
}

#[cfg(windows)]
fn reveal_in_explorer(path: &std::path::Path) {
    // /select,<path> highlights the file in its parent folder.
    // The comma is a literal separator — explorer.exe expects
    // /select,PATH with no space after the comma. Spaces in the
    // path are fine because we pass /select,PATH as a single
    // command-line argument and Command::arg handles its own
    // quoting. Use the OS-native path repr (backslashes on
    // Windows) so explorer doesn't trip on forward slashes coming
    // out of WSL-cross-compiled binaries.
    let arg = format!("/select,{}", path.display());
    if std::process::Command::new("explorer.exe")
        .arg(&arg)
        .spawn()
        .is_err()
    {
        // Fallback: open the parent folder. /select can fail if the
        // file vanished between the click and the spawn (e.g. another
        // action just renamed it).
        if let Some(parent) = path.parent() {
            let _ = std::process::Command::new("explorer.exe")
                .arg(parent)
                .spawn();
        }
    }
}

/// Open the file with the user's default application (the
/// equivalent of double-clicking it in Explorer). Used by the
/// "📂 Open file" button on each group row.
#[cfg(windows)]
fn open_file_default_app(path: &std::path::Path) {
    // `explorer.exe <file>` invokes the file's registered handler,
    // matching what double-click does. Using `cmd /c start` works
    // too but spawns an extra console window on some shells.
    let _ = std::process::Command::new("explorer.exe").arg(path).spawn();
}

/// Open the file's enclosing directory in Explorer, no file
/// selected. Distinct from `reveal_in_explorer` which highlights
/// the file inside the folder.
#[cfg(windows)]
fn open_enclosing_folder(path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        let _ = std::process::Command::new("explorer.exe")
            .arg(parent)
            .spawn();
    }
}

#[cfg(not(windows))]
fn reveal_in_explorer(_path: &std::path::Path) {}
#[cfg(not(windows))]
fn open_file_default_app(_path: &std::path::Path) {}
#[cfg(not(windows))]
fn open_enclosing_folder(_path: &std::path::Path) {}

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

/// Open a URL in the user's default browser. Per-OS shell-out;
/// non-blocking. Failure logs to stderr and is otherwise silent —
/// this is invoked from UI button clicks, no good way to surface
/// "browser refused to launch" inline.
#[cfg(feature = "telemetry")]
fn open_url_in_browser(url: &str) {
    let result = if cfg!(target_os = "windows") {
        std::process::Command::new("cmd")
            .args(["/c", "start", "", url])
            .spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        eprintln!("failed to open browser to {url}: {e}");
    }
}
