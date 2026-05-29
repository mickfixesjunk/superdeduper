//! T-BENCH-ME GUI "Benchmark" button + pre-run consent/explainer modal +
//! progress/result, per design/bench-gui-button-spec.md. Default opt-out:
//! nothing downloads/runs/submits until the user clicks an action. Runs on a
//! worker thread over the shared `leaderboard::bench_run::run` (CLI parity).
#![cfg(all(feature = "gui", feature = "telemetry"))]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Where the modal is in its lifecycle.
#[derive(Clone, PartialEq)]
pub enum Phase {
    /// Pre-run consent/explainer (the load-bearing warnings).
    Consent,
    /// Worker running; `status` carries the current stage line.
    Running,
    /// Finished — `status` holds the human result; `ranked`/`verify_only`/error flavor.
    Done,
}

/// Worker-updated shared state (UI polls it each frame).
#[derive(Default)]
pub struct Shared {
    pub status: String,
    pub finished: bool,
    /// Human result line for the Done view.
    pub result: Option<String>,
    /// true => an error/rejection (render in the error style), false => success.
    pub is_error: bool,
    /// true when the run submitted (vs local-only).
    pub submitted: bool,
}

/// A user-selectable benchmark tier. The 100MB `corpus-v1-smoke` tier is
/// intentionally NOT listed here (design 2026-05-29: smoke is test-only,
/// reachable only via the CLI `--corpus-version corpus-v1-smoke`). The
/// GUI exposes only the RANKED tiers that land on the Hall of Fame.
#[derive(Clone, Copy)]
pub struct BenchTierChoice {
    /// corpus_version sent to /bench/start.
    pub corpus_version: &'static str,
    /// tier string sent to /bench/start.
    pub tier: &'static str,
    /// Short selector label.
    pub label: &'static str,
    /// Human download-size hint for the consent copy.
    pub approx_size: &'static str,
}

/// User-exposed ranked tiers, in selector order. Default is index 0.
/// Per design's CORRECTED corpus ids (2026-05-29): the 2.5GB RANKED quick
/// tier is `corpus-v2-quick` (live on dev: 2.41GB / 8600 files); the old
/// corpus-v1-quick is now the 100MB SMOKE (test-only, CLI-only). The 6GB
/// ranked `corpus-v2-full` slots in here when web hosts it (HoF). Using a
/// fresh v2 id (not overwriting v1-quick) also keeps the cache key honest.
pub const USER_TIERS: &[BenchTierChoice] = &[BenchTierChoice {
    corpus_version: "corpus-v2-quick",
    tier: "quick",
    label: "Quick (ranked, ~2.4 GB)",
    approx_size: "~2.4 GB",
}];

/// GUI-side bench state, held in the app. `None` modal => button not clicked.
pub struct BenchUiState {
    pub open: bool,
    pub phase: Phase,
    /// Modal toggle: re-download the corpus fresh (maps to bench_run `fresh`).
    pub fresh: bool,
    /// Selected ranked tier (index into `USER_TIERS`). Defaults to 0 (Quick).
    pub tier_idx: usize,
    /// Show the "What gets shared?" full-JSON preview inline.
    pub show_share_preview: bool,
    pub shared: Arc<Mutex<Shared>>,
    pub cancel: Arc<AtomicBool>,
}

