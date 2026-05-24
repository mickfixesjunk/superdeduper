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

/// Locked modal dimensions. `fixed_size()` alone wasn't holding
/// the window against content that wanted to grow — `min_width` +
/// `max_width` (and the height pair) clamp explicitly via the
/// underlying `Resize`. Belt-and-suspenders: also `set_max_width`
/// inside the show closure so children can't stretch past it.
const MODAL_WIDTH: f32 = 600.0;
const MODAL_HEIGHT: f32 = 500.0;
const TAB_LIST_WIDTH: f32 = 132.0;
const TAB_BUTTON_WIDTH: f32 = 124.0;
const PANEL_HEIGHT: f32 = 390.0;

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
        // Hard-clamp width AND height. Earlier .fixed_size() alone
        // didn't always hold — content wanting to grow could push
        // the modal to full window width, leaving the tab list
        // "splatted across the page." Explicit min/max on both
        // axes makes the constraint un-bypassable by inner UIs.
        .min_width(MODAL_WIDTH)
        .max_width(MODAL_WIDTH)
        .min_height(MODAL_HEIGHT)
        .max_height(MODAL_HEIGHT)
        .default_size(egui::vec2(MODAL_WIDTH, MODAL_HEIGHT))
        .show(ctx, |ui| {
            ui.set_max_width(MODAL_WIDTH - 16.0);
            ui.label(
                RichText::new("Knobs apply to the next scan.")
                    .color(theme::TEXT_LO)
                    .small(),
            );
            ui.add_space(6.0);

            // Both panels (tab list, content) share an explicit
            // height so neither can grow past the window's reserved
            // area. The content panel uses ScrollArea inside this
            // height so over-long tabs scroll instead of overflow.
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), PANEL_HEIGHT),
                egui::Layout::left_to_right(egui::Align::TOP),
                |ui| {
                    // Left: tab list — locked column so the list
                    // can never reflow to a horizontal layout.
                    ui.allocate_ui_with_layout(
                        egui::vec2(TAB_LIST_WIDTH, PANEL_HEIGHT),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_max_width(TAB_LIST_WIDTH);
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
                                    .min_size(egui::vec2(TAB_BUTTON_WIDTH, 26.0));
                                if ui.add(btn).clicked() {
                                    state.tab = tab;
                                }
                            }
                        },
                    );
                    ui.separator();
                    ui.add_space(4.0);
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
            // Two registration paths share this slot:
            // * Browser captcha (spec-default for GUI; opens the
            //   superdeduper.io setup page and captures the
            //   Turnstile token on a loopback HTTP server)
            // * PoW (CLI-style, ~1s CPU; works without a working
            //   browser — useful on headless boxes or when the
            //   captcha page isn't reachable yet)
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Register via browser")
                                .color(theme::PANEL_DEEP)
                                .strong(),
                        )
                        .fill(theme::ACCENT)
                        .min_size(egui::vec2(180.0, 28.0)),
                    )
                    .on_hover_text(
                        "Opens superdeduper.io/setup in your default browser. \
                         You solve a Cloudflare Turnstile, the page POSTs the \
                         token back to a loopback HTTP server, and we complete \
                         registration. 5-minute timeout.",
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
                            eprintln!(
                                "leaderboard: already registered ({})",
                                state.install_id
                            );
                            return;
                        }
                        eprintln!(
                            "leaderboard: opening browser captcha (install_id={})...",
                            state.install_id
                        );
                        match crate::leaderboard::registration::register_gui_via_loopback(
                            &mut state,
                        ) {
                            Ok(()) => eprintln!(
                                "leaderboard: registered via captcha. id={}",
                                state.install_id
                            ),
                            Err(e) => {
                                eprintln!("leaderboard: captcha register failed: {e:?}")
                            }
                        }
                    });
                }
                ui.add_space(6.0);
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("PoW (no browser)")
                                .color(theme::TEXT_HI),
                        )
                        .fill(theme::PANEL_DEEP)
                        .min_size(egui::vec2(150.0, 28.0)),
                    )
                    .on_hover_text(
                        "Fallback for headless / sandboxed boxes. Solves a \
                         22-bit hashcash proof-of-work (~1s CPU) and POSTs to \
                         /api/v1/register. Idempotent — clicking again when \
                         registered is a no-op.",
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
                            eprintln!(
                                "leaderboard: already registered ({})",
                                state.install_id
                            );
                            return;
                        }
                        eprintln!(
                            "leaderboard: solving PoW + POSTing /api/v1/register (install_id={})...",
                            state.install_id
                        );
                        match crate::leaderboard::registration::register_cli(&mut state) {
                            Ok(()) => eprintln!(
                                "leaderboard: registered via PoW. id={}",
                                state.install_id
                            ),
                            Err(e) => eprintln!("leaderboard: register failed: {e:?}"),
                        }
                    });
                }
            });
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
    render_submit_section(ui);

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    render_privacy_section(ui);
}

