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
    /// Mick bench-lane UX (2026-05-30): user picks Ranked vs Casual before
    /// the consent/explainer. Default-selected reflects install.json's
    /// last_bench_lane; click either Ranked or Casual to persist + advance.
    Lane,
    /// Mick P0 hotfix (2026-05-30): when the user picks Ranked but the
    /// install isn't OAuth-linked yet, fire the loopback link flow before
    /// advancing to Consent. Worker thread runs `link_via_loopback_no_browser_fallback`;
    /// the UI shows a "signing in…" status and, if browser-open fails, the
    /// URL the user can paste into any browser to finish auth. Casual skips
    /// this phase entirely (anonymous submissions don't need an account).
    Linking,
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
    /// Post-bench deep-link URL ("View on leaderboard"). Built from the
    /// channel's frontend URL + the chosen lane + the server-assigned
    /// submission_id; `None` until /submit returns Accepted or
    /// DuplicateNoChange with an id.
    pub deep_link: Option<String>,
    /// Post-bench throughput, GB/s. Computed from bytes_scanned / dedupe_secs;
    /// rendered in the Done view's rank panel.
    pub throughput_gbps: Option<f64>,
    /// Post-bench rank rows from /submit Accepted's ranks[] (empty when the
    /// async ranks-poll hasn't resolved yet — rendered as "pending").
    pub ranks: Vec<crate::leaderboard::submission::RankEntry>,
    /// Mick P0 (2026-05-30): OAuth link worker result. None while linking
    /// is in progress; Some(Ok(display_name + provider)) on success (main
    /// thread advances to Phase::Consent); Some(Err(msg)) on failure (main
    /// thread surfaces the error on Phase::Linking with retry/casual/cancel).
    pub linking_done: Option<Result<String, String>>,
    /// Mick P0 (2026-05-30): when the system browser can't be launched
    /// (no-browser fallback), the link worker writes the auth URL here for
    /// the linking_view to render. User pastes into any browser to finish.
    pub linking_url_fallback: Option<String>,
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
    /// Mick bench-lane choice for this run. Defaulted to install.json's
    /// last_bench_lane on open; set when the user clicks Ranked or Casual
    /// on the Lane phase; threaded into run_worker so the server scores
    /// authoritatively per web d7df4c9.
    pub lane: Option<crate::leaderboard::install::BenchLane>,
    /// Mick P0 (2026-05-30): sticky error from the last OAuth link attempt,
    /// rendered on Phase::Linking with [Try again / Use Casual / Cancel].
    /// Cleared when the user picks one of those actions.
    pub linking_error: Option<String>,
    pub shared: Arc<Mutex<Shared>>,
    pub cancel: Arc<AtomicBool>,
}