impl Default for BenchUiState {
    fn default() -> Self {
        Self {
            open: false,
            phase: Phase::Consent,
            fresh: false,
            tier_idx: 0,
            show_share_preview: false,
            shared: Arc::new(Mutex::new(Shared::default())),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl BenchUiState {
    /// Open the consent modal (called by the "Benchmark" button). Resets state.
    pub fn open(&mut self) {
        self.open = true;
        self.phase = Phase::Consent;
        self.fresh = false;
        self.tier_idx = 0;
        self.show_share_preview = false;
        self.cancel.store(false, Ordering::Relaxed);
        *self.shared.lock().unwrap() = Shared::default();
    }

    /// The currently-selected tier (clamped to a valid entry).
    fn tier(&self) -> BenchTierChoice {
        USER_TIERS[self.tier_idx.min(USER_TIERS.len() - 1)]
    }

    /// Spawn the bench worker. `submit` = Run&submit (true) vs Run-locally (false).
    fn start(&mut self, submit: bool) {
        self.phase = Phase::Running;
        self.cancel.store(false, Ordering::Relaxed);
        {
            let mut s = self.shared.lock().unwrap();
            *s = Shared::default();
            s.status = "starting…".into();
        }
        let shared = Arc::clone(&self.shared);
        let cancel = Arc::clone(&self.cancel);
        let fresh = self.fresh;
        let tier = self.tier();
        std::thread::spawn(move || {
            run_worker(&shared, &cancel, fresh, submit, tier);
        });
    }
}

/// Worker body: load/auto-register install, run the shared bench loop, write
/// progress + the final human result into `shared`.
fn run_worker(
    shared: &Arc<Mutex<Shared>>,
    cancel: &AtomicBool,
    fresh: bool,
    submit: bool,
    tier: BenchTierChoice,
) {
    use crate::leaderboard::{bench_run, install, registration, submission};

    let set_status = |msg: &str| {
        shared.lock().unwrap().status = msg.to_string();
    };
    let finish = |result: String, is_error: bool, submitted: bool| {
        let mut s = shared.lock().unwrap();
        s.result = Some(result);
        s.is_error = is_error;
        s.submitted = submitted;
        s.finished = true;
    };

    // Load install; auto-register transparently if needed (spec §5 edge:
    // install_unknown -> register, don't dead-end).
    set_status("preparing (checking registration)…");
    let channel = crate::channel::active_channel();
    let state = match install::load_for(channel) {
        Ok(Some(s)) if s.registered => s,
        Ok(Some(mut s)) => match registration::register_cli(&mut s) {
            Ok(()) => s,
            Err(e) => return finish(format!("Couldn't register this install: {e:?}"), true, false),
        },
        Ok(None) => {
            // resolve_server_url honors SUPERDEDUPER_SERVER_URL so a fresh
            // (never-registered) install registers against the mock too
            // (T-BENCH-ME spec §9 G1).
            let url = crate::channel::resolve_server_url(channel);
            let mut s = install::new_unregistered(url);
            match registration::register_cli(&mut s) {
                Ok(()) => s,
                Err(e) => return finish(format!("Couldn't register this install: {e:?}"), true, false),
            }
        }
        Err(e) => return finish(format!("Install state error: {e}"), true, false),
    };

    let outcome = bench_run::run(
        &state,
        tier.corpus_version,
        tier.tier,
        None,
        fresh,
        submit,
        cancel,
        |m| set_status(m),
    );

    match outcome {
        Err(e) if e.downcast_ref::<bench_run::Cancelled>().is_some() => {
            finish("Cancelled — nothing was submitted.".into(), false, false)
        }
        Err(e) => finish(format!("Benchmark failed: {e}"), true, false),
        Ok(o) => {
            // Human size: GB for >=1 GiB (the ranked corpora), else MB.
            let bytes = o.bytes_scanned as f64;
            let size = if bytes >= 1_073_741_824.0 {
                format!("{:.2} GB", bytes / 1_073_741_824.0)
            } else {
                format!("{:.0} MB", bytes / 1_048_576.0)
            };
            // bytes_scanned is the CANDIDATE bytes hashed (size-grouped),
            // not the whole corpus -> word it as "hashed X of candidates"
            // rather than implying X is the corpus size.
            let perf = format!(
                "Deduped the test corpus in {:.2}s — hashed {size} of duplicate candidates, {} groups.",
                o.dedupe_secs, o.dup_groups
            );
            match o.submit {
                None => finish(
                    format!("{perf}\nRan locally — not submitted."),
                    false,
                    false,
                ),
                Some(submission::SubmitOutcome::Accepted { ranks, achievements_unlocked, .. }) => {
                    // Honest result: the GUI only runs RANKED tiers
                    // (corpus-v1-quick = 2.5GB ranked). A returned rank is
                    // shown verbatim; an empty ranks[] means submitted-OK
                    // but the rank isn't computed yet (don't fabricate one).
                    let mut msg = if let Some(top) = ranks.first() {
                        format!("Verified! You ranked on the Dedupe Hall of Fame.\n{top:?}")
                    } else {
                        format!(
                            "Verified + submitted to the Dedupe Hall of Fame.\n\
                             Your rank will appear on your profile shortly."
                        )
                    };
                    if !achievements_unlocked.is_empty() {
                        msg.push_str(&format!("\nAchievements: {}", achievements_unlocked.join(", ")));
                    }
                    finish(format!("{perf}\n{msg}"), false, true)
                }
                Some(submission::SubmitOutcome::Rejected { status, reason }) => finish(
                    format!("{perf}\nNot accepted (status {status}): {reason}"),
                    true,
                    true,
                ),
                Some(other) => finish(format!("{perf}\n{other:?}"), false, true),
            }
        }
    }
}

/// What the modal asks the app to do after this frame.
pub enum BenchModalAction {
    None,
    /// Open the existing full-share-preview / privacy surface (Settings -> Privacy).
    OpenSharePreview,
}

/// Render the modal if open. Returns an action for the app to handle (e.g.
/// route to the existing share-preview). Call once per frame from `update`.
pub fn show(state: &mut BenchUiState, ctx: &egui::Context) -> BenchModalAction {
    if !state.open {
        return BenchModalAction::None;
    }
    let mut action = BenchModalAction::None;
    let mut open = true;
    egui::Window::new("Benchmark your machine — Dedupe Hall of Fame")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| match state.phase.clone() {
            Phase::Consent => consent_view(state, ui, &mut action),
            Phase::Running => running_view(state, ui, ctx),
            Phase::Done => done_view(state, ui),
        });
    // window [x] closed: treat as cancel (abort any run).
    if !open {
        state.cancel.store(true, Ordering::Relaxed);
        state.open = false;
    }
    action
}

fn consent_view(state: &mut BenchUiState, ui: &mut egui::Ui, action: &mut BenchModalAction) {
    let size = state.tier().approx_size;
    ui.label(egui::RichText::new("What happens").strong());
    ui.label(format!(
        "1. Downloads a standard synthetic ranked corpus ({size}; cached after the first run)."
    ));
    ui.label("2. Runs SuperDeDuper's real dedup on it and times it.");
    ui.label("3. Submits your result to the public Dedupe Hall of Fame.");
    ui.add_space(6.0);
    ui.label(egui::RichText::new("Privacy").strong());
    // Consent-copy per corrected spec §2 (testdesign G3 / consent-integrity):
    // category gist + firm no-PII; NO "only [closed list]" (the wire has ~12
    // hardware fields and a hand-list drifts); CPU model + ISA flags are sent
    // AS-IS (not "bucketed"); the "What exactly gets shared?" preview, rendered
    // from the REAL payload, is the single authoritative source of truth.
    ui.label(
        "This benchmark touches no personal files — the corpus is synthetic test data, not your \
         disk. We submit your dedup result plus an anonymous hardware profile (CPU model + \
         instruction-set flags, disk class, RAM, OS, and similar hardware fields) and your \
         anonymous install ID. We never send file names, paths, contents, hashes, username, \
         machine name, or IP. The exact, complete list of every field is in \"What exactly gets \
         shared?\" below — that preview is the authoritative source of truth.",
    );
    if ui.link("What exactly gets shared?").clicked() {
        *action = BenchModalAction::OpenSharePreview;
        state.show_share_preview = true;
    }
    if state.show_share_preview {
        if let Some(json) = sample_share_json() {
            egui::ScrollArea::vertical().max_height(160.0).show(ui, |ui| {
                ui.code(json);
            });
        }
    }
    ui.add_space(6.0);
    // Tier selector (design 2026-05-29): the GUI exposes only RANKED tiers;
    // the 100MB smoke corpus is CLI-only. Default is USER_TIERS[0] (Quick).
    if USER_TIERS.len() > 1 {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Benchmark").strong());
            egui::ComboBox::from_id_salt("sd_bench_tier")
                .selected_text(USER_TIERS[state.tier_idx.min(USER_TIERS.len() - 1)].label)
                .show_ui(ui, |ui| {
                    for (i, t) in USER_TIERS.iter().enumerate() {
                        ui.selectable_value(&mut state.tier_idx, i, t.label);
                    }
                });
        });
        ui.add_space(6.0);
    }
    ui.label(egui::RichText::new("Workload").strong());
    ui.label(format!(
        "Downloads {size} (once; cached after) and runs a CPU + disk workload. Run it when your \
         machine is otherwise idle for an accurate, comparable result."
    ));
    ui.checkbox(&mut state.fresh, "Re-download a fresh corpus (ignore cache)");
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button(egui::RichText::new("Run & submit").strong()).clicked() {
            state.start(true);
        }
        if ui.button("Run locally only").clicked() {
            state.start(false);
        }
        if ui.button("Cancel").clicked() {
            state.open = false;
        }
    });
}