/// Settings > Privacy controls per client-spec §10.2:
/// share-frequency dropdown, "Preview a sample submission",
/// "Reset install" with confirmation. Per-field overrides are
/// deferred — engine surface for them doesn't exist yet (the
/// payload is built atomically; no opt-out points). When that's
/// scoped (G2+), they slot in here.
#[cfg(feature = "telemetry")]
fn render_privacy_section(ui: &mut egui::Ui) {
    use crate::leaderboard::install;

    ui.label(
        RichText::new("Privacy")
            .color(theme::TEXT_HI)
            .strong(),
    );
    ui.add_space(4.0);

    // Load mutable state. If the install isn't present (not
    // registered yet), the controls show a hint but stay disabled.
    let loaded = install::load();
    let state_opt: Option<install::InstallState> = match loaded {
        Ok(s) => s,
        Err(_) => None,
    };

    let current_share = state_opt
        .as_ref()
        .map(|s| s.share_default)
        .unwrap_or(install::ShareDefault::AlwaysAsk);

    ui.horizontal(|ui| {
        ui.label(RichText::new("Submit frequency").color(theme::TEXT_HI));
        let enabled = state_opt.is_some();
        let label_text = match current_share {
            install::ShareDefault::AlwaysAsk => "Always ask",
            install::ShareDefault::AutoOptIn => "Auto-submit",
            install::ShareDefault::Never => "Never",
        };
        ui.add_enabled_ui(enabled, |ui| {
            egui::ComboBox::from_id_source("sd_share_frequency")
                .selected_text(label_text)
                .show_ui(ui, |ui| {
                    let mut chosen: Option<install::ShareDefault> = None;
                    if ui
                        .selectable_label(
                            current_share == install::ShareDefault::AlwaysAsk,
                            "Always ask",
                        )
                        .on_hover_text(
                            "Pop the post-scan modal after every scan; user picks Submit / Skip.",
                        )
                        .clicked()
                    {
                        chosen = Some(install::ShareDefault::AlwaysAsk);
                    }
                    if ui
                        .selectable_label(
                            current_share == install::ShareDefault::AutoOptIn,
                            "Auto-submit",
                        )
                        .on_hover_text(
                            "Submit silently in the background after every scan. \
                             Rank + achievements still surface via toast.",
                        )
                        .clicked()
                    {
                        chosen = Some(install::ShareDefault::AutoOptIn);
                    }
                    if ui
                        .selectable_label(
                            current_share == install::ShareDefault::Never,
                            "Never",
                        )
                        .on_hover_text(
                            "Never attempt to submit; never show the post-scan modal. \
                             Engine still builds the payload locally for diagnostic \
                             logging — set to Never to fully opt out.",
                        )
                        .clicked()
                    {
                        chosen = Some(install::ShareDefault::Never);
                    }
                    if let (Some(chosen), Some(mut s)) =
                        (chosen, state_opt.clone())
                    {
                        if chosen != s.share_default {
                            s.share_default = chosen;
                            if let Err(e) = install::save(&s) {
                                eprintln!(
                                    "leaderboard: failed to persist share preference: {e:?}"
                                );
                            }
                        }
                    }
                });
        });
    });
    if state_opt.is_none() {
        ui.label(
            RichText::new("Register an install above to change this.")
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Per-field overrides (toggle individual payload fields off) — coming \
             in a follow-up slice. Today the payload is all-or-nothing.",
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
                    RichText::new("Preview a sample submission").color(theme::TEXT_HI),
                )
                .min_size(egui::vec2(220.0, 26.0)),
            )
            .on_hover_text(
                "Render a synthetic payload showing the exact JSON sd would \
                 POST. No real scan data; safe to share publicly.",
            )
            .clicked()
        {
            print_sample_payload();
        }
        ui.add_space(4.0);
        if ui
            .add(
                egui::Button::new(
                    RichText::new("Reset install").color(theme::WARN),
                )
                .min_size(egui::vec2(140.0, 26.0)),
            )
            .on_hover_text(
                "Rotate install_id + install_key. Equivalent to \
                 `sd register --reset` from the CLI. Backend treats \
                 the new id as a fresh user; rank + achievements reset.",
            )
            .clicked()
        {
            // Reset is destructive on the install identity — gate
            // behind a confirmation in the same egui frame's modal.
            // For first slice, do it inline with eprintln warning;
            // a proper confirm modal layered in a follow-up.
            std::thread::spawn(|| {
                eprintln!(
                    "leaderboard: install reset requested — rotating install_id + install_key"
                );
                let fresh = install::new_unregistered(
                    "https://api.superdeduper.io".to_string(),
                );
                if let Err(e) = install::save(&fresh) {
                    eprintln!("leaderboard: reset failed: {e:?}");
                } else {
                    eprintln!(
                        "leaderboard: reset complete. new install_id={}. \
                         Re-register via the Leaderboard tab above.",
                        fresh.install_id
                    );
                }
            });
        }
    });

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.hyperlink_to(
            RichText::new("Privacy policy").color(theme::ACCENT).small(),
            "https://superdeduper.io/privacy/",
        );
        ui.add_space(8.0);
        ui.hyperlink_to(
            RichText::new("Terms").color(theme::ACCENT).small(),
            "https://superdeduper.io/terms/",
        );
    });
}