impl Default for BenchUiState {
    fn default() -> Self {
        Self {
            open: false,
            phase: Phase::Lane,
            fresh: false,
            tier_idx: 0,
            show_share_preview: false,
            lane: None,
            linking_error: None,
            shared: Arc::new(Mutex::new(Shared::default())),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl BenchUiState {
    /// Open the bench modal (called by the "Benchmark" button). Resets state
    /// and starts at the Lane-prompt phase. Pre-populates lane from
    /// install.json's last_bench_lane (the default-selected button) so the
    /// modal is effectively one-click after first time.
    pub fn open(&mut self) {
        self.open = true;
        self.phase = Phase::Lane;
        self.fresh = false;
        self.tier_idx = 0;
        self.show_share_preview = false;
        self.lane = crate::leaderboard::install::load()
            .ok()
            .flatten()
            .and_then(|s| s.last_bench_lane);
        self.linking_error = None;
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
        let lane = self.lane;
        std::thread::spawn(move || {
            run_worker(&shared, &cancel, fresh, submit, tier, lane);
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
    lane: Option<crate::leaderboard::install::BenchLane>,
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
    // A-prod-undark-bench (2026-05-30 19:02 PST): prod V3 corpus live
    // (web Mick GO override). Belt-and-suspenders refuse-on-prod gate
    // removed; bench-me now reaches the real /bench endpoints on prod.
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

    // Phase 2-B 2026-06-01: bench_run::run takes install creds + a
    // submit-callback closure instead of &InstallState. Build the
    // params here; closure captures &state for the submission HTTP.
    if !state.registered {
        return finish(
            "Install not registered; run `superdeduper register` first.".into(),
            true,
            false,
        );
    }
    let install_key_arr = match state.install_key() {
        Some(k) => k,
        None => {
            return finish(
                "Install key malformed; re-register.".into(),
                true,
                false,
            );
        }
    };
    let install_key = superdeduper_bench_iface::InstallKey(install_key_arr);
    let mut submit_fn = |inputs: &crate::leaderboard::submission::SubmissionInputs| {
        crate::leaderboard::submission::submit(&state, inputs)
    };
    let outcome = bench_run::run(
        &state.install_id,
        &install_key,
        &state.server_url,
        tier.corpus_version,
        tier.tier,
        None,
        fresh,
        cancel,
        |m| set_status(m),
        lane.map(|l| l.as_slug()),
        if submit { Some(&mut submit_fn) } else { None },
    );

    match outcome {
        Err(e) if e.downcast_ref::<bench_run::Cancelled>().is_some() => {
            finish("Cancelled — nothing was submitted.".into(), false, false)
        }
        // A-bench-modal-error-surface (2026-05-30): use the alternate-Display
        // form so anyhow joins the full context chain ('outer: inner: ...').
        // Plain {e} only shows the top wrap (e.g., 'POST /bench/start failed')
        // and swallows the actual cause (e.g., '404'). Mick's v0.2.62 bug
        // diagnosis took an extra round-trip because the GUI showed only the
        // top — CLI exposed the chain via anyhow's Debug formatter.
        Err(e) => finish(format!("Benchmark failed: {e:#}"), true, false),
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
            // Mick UX: compute throughput + deep-link URL for the rank panel.
            // GB/s = bytes_scanned / dedupe_secs / 1e9. Deep link uses the
            // chosen lane (or "ranked" default) so the tab matches what the
            // user picked.
            let gbps = if o.dedupe_secs > 0.0 {
                o.bytes_scanned as f64 / o.dedupe_secs / 1e9
            } else {
                0.0
            };
            shared.lock().unwrap().throughput_gbps = Some(gbps);
            let frontend = crate::channel::frontend_url_for(channel);
            let tab = lane.map(|l| l.as_slug()).unwrap_or("ranked");
            match o.submit {
                None => finish(
                    format!("{perf}\nRan locally — not submitted."),
                    false,
                    false,
                ),
                Some(submission::SubmitOutcome::Accepted { submission_id, ranks, achievements_unlocked, .. }) => {
                    // Honest result: the GUI only runs RANKED tiers
                    // (corpus-v1-quick = 2.5GB ranked). A returned rank is
                    // shown verbatim; an empty ranks[] means submitted-OK
                    // but the rank isn't computed yet (don't fabricate one).
                    let deep_link = format!("{frontend}/leaderboard?tab={tab}&highlight={submission_id}");
                    {
                        let mut s = shared.lock().unwrap();
                        s.deep_link = Some(deep_link);
                        s.ranks = ranks.clone();
                    }
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
                Some(submission::SubmitOutcome::DuplicateNoChange { submission_id: Some(id) }) => {
                    let deep_link = format!("{frontend}/leaderboard?tab={tab}&highlight={id}");
                    shared.lock().unwrap().deep_link = Some(deep_link);
                    finish(
                        format!("{perf}\nAlready on the leaderboard (same payload).\nView the existing entry below."),
                        false,
                        true,
                    )
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

/// Render the modal if open. Call once per frame from `update`.
pub fn show(state: &mut BenchUiState, ctx: &egui::Context) {
    if !state.open {
        return;
    }
    let mut open = true;
    egui::Window::new("Benchmark your machine — Dedupe Hall of Fame")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| match state.phase.clone() {
            Phase::Lane => lane_view(state, ui),
            Phase::Linking => linking_view(state, ui),
            Phase::Consent => consent_view(state, ui),
            Phase::Running => running_view(state, ui, ctx),
            Phase::Done => done_view(state, ui),
        });
    // Mick P0 (2026-05-30): drive the linking-phase state machine — poll the
    // worker thread's result from `Shared::linking_done` and either advance
    // to Consent (on success) or stash the error on BenchUiState so the
    // linking_view shows it with retry/casual/cancel. Keep repainting while
    // linking is active so the URL fallback / status updates land
    // immediately instead of waiting for the user to move the mouse.
    if matches!(state.phase, Phase::Linking) {
        let done = state.shared.lock().unwrap().linking_done.take();
        match done {
            Some(Ok(_who)) => {
                state.linking_error = None;
                state.phase = Phase::Consent;
            }
            Some(Err(msg)) => {
                state.linking_error = Some(msg);
            }
            None => {}
        }
        ctx.request_repaint();
    }
    // window [x] closed: treat as cancel (abort any run).
    if !open {
        state.cancel.store(true, Ordering::Relaxed);
        state.open = false;
    }
}

/// Mick bench-lane UX (2026-05-30): two-button modal — Ranked (primary) vs
/// Casual (secondary). Default-selected button reflects install.json's
/// `last_bench_lane`; click either to persist the choice and advance to the
/// existing Consent phase. Keyboard nav: Enter -> primary (Ranked); Escape ->
/// closes modal (caller treats as Cancel).
fn lane_view(state: &mut BenchUiState, ui: &mut egui::Ui) {
    use crate::leaderboard::install::BenchLane;
    ui.label(egui::RichText::new("Choose a bench lane").strong().size(16.0));
    ui.add_space(4.0);
    ui.label(
        "Ranked submissions land on the public Hall of Fame and require an account \
         (Google or Discord sign-in). Casual submissions just run the bench and show \
         your throughput — no account needed.",
    );
    ui.add_space(8.0);

    let default_is_ranked = matches!(state.lane, None | Some(BenchLane::Ranked));
    let ranked_label = if default_is_ranked {
        egui::RichText::new("● Ranked — I'm not here to play games").strong()
    } else {
        egui::RichText::new("Ranked — I'm not here to play games")
    };
    let casual_label = if !default_is_ranked {
        egui::RichText::new("● Casual — just for fun").strong()
    } else {
        egui::RichText::new("Casual — just for fun")
    };

    if ui.add_sized([320.0, 36.0], egui::Button::new(ranked_label)).clicked()
        || (ui.input(|i| i.key_pressed(egui::Key::Enter)) && default_is_ranked)
    {
        state.lane = Some(BenchLane::Ranked);
        persist_lane(BenchLane::Ranked);
        // Mick P0 fix (2026-05-30): Ranked requires an OAuth link. If the
        // install isn't linked yet, route through Phase::Linking which fires
        // the loopback flow; otherwise skip straight to Consent. Mirrors the
        // CLI gate in main::run_bench_me.
        advance_from_lane_to_ranked(state);
    }
    ui.add_space(4.0);
    if ui.add_sized([320.0, 36.0], egui::Button::new(casual_label)).clicked()
        || (ui.input(|i| i.key_pressed(egui::Key::Enter)) && !default_is_ranked)
    {
        state.lane = Some(BenchLane::Casual);
        persist_lane(BenchLane::Casual);
        // Casual is anonymous — no OAuth needed.
        state.phase = Phase::Consent;
    }
    ui.add_space(8.0);
    if ui.button("Cancel").clicked() {
        state.open = false;
    }
}

/// Mick P0 (2026-05-30): user picked Ranked on the lane modal. If already
/// OAuth-linked, advance to Consent (no extra friction). If anonymous, set
/// Phase::Linking and spawn the link worker — the worker handles install
/// load/register + the loopback OAuth flow, writing the outcome back via
/// `Shared::linking_done` for the main thread to poll. If status_for itself
/// errors (rare — disk i/o on the token file), surface that as a Linking
/// error so the user can retry/cancel/casual rather than getting wedged.
fn advance_from_lane_to_ranked(state: &mut BenchUiState) {
    use crate::leaderboard::oauth;
    let channel = crate::channel::active_channel();
    match oauth::status_for(channel) {
        Ok(oauth::AccountStatus::Linked { .. }) => {
            state.phase = Phase::Consent;
        }
        Ok(oauth::AccountStatus::Anonymous) => {
            start_oauth_link(state);
        }
        Err(e) => {
            state.linking_error = Some(format!(
                "Couldn't read sign-in status: {e}. Try again, or pick Casual."
            ));
            state.phase = Phase::Linking;
        }
    }
}

/// Spawn the OAuth link worker thread and switch to Phase::Linking. Resets
/// any prior linking state on `Shared` and clears the sticky error on
/// `BenchUiState` so a Retry click re-arms cleanly.
fn start_oauth_link(state: &mut BenchUiState) {
    state.linking_error = None;
    {
        let mut s = state.shared.lock().unwrap();
        s.linking_done = None;
        s.linking_url_fallback = None;
        s.status = "Opening your browser for sign-in…".into();
    }
    state.phase = Phase::Linking;
    let shared = Arc::clone(&state.shared);
    std::thread::spawn(move || link_worker(&shared));
}

/// Worker body: load/auto-register the install (CLI parity), then run the
/// loopback OAuth link with the no-browser fallback writing the URL back to
/// `Shared::linking_url_fallback` so the linking_view can render it. Final
/// outcome lands in `Shared::linking_done` for the main thread to drive the
/// phase transition.
fn link_worker(shared: &Arc<Mutex<Shared>>) {
    use crate::leaderboard::{install, oauth, registration};
    let channel = crate::channel::active_channel();

    let state = match install::load_for(channel) {
        Ok(Some(s)) if s.registered => s,
        Ok(Some(mut s)) => match registration::register_cli(&mut s) {
            Ok(()) => s,
            Err(e) => {
                shared.lock().unwrap().linking_done =
                    Some(Err(format!("Couldn't register this install: {e:?}")));
                return;
            }
        },
        Ok(None) => {
            let url = crate::channel::resolve_server_url(channel);
            let mut s = install::new_unregistered(url);
            match registration::register_cli(&mut s) {
                Ok(()) => s,
                Err(e) => {
                    shared.lock().unwrap().linking_done =
                        Some(Err(format!("Couldn't register this install: {e:?}")));
                    return;
                }
            }
        }
        Err(e) => {
            shared.lock().unwrap().linking_done =
                Some(Err(format!("Install state error: {e}")));
            return;
        }
    };

    let server_url = crate::channel::server_url_for(channel);
    let shared_for_fallback = Arc::clone(shared);
    let result = oauth::link_via_loopback_no_browser_fallback(
        oauth::Provider::Google,
        channel,
        server_url,
        &state.install_id,
        std::time::Duration::from_secs(300),
        move |url: &str| {
            shared_for_fallback.lock().unwrap().linking_url_fallback = Some(url.to_string());
        },
    );

    let outcome = match result {
        Ok(token) => Ok(format!(
            "{} ({})",
            token.display_name,
            token.provider.display_name()
        )),
        Err(e) => Err(format!("Sign-in failed: {e:?}")),
    };
    shared.lock().unwrap().linking_done = Some(outcome);
}

/// Mick P0 (2026-05-30): Phase::Linking view. Shows progress/URL fallback +
/// the sticky error from a failed attempt with [Try again / Use Casual /
/// Cancel] controls. "Use Casual" demotes the run lane so the user can
/// proceed without sign-in (Casual submissions are anonymous).
fn linking_view(state: &mut BenchUiState, ui: &mut egui::Ui) {
    use crate::leaderboard::install::BenchLane;
    ui.label(
        egui::RichText::new("Signing in for Ranked submission")
            .strong()
            .size(16.0),
    );
    ui.add_space(4.0);

    let (url_fallback, status_line) = {
        let s = state.shared.lock().unwrap();
        (s.linking_url_fallback.clone(), s.status.clone())
    };

    if let Some(err) = state.linking_error.as_ref() {
        ui.colored_label(egui::Color32::from_rgb(220, 80, 80), err);
        ui.add_space(4.0);
        ui.label(
            "You can try again, switch to Casual (anonymous submission), or cancel.",
        );
    } else if let Some(url) = url_fallback {
        ui.label("Couldn't open a browser automatically. Open this URL in any browser to finish signing in:");
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.code(&url);
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(url.clone());
            }
        });
        ui.add_space(4.0);
        ui.label("Waiting for sign-in (up to 5 minutes)…");
    } else {
        ui.label(if status_line.is_empty() {
            "Opening your browser for Google sign-in… waiting for callback (up to 5 minutes).".to_string()
        } else {
            status_line
        });
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if state.linking_error.is_some() {
            if ui.button("Try again").clicked() {
                start_oauth_link(state);
            }
        }
        if ui.button("Use Casual instead").clicked() {
            state.lane = Some(BenchLane::Casual);
            persist_lane(BenchLane::Casual);
            state.linking_error = None;
            state.phase = Phase::Consent;
        }
        if ui.button("Cancel").clicked() {
            state.linking_error = None;
            state.open = false;
        }
    });
}

/// Save the chosen lane to install.json. Best-effort; failure is logged but
/// not user-surfaced (the bench still runs with the chosen lane in-memory).
fn persist_lane(lane: crate::leaderboard::install::BenchLane) {
    let channel = crate::channel::active_channel();
    if let Ok(Some(mut s)) = crate::leaderboard::install::load_for(channel) {
        if s.last_bench_lane != Some(lane) {
            s.last_bench_lane = Some(lane);
            if let Err(e) = crate::leaderboard::install::save_for(channel, &s) {
                crate::log_warn!("bench-modal: failed to persist last_bench_lane: {e}");
            }
        }
    }
}

fn consent_view(state: &mut BenchUiState, ui: &mut egui::Ui) {
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
    // Mick bug #1 fix (2026-05-30): clicking this used to fire BOTH the
    // inline preview AND the OpenSharePreview action that routed to
    // Settings>Privacy, so the settings modal popped over the bench modal.
    // The inline preview below is the canonical "show me" surface; the
    // settings detour is dropped.
    if ui.link("What exactly gets shared?").clicked() {
        state.show_share_preview = !state.show_share_preview;
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
    let (result, is_error, deep_link, throughput_gbps, ranks) = {
        let s = state.shared.lock().unwrap();
        (s.result.clone().unwrap_or_default(),
         s.is_error,
         s.deep_link.clone(),
         s.throughput_gbps,
         s.ranks.clone())
    };
    let color = if is_error { egui::Color32::from_rgb(220, 120, 90) } else { egui::Color32::from_rgb(120, 200, 140) };
    ui.label(egui::RichText::new(result).color(color));

    // Mick post-bench rank panel (Mick UX 2026-05-30): on a successful submit
    // (deep_link populated), show throughput + rank rows + a 'View on
    // leaderboard' button that opens the deep link in the system browser via
    // platform::open_url.
    if let Some(deep_link) = deep_link.as_deref() {
        ui.add_space(10.0);
        ui.separator();
        ui.add_space(6.0);
        if let Some(gbps) = throughput_gbps {
            ui.label(egui::RichText::new(format!("Throughput: {gbps:.2} GB/s")).size(14.0));
        }
        if ranks.is_empty() {
            ui.label(
                egui::RichText::new("Rank: pending (resolving asynchronously; check the leaderboard shortly)")
                    .italics(),
            );
        } else {
            for r in &ranks {
                ui.label(format!(
                    "Rank: #{} of {} in {} / {}",
                    r.rank, r.bucket_size, r.category, r.bracket
                ));
            }
        }
        ui.add_space(8.0);
        if ui.button(egui::RichText::new("View on leaderboard").strong()).clicked() {
            if let Err(e) = crate::platform::open_url(deep_link) {
                crate::log_warn!("bench-modal: failed to open leaderboard URL ({deep_link}): {e}");
            }
        }
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if ui.button("Run again").clicked() {
            state.phase = Phase::Lane;
            state.lane = crate::leaderboard::install::load()
                .ok()
                .flatten()
                .and_then(|s| s.last_bench_lane);
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