fn running_view(state: &mut BenchUiState, ui: &mut egui::Ui, ctx: &egui::Context) {
    let (status, finished) = {
        let s = state.shared.lock().unwrap();
        (s.status.clone(), s.finished)
    };
    ui.horizontal(|ui| {
        ui.spinner();
        ui.label(&status);
    });
    ui.add_space(8.0);
    if ui.button("Cancel").clicked() {
        state.cancel.store(true, Ordering::Relaxed);
    }
    if finished {
        state.phase = Phase::Done;
    }
    // keep polling the worker.
    ctx.request_repaint_after(std::time::Duration::from_millis(120));
}

fn done_view(state: &mut BenchUiState, ui: &mut egui::Ui) {
    let (result, is_error) = {
        let s = state.shared.lock().unwrap();
        (s.result.clone().unwrap_or_default(), s.is_error)
    };
    let color = if is_error { egui::Color32::from_rgb(220, 120, 90) } else { egui::Color32::from_rgb(120, 200, 140) };
    ui.label(egui::RichText::new(result).color(color));
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Run again").clicked() {
            state.phase = Phase::Consent;
        }
        if ui.button("Close").clicked() {
            state.open = false;
        }
    });
}

/// The full-schema submission preview JSON for the BENCH context. Renders
/// the canonical-bench shape (corpus_kind=canonical-bench, synthetic ~2.4GB
/// numbers, + the bench fields), NOT the generic scan sample — the scan
/// sample shows corpus_kind=user-data + bytes_scanned=320GB which would
/// misrepresent the synthetic bench and contradict this modal's "no
/// personal files" callout (design preview-context fix).
fn sample_share_json() -> Option<String> {
    Some(crate::gui::widgets::settings_modal::build_bench_sample_payload_json())
}