/// Build + pretty-print a synthetic submission payload so the
/// user can see exactly what shape goes on the wire. Writes to
/// stderr for now; a follow-up slice plumbs it into the
/// "What gets shared?" modal alongside the post-scan modal's
/// real payload preview.
#[cfg(feature = "telemetry")]
fn print_sample_payload() {
    use crate::leaderboard::{hardware, submission};
    let inputs = submission::SubmissionInputs {
        run_uuid: "00000000-0000-0000-0000-000000000000".into(),
        sd_version: env!("CARGO_PKG_VERSION").to_string(),
        hardware: hardware::detect(),
        scan: submission::ScanResults {
            files_scanned: 412_998,
            bytes_scanned: 320_000_000_000,
            wall_clock_ms: 137_400,
            duplicate_groups: 18_204,
            reclaimable_inode_bytes: 38_100_000_000,
            hash_algo: "river5".into(),
            defender_rtp_state_pre: Some(false),
            defender_rtp_state_post: Some(false),
            corpus_signature_hash:
                "sha256:00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff".into(),
        },
    };
    let payload = submission::build_payload(&inputs);
    match serde_json::to_string_pretty(&payload) {
        Ok(s) => {
            eprintln!("---- sample submission payload ----\n{s}\n-----------------------------------");
        }
        Err(e) => eprintln!("leaderboard: sample payload render failed: {e:?}"),
    }
}

