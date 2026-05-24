//! Settings modal — exposes the engine knobs the spec defines as CLI
//! flags. Persisted via egui's `persistence` feature so the settings
//! survive app restarts.
//!
//! Layout: anchored to centre of the window, tabbed panel on the left,
//! content panel on the right. Tabs:
//!
//! * **Engine** — content hash, size filters, threads, paths
//! * **Cache** — persistent cache + per-scan banner controls
//! * **Keep Strategy** — which-file-to-keep picker per duplicate group
//! * **Safety** — paranoid verify, destructive-action confirmation,
//!   system-path permission
//! * **Pre-flight** — skip the score-card modal before scans

use egui::{Context, RichText, Window};

use crate::gui::state::ScanSettings;
use crate::gui::theme;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum SettingsTab {
    #[default]
    Engine,
    Cache,
    KeepStrategy,
    Safety,
    Preflight,
    #[cfg(feature = "telemetry")]
    Leaderboard,
}

impl SettingsTab {
    fn label(self) -> &'static str {
        match self {
            SettingsTab::Engine => "Engine",
            SettingsTab::Cache => "Cache",
            SettingsTab::KeepStrategy => "Keep strategy",
            SettingsTab::Safety => "Safety",
            SettingsTab::Preflight => "Pre-flight",
            #[cfg(feature = "telemetry")]
            SettingsTab::Leaderboard => "Leaderboard",
        }
    }
    fn all() -> Vec<SettingsTab> {
        let mut v = vec![
            SettingsTab::Engine,
            SettingsTab::Cache,
            SettingsTab::KeepStrategy,
            SettingsTab::Safety,
            SettingsTab::Preflight,
        ];
        #[cfg(feature = "telemetry")]
        v.push(SettingsTab::Leaderboard);
        v
    }
}

/// Tab selection persists across modal opens within a session.
/// Sticks in `SuperdeduperApp` via the caller.
#[derive(Default)]
pub struct SettingsModalState {
    pub tab: SettingsTab,
}

/// Returns `true` if the user clicked Close / Done this frame.
pub fn show(
    ctx: &Context,
    open: &mut bool,
    settings: &mut ScanSettings,
    state: &mut SettingsModalState,
) -> bool {
    let mut closed = false;
    Window::new(RichText::new("⚙ Settings").color(theme::TEXT_HI).heading())
        .open(open)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        // Locked size so the Done / Reset footer is always visible.
        // Previously the modal expanded freely to fit content and
        // pushed the footer below the screen on tabs with lots of
        // controls (Engine, Keep strategy).
        .fixed_size(egui::vec2(640.0, 520.0))
        .show(ctx, |ui| {
            ui.label(
                RichText::new("Knobs apply to the next scan.")
                    .color(theme::TEXT_LO)
                    .small(),
            );
            ui.add_space(8.0);

            // Both panels (tab list, content) share an explicit
            // height so neither can grow past the window's reserved
            // area. The content panel uses ScrollArea inside this
            // height so over-long tabs scroll instead of overflow.
            const PANEL_HEIGHT: f32 = 400.0;
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), PANEL_HEIGHT),
                egui::Layout::left_to_right(egui::Align::TOP),
                |ui| {
                    // Left: tab list.
                    ui.allocate_ui_with_layout(
                        egui::vec2(140.0, PANEL_HEIGHT),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            for tab in SettingsTab::all() {
                                let selected = state.tab == tab;
                                let label = if selected {
                                    RichText::new(tab.label())
                                        .color(theme::ACCENT)
                                        .strong()
                                        .size(14.0)
                                } else {
                                    RichText::new(tab.label())
                                        .color(theme::TEXT_HI)
                                        .size(14.0)
                                };
                                let btn = egui::Button::new(label)
                                    .frame(false)
                                    .fill(if selected {
                                        theme::ACCENT_DIM
                                    } else {
                                        egui::Color32::TRANSPARENT
                                    })
                                    .min_size(egui::vec2(130.0, 28.0));
                                if ui.add(btn).clicked() {
                                    state.tab = tab;
                                }
                            }
                        },
                    );
                    ui.separator();
                    ui.add_space(6.0);
                    // Right: tab content. ScrollArea constrained so
                    // over-long tabs scroll inside the panel; the
                    // panel never pushes the footer off-screen.
                    egui::ScrollArea::vertical()
                        .max_height(PANEL_HEIGHT)
                        .auto_shrink([false, false])
                        .show(ui, |ui| match state.tab {
                            SettingsTab::Engine => render_engine(ui, settings),
                            SettingsTab::Cache => render_cache(ui, settings),
                            SettingsTab::KeepStrategy => render_keep_strategy(ui, settings),
                            SettingsTab::Safety => render_safety(ui, settings),
                            SettingsTab::Preflight => render_preflight(ui, settings),
                            #[cfg(feature = "telemetry")]
                            SettingsTab::Leaderboard => render_leaderboard(ui),
                        });
                },
            );

            ui.add_space(10.0);
            ui.separator();
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Reset to defaults").clicked() {
                    *settings = ScanSettings::default();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                RichText::new("Done").color(theme::PANEL_DEEP).strong(),
                            )
                            .fill(theme::ACCENT)
                            .min_size(egui::vec2(100.0, 28.0)),
                        )
                        .clicked()
                    {
                        closed = true;
                    }
                });
            });
        });
    closed
}

