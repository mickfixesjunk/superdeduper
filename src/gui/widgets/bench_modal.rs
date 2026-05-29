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

/// GUI-side bench state, held in the app. `None` modal => button not clicked.
pub struct BenchUiState {
    pub open: bool,
    pub phase: Phase,
    /// Modal toggle: re-download the corpus fresh (maps to bench_run `fresh`).
    pub fresh: bool,
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
        self.show_share_preview = false;
        self.cancel.store(false, Ordering::Relaxed);
        *self.shared.lock().unwrap() = Shared::default();
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
        std::thread::spawn(move || {
            run_worker(&shared, &cancel, fresh, submit);
        });
    }
}

/// Worker body: load/auto-register install, run the shared bench loop, write
/// progress + the final human result into `shared`.
fn run_worker(shared: &Arc<Mutex<Shared>>, cancel: &AtomicBool, fresh: bool, submit: bool) {
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
        "corpus-v1-quick",
        "quick",
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
            let perf = format!(
                "Deduped the {:.0} MB test corpus in {:.2}s ({} groups).",
                o.bytes_scanned as f64 / 1_048_576.0,
                o.dedupe_secs,
                o.dup_groups
            );
            match o.submit {
                None => finish(
                    format!("{perf}\nRan locally — not submitted."),
                    false,
                    false,
                ),
                Some(submission::SubmitOutcome::Accepted { ranks, achievements_unlocked, .. }) => {
                    // Honest result: <1GB dev/small is verify-only (NOT a fake
                    // rank); a real ranked tier returns ranks[].
                    let mut msg = if let Some(top) = ranks.first() {
                        format!("Verified! You ranked on the Dedupe Hall of Fame.\n{top:?}")
                    } else {
                        format!(
                            "Verified! Your machine deduped the test corpus correctly.\n\
                             (Warm-up tier — the ranked Hall of Fame uses the full corpus.)"
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
    ui.label(egui::RichText::new("What happens").strong());
    ui.label("1. Downloads a standard synthetic test corpus (~100 MB; cached after the first run).");
    ui.label("2. Runs SuperDeDuper's real dedup on it and times it.");
    ui.label("3. Submits your result to the public Dedupe Hall of Fame.");
    ui.add_space(6.0);
    ui.label(egui::RichText::new("Privacy").strong());
    ui.label(
        "This benchmark touches NO personal files — the corpus is synthetic. We submit only your \
         dedup throughput, a bucketed anonymous hardware profile (CPU/disk/RAM/OS), and your \
         anonymous install ID. No file names, paths, contents, hashes, username, or machine name.",
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
    ui.label(egui::RichText::new("Workload").strong());
    ui.label("Downloads ~100 MB (once; cached after) and runs a few-second CPU + disk workload. Run it when your machine is otherwise idle for an accurate result.");
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

/// The existing full-schema submission preview JSON (gamification §10.1) — the
/// same surface Settings -> Privacy uses; here it's shown inline for "What
/// gets shared?". Returns None if telemetry isn't available.
fn sample_share_json() -> Option<String> {
    Some(crate::gui::widgets::settings_modal::build_sample_payload_json())
}