#[cfg(feature = "telemetry")]
fn render_submit_section(ui: &mut egui::Ui) {
    use crate::leaderboard::{install, submission};

    ui.label(
        RichText::new("Submit last completed scan")
            .color(theme::TEXT_HI)
            .strong(),
    );
    ui.add_space(4.0);

    let pending = submission::peek_pending();
    let registered = matches!(install::load(), Ok(Some(s)) if s.registered);

    match (&pending, registered) {
        (None, _) => {
            ui.label(
                RichText::new("No completed scan available — run a scan first.")
                    .color(theme::TEXT_LO)
                    .small(),
            );
        }
        (Some(p), false) => {
            ui.label(
                RichText::new(format!(
                    "Run ready ({} files, {}). Register this install above to enable submission.",
                    p.scan.files_scanned,
                    theme::humansize(p.scan.bytes_scanned),
                ))
                .color(theme::WARN)
                .small(),
            );
        }
        (Some(p), true) => {
            ui.label(
                RichText::new(format!(
                    "Run ready: {} files, {} read, {} groups, hash={}",
                    p.scan.files_scanned,
                    theme::humansize(p.scan.bytes_scanned),
                    p.scan.duplicate_groups,
                    p.scan.hash_algo,
                ))
                .color(theme::TEXT_HI)
                .small(),
            );
            ui.add_space(4.0);
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("Submit run")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(140.0, 28.0)),
                )
                .on_hover_text(
                    "POST signed payload to api.superdeduper.io/api/v1/submit. \
                     Failed submissions queue to disk and retry on next launch.",
                )
                .clicked()
            {
                std::thread::spawn(|| {
                    let state = match install::load() {
                        Ok(Some(s)) if s.registered => s,
                        _ => {
                            submission::store_last_outcome(
                                submission::SubmitOutcome::Rejected {
                                    status: 0,
                                    reason: "install not registered".into(),
                                },
                            );
                            return;
                        }
                    };
                    let inputs = match submission::take_pending() {
                        Some(i) => i,
                        None => {
                            submission::store_last_outcome(
                                submission::SubmitOutcome::Rejected {
                                    status: 0,
                                    reason: "no pending submission".into(),
                                },
                            );
                            return;
                        }
                    };
                    let outcome = submission::submit(&state, &inputs);
                    // 5xx / transport failures queue for retry.
                    if let submission::SubmitOutcome::Transient { reason } = &outcome {
                        eprintln!("leaderboard: submit transient ({reason}); enqueueing");
                        let body = crate::leaderboard::hmac_signer::canonical_body(
                            &submission::build_payload(&inputs),
                        );
                        let signature = match state.install_key() {
                            Some(k) => crate::leaderboard::hmac_signer::sign(&k, &body),
                            None => String::new(),
                        };
                        if let Err(e) = submission::enqueue(&inputs, &signature) {
                            eprintln!("leaderboard: enqueue failed: {e:?}");
                        }
                    }
                    submission::store_last_outcome(outcome);
                });
            }
        }
    }

    ui.add_space(8.0);
    if let Some(out) = submission::peek_last_outcome() {
        render_outcome(ui, &out);
    }
}

#[cfg(feature = "telemetry")]
fn render_outcome(ui: &mut egui::Ui, outcome: &crate::leaderboard::submission::SubmitOutcome) {
    use crate::leaderboard::submission::SubmitOutcome;
    match outcome {
        SubmitOutcome::Accepted {
            submission_id,
            ranks,
            achievements_unlocked,
            profile_url,
        } => {
            ui.label(
                RichText::new("Accepted")
                    .color(theme::ACCENT)
                    .strong(),
            );
            if !submission_id.is_empty() {
                ui.label(
                    RichText::new(format!("submission_id:  {submission_id}"))
                        .color(theme::TEXT_LO)
                        .small()
                        .monospace(),
                );
            }
            for r in ranks {
                ui.label(
                    RichText::new(format!(
                        "  rank #{} in {}/{} (of {})",
                        r.rank, r.category, r.bracket, r.bucket_size,
                    ))
                    .color(theme::TEXT_HI)
                    .small(),
                );
            }
            for a in achievements_unlocked {
                ui.label(
                    RichText::new(format!("  achievement unlocked: {a}"))
                        .color(theme::ACCENT)
                        .small(),
                );
            }
            if let Some(url) = profile_url {
                ui.hyperlink_to(
                    RichText::new("view profile").color(theme::ACCENT),
                    url,
                );
            }
        }
        SubmitOutcome::DuplicateNoChange => {
            ui.label(
                RichText::new("Already submitted (no change)")
                    .color(theme::TEXT_LO)
                    .small(),
            );
        }
        SubmitOutcome::Rejected { status, reason } => {
            ui.label(
                RichText::new(format!("Rejected ({status}): {reason}"))
                    .color(theme::HOT)
                    .small(),
            );
        }
        SubmitOutcome::Transient { reason } => {
            ui.label(
                RichText::new(format!(
                    "Transient failure: {reason} — queued for retry on next launch.",
                ))
                .color(theme::WARN)
                .small(),
            );
        }
    }
}