// ============================================================
// Tab content renderers — one per SettingsTab variant.
// ============================================================

fn render_engine(ui: &mut egui::Ui, settings: &mut ScanSettings) {
    ui.heading("Content hash");
    const RIVER5_TOOLTIP: &str = "RIVER5 — 16-byte AES-NI-accelerated content hash.\n\n\
         What you get:\n\
         • ~3× faster than BLAKE3 on bulk content on any CPU \
           with AES-NI (Intel Westmere+ / AMD Bulldozer+ — \
           roughly anything from 2010 onwards).\n\
         • 128-bit output — collision probability negligible \
           for any realistic file count (you'd need 2^64 ≈ \
           18 quintillion files to expect one accidental \
           collision).\n\
         • Same identity guarantees as BLAKE3 on non-adversarial \
           input: two files with the same content always produce \
           the same digest, two files with different content \
           essentially never collide.\n\n\
         What it is NOT:\n\
         • Cryptographic. RIVER5 is built for speed against \
           real-world dedup workloads, NOT for resisting a \
           malicious adversary deliberately crafting collisions. \
           If your dedup target is untrusted user-uploaded \
           content where someone has motive to fool the hash, \
           use BLAKE3.\n\n\
         The cache keys on the algo so flipping this dropdown \
         doesn't pull stale hashes from a prior scan.";

    const BLAKE3_TOOLTIP: &str = "BLAKE3 — 32-byte cryptographic hash.\n\n\
         Cryptographically secure (256-bit collision resistance, \
         the post-SHA-3 standard). Strictly slower than RIVER5 \
         on bulk content but the difference is only meaningful \
         if your scan is hash-bound — most superdeduper scans \
         are open()-bound and the algo barely matters.\n\n\
         Pick BLAKE3 when you need to defend against an \
         adversary trying to craft hash collisions. Otherwise \
         RIVER5 is faster and just as accurate.";

    ui.horizontal(|ui| {
        ui.label("Algorithm:");
        let mut algo = settings.hash_algo;
        let combo = egui::ComboBox::from_id_source("hash-algo")
            .selected_text(match algo {
                crate::pipeline::hash::HashAlgo::Blake3 => "BLAKE3 (32-byte, cryptographic)",
                crate::pipeline::hash::HashAlgo::River5 => "RIVER5 (16-byte, AES-NI, default)",
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut algo,
                    crate::pipeline::hash::HashAlgo::River5,
                    "RIVER5 (16-byte, AES-NI, default)",
                )
                .on_hover_text(RIVER5_TOOLTIP);
                ui.selectable_value(
                    &mut algo,
                    crate::pipeline::hash::HashAlgo::Blake3,
                    "BLAKE3 (32-byte, cryptographic)",
                )
                .on_hover_text(BLAKE3_TOOLTIP);
            });
        let tip = match algo {
            crate::pipeline::hash::HashAlgo::Blake3 => BLAKE3_TOOLTIP,
            crate::pipeline::hash::HashAlgo::River5 => RIVER5_TOOLTIP,
        };
        combo.response.on_hover_text(tip);
        if algo != settings.hash_algo {
            settings.hash_algo = algo;
        }
    });
    ui.label(
        RichText::new(
            "Default: RIVER5. Switch to BLAKE3 only if you need \
             cryptographic-strength collision resistance.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(8.0);
    ui.checkbox(
        &mut settings.use_format_aware,
        "Tier 0 format-aware fingerprints",
    )
    .on_hover_text(
        "ZIP / DOCX / XLSX / EPUB get fingerprinted via their central-\
         directory tuples before the byte-content tiers fire. Cheap \
         pre-filter that drops obvious non-matches.",
    );
    ui.add_space(12.0);

    ui.heading("Size filters");
    ui.horizontal(|ui| {
        ui.label("Min size:");
        let mut min = settings.min_size_bytes as f64;
        if ui
            .add(
                egui::DragValue::new(&mut min)
                    .speed(1024.0)
                    .range(0.0..=1.0e15)
                    .custom_formatter(|n, _| theme::humansize(n as u64)),
            )
            .changed()
        {
            settings.min_size_bytes = min as u64;
        }
    });
    ui.horizontal(|ui| {
        let mut has_max = settings.max_size_bytes.is_some();
        if ui.checkbox(&mut has_max, "Cap max size").changed() {
            settings.max_size_bytes = if has_max {
                Some(settings.max_size_bytes.unwrap_or(1024 * 1024 * 1024))
            } else {
                None
            };
        }
        if let Some(max) = settings.max_size_bytes.as_mut() {
            let mut v = *max as f64;
            if ui
                .add(
                    egui::DragValue::new(&mut v)
                        .speed(1_048_576.0)
                        .range(0.0..=1.0e15)
                        .custom_formatter(|n, _| theme::humansize(n as u64)),
                )
                .changed()
            {
                *max = v as u64;
            }
        }
    });
    ui.add_space(12.0);

    ui.heading("Path filters");
    ui.horizontal(|ui| {
        ui.label("Include glob:");
        ui.text_edit_singleline(&mut settings.include_glob);
    });
    ui.horizontal(|ui| {
        ui.label("Exclude glob:");
        ui.text_edit_singleline(&mut settings.exclude_glob);
    });
    ui.label(
        RichText::new("Standard globs. Empty = no filter.")
            .color(theme::TEXT_LO)
            .small(),
    );
    ui.add_space(12.0);

    ui.heading("Threads");
    ui.horizontal(|ui| {
        ui.label("Worker threads:");
        let mut has_explicit = settings.threads.is_some();
        if ui.checkbox(&mut has_explicit, "explicit").changed() {
            settings.threads = if has_explicit { Some(num_cpus()) } else { None };
        }
        if let Some(t) = settings.threads.as_mut() {
            let mut v = *t as i32;
            if ui.add(egui::DragValue::new(&mut v).range(1..=256)).changed() {
                *t = v as usize;
            }
        } else {
            ui.label(
                RichText::new(format!("auto ({})", num_cpus()))
                    .color(theme::TEXT_LO)
                    .small(),
            );
        }
    });
    ui.checkbox(
        &mut settings.follow_links,
        "Follow reparse points / symlinks",
    )
    .on_hover_text(
        "Default OFF — symlinks are skipped. ON: re-stat through \
         the link to its target. No loop / cycle detection; a \
         circular symlink structure would recurse indefinitely.",
    );
    ui.checkbox(
        &mut settings.allow_system_paths,
        "Permit scanning system paths (C:\\Windows etc.)",
    );
}

fn render_cache(ui: &mut egui::Ui, settings: &mut ScanSettings) {
    ui.heading("Persistent cache");
    ui.checkbox(
        &mut settings.use_cache,
        "Use cache (USN delta + last hashes)",
    )
    .on_hover_text(
        "When ON, superdeduper persists per-file hashes keyed on \
         (volume, USN, path, mtime) so subsequent scans of the same \
         corpus skip already-hashed files. Huge win on re-scans.",
    );
    ui.indent("cache-sub", |ui| {
        ui.add_enabled_ui(settings.use_cache, |ui| {
            ui.checkbox(
                &mut settings.always_use_cache,
                "…and always use it when available (no per-scan prompt)",
            )
            .on_hover_text(
                "When ON, superdeduper silently uses the cached scan if one \
                 is found for the current scan roots' volume. When OFF \
                 (default), a banner appears above the scan controls so you \
                 can opt out of cache reuse per scan.",
            );
        });
    });
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "The cache is at %LOCALAPPDATA%\\superdeduper\\cache.sqlite. \
             Bumping the hash-algorithm in Settings → Engine doesn't \
             pull stale hashes; the cache stores the algo per row.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
}

fn render_keep_strategy(ui: &mut egui::Ui, settings: &mut ScanSettings) {
    use crate::cli::KeepStrategy;
    ui.heading("Which file is the keeper?");
    ui.label(
        RichText::new(
            "When a duplicate group is confirmed, the engine picks one \
             file to protect and treats the rest as dupes. This setting \
             chooses how that pick is made.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(6.0);

    let choices: &[(KeepStrategy, &str, &str)] = &[
        (
            KeepStrategy::Smart,
            "Smart (default) — heuristic scoring",
            "Scores each file on path quality (Recycle Bin / temp / \
             cache penalised, depth rewarded), filename patterns \
             (_final rewarded, _draft / 'Copy of' / (1) penalised), \
             and mtime. Highest-scored wins. Reasoning shown in the \
             keep-tag tooltip in the results table.",
        ),
        (
            KeepStrategy::Newest,
            "Newest — keep most recently modified",
            "Pure mtime — the file with the most recent modification \
             time becomes the keeper. Good for workflows where the \
             active copy is always the latest.",
        ),
        (
            KeepStrategy::Oldest,
            "Oldest — keep earliest modified",
            "Pure mtime — the file with the earliest modification \
             time becomes the keeper. Good when copies are derived \
             work and the original is the canonical source.",
        ),
        (
            KeepStrategy::ShortestPath,
            "Shortest path — closer to volume root",
            "Picks the file whose path string is shortest. Tends to \
             favour the top-level / organised copy over deep nested \
             duplicates.",
        ),
        (
            KeepStrategy::LongestPath,
            "Longest path — deepest in tree",
            "Picks the file whose path string is longest. Useful \
             when archive / dated folders nest copies deeper than \
             the active working copy.",
        ),
        (
            KeepStrategy::First,
            "First — preserve scan order",
            "No reorder — whichever file the walker found first in \
             each group stays the keeper. Deterministic but \
             arbitrary on Windows where directory order varies.",
        ),
    ];

    let active = settings.keep_strategy;
    for (variant, label, detail) in choices {
        let resp = ui
            .radio_value(&mut settings.keep_strategy, *variant, *label)
            .on_hover_text(*detail);
        // Indent the detail underneath the active row so the user
        // can see the reasoning without hovering.
        if *variant == active {
            ui.indent("keep-active-detail", |ui| {
                ui.label(
                    RichText::new(*detail)
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                );
            });
        }
        let _ = resp;
    }
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "You can always override the picked keeper per group via \
             the 👑 button in the results table.",
        )
        .color(theme::TEXT_LO)
        .small()
        .italics(),
    );
}

fn render_safety(ui: &mut egui::Ui, settings: &mut ScanSettings) {
    ui.heading("Verification");
    ui.checkbox(
        &mut settings.paranoid,
        "Paranoid byte-by-byte confirm before reporting",
    )
    .on_hover_text(
        "Hash collisions are astronomically unlikely, but if ON, \
         every confirmed duplicate group does a final byte-by-byte \
         compare before being reported. Doubles I/O for the dupe \
         set but eliminates the residual collision risk entirely.",
    );
    ui.add_space(12.0);

    ui.heading("Destructive actions");
    let bypass_on = settings.bypass_destructive_confirmation;
    let bypass_check = ui.checkbox(
        &mut settings.bypass_destructive_confirmation,
        RichText::new("Bypass \"type DELETE\" confirmation").color(if bypass_on {
            theme::HOT
        } else {
            theme::TEXT_HI
        }),
    );
    bypass_check.on_hover_text(
        "OFF (default): every Recycle / Hardlink / Safe-rename action shows a \
         modal asking you to type \"DELETE\" before it fires.\n\n\
         ON: actions fire immediately on click — no prompt. Use only when you \
         trust the dedup picks (eg. running Smart-keep against the same corpus \
         repeatedly and reviewing results before clicking each action).\n\n\
         Reveal-in-Explorer and Unsuperdeduper never prompt regardless of this \
         setting — Reveal touches nothing, and Unsuperdeduper is a reversal.",
    );
    if settings.bypass_destructive_confirmation {
        ui.label(
            RichText::new("⚠ Destructive actions will fire WITHOUT confirmation.")
                .color(theme::HOT)
                .small()
                .italics(),
        );
    }
    ui.add_space(12.0);

    ui.heading("Startup");
    ui.checkbox(
        &mut settings.dismissed_alpha_warning,
        "Don't show the alpha-software warning on launch",
    )
    .on_hover_text(
        "Persists the \"I've seen the alpha-warning\" acknowledgement \
         so it doesn't appear on next launch. The warning still \
         appears once per fresh install.",
    );
}

fn render_preflight(ui: &mut egui::Ui, settings: &mut ScanSettings) {
    ui.heading("Pre-flight modal");
    ui.checkbox(
        &mut settings.skip_preflight,
        "Skip pre-flight modal before each scan",
    )
    .on_hover_text(
        "When ON, scans start immediately without the score-card modal. \
         You can still trigger Diagnose manually from the CLI to see \
         your machine's profile.",
    );
    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Pre-flight measures hash compute (one machine-wide \
             measurement) plus Tier 1 + Tier 3 disk throughput \
             for every drive in the scan. Usually 5–15 seconds per \
             drive. Resumed scans always skip pre-flight regardless \
             of this setting — you've already decided to continue.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(feature = "telemetry")]
fn render_leaderboard(ui: &mut egui::Ui) {
    use crate::leaderboard::install;

    ui.heading("Leaderboard participation");
    ui.label(
        RichText::new(
            "Opt-in to submit anonymous run stats to superdeduper.io. \
             Hardware bracket and dup throughput are visible on a public \
             leaderboard; identities default to a UUID until you link a \
             Google or Discord account at G3.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(8.0);

    let path_str = install::install_path()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(install path unavailable)".to_string());

    match install::load() {
        Ok(Some(state)) => {
            ui.group(|ui| {
                ui.label(
                    RichText::new("Install state")
                        .color(theme::TEXT_HI)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("install_id:  {}", state.install_id))
                        .color(theme::TEXT_LO)
                        .monospace()
                        .small(),
                );
                ui.label(
                    RichText::new(format!(
                        "registered:  {}",
                        if state.registered { "yes" } else { "no" }
                    ))
                    .color(if state.registered {
                        theme::ACCENT
                    } else {
                        theme::WARN
                    })
                    .small(),
                );
                ui.label(
                    RichText::new(format!("server_url:  {}", state.server_url))
                        .color(theme::TEXT_LO)
                        .small(),
                );
                ui.label(
                    RichText::new(format!("share_default:  {:?}", state.share_default))
                        .color(theme::TEXT_LO)
                        .small(),
                );
            });
        }
        Ok(None) => {
            ui.label(
                RichText::new(
                    "Not registered. Click below to enrol — uses a small CPU proof-of-work (~1 second), \
                     no network round-trip beyond a single POST to api.superdeduper.io.",
                )
                .color(theme::TEXT_HI),
            );
            ui.add_space(8.0);
            // Note: button clicks fire on the UI thread. Registration
            // is ~1s of CPU (PoW) + a 15s timeout HTTP POST; we spawn
            // off-thread so the modal stays responsive. Failures /
            // success are written back via a status string stored in
            // a thread-shared OnceLock (next slice) — for now we
            // print to stdout and ask the user to retry / check log.
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("Register this install")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(180.0, 28.0)),
                )
                .on_hover_text(
                    "Generates a UUID + HMAC key, solves a 22-bit hashcash proof-of-work, \
                     then POSTs to api.superdeduper.io/api/v1/register. \
                     Idempotent — clicking again when registered is a no-op.",
                )
                .clicked()
            {
                std::thread::spawn(|| {
                    let mut state = match install::load() {
                        Ok(Some(s)) => s,
                        _ => install::new_unregistered(
                            "https://api.superdeduper.io".to_string(),
                        ),
                    };
                    if state.registered {
                        eprintln!("leaderboard: already registered ({})", state.install_id);
                        return;
                    }
                    eprintln!(
                        "leaderboard: solving PoW + POSTing /api/v1/register (install_id={})...",
                        state.install_id
                    );
                    match crate::leaderboard::registration::register_cli(&mut state) {
                        Ok(()) => {
                            eprintln!("leaderboard: registered. id={}", state.install_id);
                        }
                        Err(e) => {
                            eprintln!("leaderboard: register failed: {e:?}");
                        }
                    }
                });
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Result goes to stderr while a future slice wires inline status display \
                     + auto-refresh of this tab after the thread completes.",
                )
                .color(theme::TEXT_LO)
                .small()
                .italics(),
            );
        }
        Err(e) => {
            ui.label(
                RichText::new(format!(
                    "install.json failed to load: {e}. Either corrupted or written by a newer client.",
                ))
                .color(theme::HOT),
            );
            ui.label(
                RichText::new(
                    "Run `sd register --reset` from a CLI to start fresh (rotates your install_id).",
                )
                .color(theme::TEXT_LO)
                .small(),
            );
        }
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(format!("install.json:  {}", path_str))
            .color(theme::TEXT_LO)
            .small()
            .monospace(),
    );
    ui.add_space(8.0);

    ui.separator();
    ui.add_space(6.0);
    ui.label(
        RichText::new("Submit button (G1 next slice)")
            .color(theme::TEXT_HI)
            .strong(),
    );
    ui.label(
        RichText::new(
            "After each completed scan the engine will surface a 'Submit run' \
             button in the post-scan view (greyed until registered + opt-in). \
             Failed submissions queue to disk and retry on the next launch.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
}
