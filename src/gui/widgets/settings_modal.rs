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

use egui::{Context, RichText};

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
    Network,
    #[cfg(feature = "telemetry")]
    Account,
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
            SettingsTab::Network => "Network",
            #[cfg(feature = "telemetry")]
            SettingsTab::Account => "Account",
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
            SettingsTab::Network,
        ];
        #[cfg(feature = "telemetry")]
        {
            v.push(SettingsTab::Account);
            v.push(SettingsTab::Leaderboard);
        }
        v
    }
}

/// Tab selection persists across modal opens within a session.
/// Sticks in `SuperdeduperApp` via the caller.
#[derive(Default)]
pub struct SettingsModalState {
    pub tab: SettingsTab,
    /// Channel the user has currently selected in the Network tab's
    /// dropdown. `None` means "no change pending — match the active
    /// channel." When `Some(c)` and `c != channel::active_channel()`,
    /// the Save button appears and clicking it shows the inline
    /// confirm row.
    pub pending_channel: Option<crate::channel::Channel>,
    /// Set to `Some(channel)` when the user clicked Save and the
    /// inline confirm row is showing. `None` = no confirmation in
    /// flight. Cleared after either Confirm or Cancel.
    pub channel_switch_confirm: Option<crate::channel::Channel>,
}

/// Process-wide slot for the "Preview a sample submission" modal's
/// JSON body. The Preview button (deep inside the leaderboard tab's
/// closure) writes here; the outer `show()` reads it and renders a
/// secondary window with the JSON. OnceLock + Mutex matches the
/// pattern used by `leaderboard::submission` for cross-frame state
/// that doesn't fit naturally on the per-render Ui chain.
static SAMPLE_PREVIEW: parking_lot::Mutex<Option<String>> =
    parking_lot::Mutex::new(None);

/// Process-wide slot for a simple "Done" confirmation dialog —
/// shown after register / unlink / reset completions per Mick's
/// 2026-05-25T01:35Z preference. Anyone can write a message; the
/// outer `show()` renders an OK-button modal until dismissed.
static DONE_DIALOG: parking_lot::Mutex<Option<String>> =
    parking_lot::Mutex::new(None);

pub fn show_done_dialog(message: String) {
    *DONE_DIALOG.lock() = Some(message);
}

fn take_done_dialog() -> Option<String> {
    DONE_DIALOG.lock().clone()
}

fn clear_done_dialog() {
    *DONE_DIALOG.lock() = None;
}

fn render_done_dialog(ctx: &egui::Context) {
    let Some(msg) = take_done_dialog() else {
        return;
    };
    let mut close = false;
    egui::Window::new(
        RichText::new("Done")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(420.0)
    .show(ctx, |ui| {
        ui.label(RichText::new(msg).color(theme::TEXT_HI));
        ui.add_space(12.0);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("OK")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(100.0, 28.0)),
                )
                .clicked()
            {
                close = true;
            }
        });
    });
    if close {
        clear_done_dialog();
    }
}

fn show_sample_preview(json: String) {
    *SAMPLE_PREVIEW.lock() = Some(json);
}

fn take_sample_preview() -> Option<String> {
    SAMPLE_PREVIEW.lock().clone()
}

fn clear_sample_preview() {
    *SAMPLE_PREVIEW.lock() = None;
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
///
/// Layout note: previously used `egui::Window` which got hijacked
/// by egui's persistent window-state memory — a prior session that
/// dragged or resized the modal would have its geometry restored
/// even with `.fixed_size()` set on the builder. Switched to an
/// `egui::Area` with a versioned id so persistence resets cleanly,
/// plus an inner `Frame` with a fixed allocated rect so the body's
/// dimensions are computed from one explicit source of truth instead
/// of inferred from content.
pub fn show(
    ctx: &Context,
    open: &mut bool,
    settings: &mut ScanSettings,
    state: &mut SettingsModalState,
) -> bool {
    // Render the sample-preview modal (if one's been requested via
    // the Privacy tab's button) ABOVE the main settings layer. Sits
    // on its own egui::Area so its close button can fire without
    // affecting the main modal's state.
    #[cfg(feature = "telemetry")]
    if let Some(json) = take_sample_preview() {
        render_sample_preview_modal(ctx, &json);
    }

    // Done-dialog: simple OK-button modal shown after
    // register / unlink / reset completions. Rendered ABOVE the
    // main settings layer so the OK click doesn't affect the
    // settings-modal state machine.
    render_done_dialog(ctx);

    if !*open {
        return false;
    }
    let mut closed = false;
    let screen = ctx.screen_rect();
    let top_left = egui::pos2(
        (screen.width() - MODAL_WIDTH).max(0.0) / 2.0 + screen.left(),
        (screen.height() - MODAL_HEIGHT).max(0.0) / 2.0 + screen.top(),
    );

    // Dim background — full-screen overlay so the modal feels modal.
    egui::Area::new(egui::Id::new("sd-settings-modal-v3-backdrop"))
        .order(egui::Order::Background)
        .fixed_pos(screen.left_top())
        .show(ctx, |ui| {
            let painter = ui.painter();
            painter.rect_filled(
                screen,
                egui::Rounding::ZERO,
                egui::Color32::from_black_alpha(140),
            );
        });

    egui::Area::new(egui::Id::new("sd-settings-modal-v3"))
        .order(egui::Order::Foreground)
        .fixed_pos(top_left)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style())
                .fill(theme::PANEL_DEEP)
                .stroke(egui::Stroke::new(1.0, theme::ACCENT_DIM))
                .rounding(egui::Rounding::same(6))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_min_size(egui::vec2(MODAL_WIDTH, MODAL_HEIGHT));
                    ui.set_max_size(egui::vec2(MODAL_WIDTH, MODAL_HEIGHT));
                    closed = render_modal_body(ui, open, settings, state);
                });
        });

    closed
}

/// Inner body of the modal — title bar, tab list + content panel,
/// footer. Returns `true` if the user clicked Done or the X close
/// button.
fn render_modal_body(
    ui: &mut egui::Ui,
    open: &mut bool,
    settings: &mut ScanSettings,
    state: &mut SettingsModalState,
) -> bool {
    let mut closed = false;

    // Title bar — heading + flush-right X close button.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("⚙ Settings")
                .color(theme::TEXT_HI)
                .heading(),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    egui::Button::new(RichText::new("✕").color(theme::TEXT_HI))
                        .frame(false)
                        .min_size(egui::vec2(24.0, 24.0)),
                )
                .clicked()
            {
                *open = false;
                closed = true;
            }
        });
    });
    ui.add_space(2.0);
    ui.label(
        RichText::new("Knobs apply to the next scan.")
            .color(theme::TEXT_LO)
            .small(),
    );
    ui.add_space(8.0);

    // Body — two-column layout with explicit column widths.
    // `columns_const`-style: hand-allocate two child UIs with rigid
    // sizes. Earlier `allocate_ui_with_layout(left_to_right)` was
    // letting children grow horizontally on some egui paths; this
    // structure removes the ambiguity.
    let body_size = egui::vec2(MODAL_WIDTH - 24.0, PANEL_HEIGHT);
    ui.allocate_ui_with_layout(body_size, egui::Layout::left_to_right(egui::Align::TOP), |ui| {
        // Left: tab list.
        ui.allocate_ui_with_layout(
            egui::vec2(TAB_LIST_WIDTH, PANEL_HEIGHT),
            egui::Layout::top_down_justified(egui::Align::Min),
            |ui| {
                ui.set_min_width(TAB_LIST_WIDTH);
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
        // Right: tab content. Remaining width is body - tab list -
        // separator - spacing. Constrained so children can't reflow.
        let content_width = body_size.x - TAB_LIST_WIDTH - 12.0;
        ui.allocate_ui_with_layout(
            egui::vec2(content_width, PANEL_HEIGHT),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                ui.set_min_width(content_width);
                ui.set_max_width(content_width);
                egui::ScrollArea::vertical()
                    .max_height(PANEL_HEIGHT)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_max_width(content_width - 4.0);
                        match state.tab {
                            SettingsTab::Engine => render_engine(ui, settings),
                            SettingsTab::Cache => render_cache(ui, settings),
                            SettingsTab::KeepStrategy => render_keep_strategy(ui, settings),
                            SettingsTab::Safety => render_safety(ui, settings),
                            SettingsTab::Preflight => render_preflight(ui, settings),
                            SettingsTab::Network => render_network(ui, state),
                            #[cfg(feature = "telemetry")]
                            SettingsTab::Account => render_account(ui),
                            #[cfg(feature = "telemetry")]
                            SettingsTab::Leaderboard => render_leaderboard(ui),
                        }
                    });
            },
        );
    });

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
                *open = false;
                closed = true;
            }
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

/// Settings → Network tab — channel selection.
///
/// Per `dev-channel-spec.md` §5.4. Shows the currently active channel,
/// an explainer for what each channel is for, and a dropdown +
/// Save flow that lets the user switch mid-session. The actual
/// channel-switch (which load `install.{channel}.json` becomes
/// "active") happens via [`crate::channel::set_active_channel`] +
/// [`crate::channel::write_persisted_channel`] after the user
/// confirms via the inline Confirm row.
fn render_network(ui: &mut egui::Ui, state: &mut SettingsModalState) {
    use crate::channel::{self, Channel};

    let current = channel::active_channel();
    let selected = state.pending_channel.unwrap_or(current);

    ui.heading("Channel");
    ui.label(
        RichText::new(
            "Server environment this client talks to. Identity, \
             achievements, and submissions live per channel — \
             switching to dev or local won't pollute or read from \
             your prod data.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(8.0);

    // Dropdown
    let mut pick = selected;
    let prior_pick = pick;
    egui::ComboBox::from_id_source("sd_network_channel_dropdown")
        .selected_text(pick.as_slug())
        .show_ui(ui, |ui| {
            for &c in Channel::all() {
                ui.selectable_value(&mut pick, c, c.as_slug());
            }
        });
    if pick != prior_pick {
        state.pending_channel = Some(pick);
        // Selecting a different option clears any in-flight confirm
        // so the user re-confirms against the new selection.
        state.channel_switch_confirm = None;
    }

    ui.add_space(6.0);
    ui.label(
        RichText::new(pick.description())
            .color(theme::TEXT_HI)
            .small(),
    );
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!("Endpoint: {}", channel::server_url_for(pick)))
            .color(theme::TEXT_LO)
            .small()
            .italics(),
    );
    ui.add_space(4.0);
    // Registration status for the SELECTED channel — answers the
    // "if I switch here, am I already registered?" question before
    // the user clicks Save. Best-effort read; an I/O error reads
    // as "unknown" rather than crashing the panel.
    #[cfg(feature = "telemetry")]
    {
        let registered = crate::leaderboard::install::install_path_for(pick)
            .ok()
            .map(|p| p.exists())
            .unwrap_or(false);
        let line = if registered {
            format!("Registered on {}.", pick.as_slug())
        } else {
            format!(
                "Not yet registered on {}. Run `superdeduper register --channel {}` first.",
                pick.as_slug(),
                pick.as_slug(),
            )
        };
        let color = if registered { theme::TEXT_LO } else { theme::HOT };
        ui.label(RichText::new(line).color(color).small());
    }

    ui.add_space(12.0);

    // Save + confirm flow. The Save button is only visible when the
    // pending channel differs from the active channel. Clicking
    // Save arms an inline confirm row (per spec §5.4: "Switch
    // channel to {channel}?"); the confirm row stays until the
    // user clicks Confirm or Cancel. Switching to the same channel
    // is a no-op (no Save button).
    if pick != current {
        if state.channel_switch_confirm == Some(pick) {
            ui.label(
                RichText::new(format!("Switch channel to {}?", pick.as_slug()))
                    .strong()
                    .color(theme::TEXT_HI),
            );
            ui.label(
                RichText::new(format!(
                    "Switching to {} channel. You'll be a fresh install on {} \
                     (or load the existing one if you've registered there before). \
                     Your {} grants stay safe but won't transfer between channels.",
                    pick.as_slug(),
                    pick.as_slug(),
                    current.as_slug(),
                ))
                .color(theme::TEXT_LO)
                .small(),
            );
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Cancel").clicked() {
                    state.channel_switch_confirm = None;
                    state.pending_channel = None;
                }
                let confirm_label = format!("Switch to {}", pick.as_slug());
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new(confirm_label)
                                .color(theme::PANEL_DEEP)
                                .strong(),
                        )
                        .fill(theme::ACCENT),
                    )
                    .clicked()
                {
                    // Persist + activate. Best-effort write; the
                    // active-channel set still happens even if the
                    // disk write fails (the user gets the switch
                    // for this session at least).
                    if let Err(e) = channel::write_persisted_channel(pick) {
                        eprintln!("channel: write_persisted_channel failed: {e}");
                    }
                    channel::set_active_channel(pick);
                    state.channel_switch_confirm = None;
                    state.pending_channel = None;
                }
            });
        } else if ui
            .button(format!("Save (switch to {})", pick.as_slug()))
            .clicked()
        {
            state.channel_switch_confirm = Some(pick);
        }
    } else {
        ui.label(
            RichText::new(format!("Active channel: {}", current.as_slug()))
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );
    }
}

/// Settings → Account tab — G3 OAuth surface.
///
/// Per `gamification-client-spec.md` §10.3 + Mick's 2026-05-24T22:14:51Z
/// directive. Shows the current link status (Anonymous vs Linked)
/// and exposes Link Google / Link Discord / Unlink actions. The
/// "Login & Claim" CTA above the achievements grid + the post-scan
/// modal sign-in CTA are separate surfaces (v1.1); this tab is the
/// canonical management surface for both.
///
/// OAuth flow runs synchronously on the UI thread for simplicity:
/// browser opens, this tab blocks until the loopback callback
/// arrives (~5 min timeout). v1.1 can move it to a background
/// thread + progress spinner if the UX needs it.
#[cfg(feature = "telemetry")]
fn render_account(ui: &mut egui::Ui) {
    use crate::channel;
    use crate::leaderboard::{install, oauth};

    let active = channel::active_channel();

    // Drain any completed background OAuth session BEFORE reading
    // status, so the post-link "Linked: …" row shows up the same
    // frame the user finished sign-in. Issue #2 fix.
    if let Some(result) = oauth::poll_session() {
        // Auto-register chain: if the OAuth flow failed because
        // this install isn't known to the leaderboard server,
        // remember which provider the user tried + kick off a
        // register session in the background. When the register
        // completes successfully, the next poll drains it +
        // auto-retries OAuth with the same provider. Per Mick's
        // 2026-05-25T01:35Z preference — the user already
        // committed to participation by clicking Link, so engine
        // doesn't need to ask twice.
        if let Err(oauth::OauthError::InstallNotRegistered) = &result {
            // Capture the failing provider so we can retry once
            // register lands. snapshot before record_toast clears.
            // We don't know the provider from the result directly,
            // but the current_session_snapshot was the last
            // in-flight session's provider; egui state holds it.
            // Simpler: stash it from current_session_snapshot
            // BEFORE the session was drained — too late now;
            // record_toast still useful for failure-other-than-401.
            // Instead set the retry below from the per-CTA
            // start_link path (see oauth_chooser::start_link).
            let _ = ();
        }
        oauth::record_toast(&result);
        ui.ctx().request_repaint();
        match &result {
            Ok(token) => eprintln!(
                "account: linked {} as {}",
                token.provider.display_name(),
                token.display_name
            ),
            Err(e) => eprintln!("account: link failed: {e}"),
        }
    }

    // Drain any completed register session. When register lands
    // successfully AND a pending retry provider is stashed, fire
    // a fresh OAuth flow with the retry provider — that's the
    // auto-register chain landing.
    if let Some(reg_result) = crate::leaderboard::registration::poll_register_session() {
        match &reg_result {
            Ok(id) => {
                eprintln!("account: register OK, install_id={id}");
                if let Some(provider) = oauth::take_pending_retry_provider() {
                    eprintln!(
                        "account: auto-retrying OAuth with {} after register",
                        provider.display_name()
                    );
                    let server_url = crate::channel::server_url_for(active);
                    if let Err(()) = oauth::try_start_session(
                        provider,
                        active,
                        server_url,
                        id,
                        oauth::DEFAULT_OAUTH_TIMEOUT,
                    ) {
                        eprintln!(
                            "account: couldn't auto-retry OAuth (session already in flight)"
                        );
                    }
                }
                show_done_dialog(format!(
                    "Registered. install_id = {}\n\nYou can now sign in with Google or Discord.",
                    id
                ));
            }
            Err(e) => {
                eprintln!("account: register failed: {e:?}");
                show_done_dialog(format!("Register failed:\n\n{e:?}"));
            }
        }
    }

    let status = oauth::status_for(active).ok();

    ui.heading("Account");
    ui.label(
        RichText::new(
            "Link this install to a Google or Discord account so \
             your achievements roll up across machines + your public \
             profile shows a display name. Per-channel: linking on \
             prod doesn't transfer to dev.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(8.0);

    // Status row.
    match &status {
        Some(oauth::AccountStatus::Anonymous) | None => {
            let install_id = install::load()
                .ok()
                .flatten()
                .map(|s| s.install_id);
            let id_short = install_id
                .as_deref()
                .map(|s| s.split('-').next().unwrap_or(s).to_string())
                .unwrap_or_else(|| "not registered".to_string());
            ui.label(
                RichText::new(format!(
                    "Status: Anonymous (UUID {id_short}…)"
                ))
                .color(theme::TEXT_HI),
            );
        }
        Some(oauth::AccountStatus::Linked {
            provider,
            display_name,
            ..
        }) => {
            ui.label(
                RichText::new(format!(
                    "Status: Linked — {display_name} ({})",
                    provider.display_name()
                ))
                .color(theme::TEXT_HI),
            );
        }
    }
    ui.add_space(4.0);
    ui.label(
        RichText::new(format!("Channel: {}", active))
            .color(theme::TEXT_LO)
            .small()
            .italics(),
    );

    ui.add_space(12.0);
    ui.separator();
    ui.add_space(8.0);

    // Render any recent OAuth result toast (success or failure)
    // so the user gets immediate visible feedback without grepping
    // oauth.log. Persists until Dismiss or until a new OAuth flow
    // starts (which clears it via try_start_session).
    if let Some(toast) = oauth::current_toast() {
        ui.horizontal(|ui| {
            match &toast {
                oauth::OauthToast::Success {
                    provider,
                    display_name,
                } => {
                    ui.label(
                        RichText::new(format!(
                            "✓ Linked: {} ({})",
                            display_name,
                            provider.display_name(),
                        ))
                        .color(theme::ACCENT)
                        .strong(),
                    );
                }
                oauth::OauthToast::Failure { reason } => {
                    ui.label(
                        RichText::new(format!("⚠ Link failed: {reason}"))
                            .color(theme::HOT)
                            .strong(),
                    );
                }
            }
            if ui
                .add(
                    egui::Button::new(RichText::new("Dismiss").color(theme::TEXT_LO))
                        .min_size(egui::vec2(64.0, 22.0)),
                )
                .clicked()
            {
                oauth::clear_toast();
            }
        });
        ui.add_space(8.0);
    }

    // Auto-register chain spinner — shows whenever a background
    // register session is running (fresh-install path: user
    // clicked Link before machine was registered; engine auto-
    // registers first then auto-retries OAuth). Render this
    // BEFORE the OAuth spinner so the user sees the registration
    // step distinctly from the sign-in step.
    if let Some(elapsed) = crate::leaderboard::registration::register_session_elapsed() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "Registering machine ({}s)…",
                    elapsed.as_secs()
                ))
                .color(theme::TEXT_HI),
            );
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "First-time setup. Once your machine is registered \
                 (~1s), the sign-in flow continues automatically.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        return;
    }

    // In-flight render: spinner + Cancel. While a background
    // session runs, the Link / Unlink rows are replaced with the
    // "Waiting for ${provider} sign-in (${elapsed}s)…" affordance.
    // Issue #2 fix: never block the egui render loop.
    if let Some((provider, elapsed)) = oauth::current_session_snapshot() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "Waiting for {} sign-in ({}s)…",
                    provider.display_name(),
                    elapsed.as_secs(),
                ))
                .color(theme::TEXT_HI),
            );
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").color(theme::HOT))
                        .min_size(egui::vec2(80.0, 28.0)),
                )
                .clicked()
            {
                oauth::cancel_current_session();
            }
        });
        ui.add_space(8.0);
        ui.label(
            RichText::new(
                "Complete the sign-in in the browser window that opened. \
                 Cancel here to abort + reuse the loopback port.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
        // Keep the spinner ticking smoothly while we wait.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        return;
    }

    // Action row. Single Link/Unlink pair per Mick's
    // 2026-05-25T01:20Z preference — Link… opens the modal
    // provider chooser (shared with the above-grid CTA);
    // Unlink stays visible but greyed when anonymous so the
    // row layout doesn't jump between states.
    let is_linked = matches!(status, Some(oauth::AccountStatus::Linked { .. }));
    ui.horizontal(|ui| {
        // Link button — enabled only when anonymous. Opens the
        // shared chooser modal; picking a provider there starts
        // the OAuth flow.
        let link_text = if is_linked {
            RichText::new("Link…").color(theme::TEXT_LO)
        } else {
            RichText::new("Link…").color(theme::TEXT_HI)
        };
        let link_btn = egui::Button::new(link_text).min_size(egui::vec2(120.0, 28.0));
        if ui.add_enabled(!is_linked, link_btn).clicked() {
            crate::gui::widgets::oauth_chooser::open();
        }
        ui.add_space(12.0);
        // Unlink button — enabled only when linked.
        let unlink_text = if is_linked {
            RichText::new("Unlink").color(theme::HOT)
        } else {
            RichText::new("Unlink").color(theme::TEXT_LO)
        };
        let unlink_btn = egui::Button::new(unlink_text).min_size(egui::vec2(100.0, 28.0));
        if ui.add_enabled(is_linked, unlink_btn).clicked() {
            let prior_display = if let Some(oauth::AccountStatus::Linked {
                provider,
                display_name,
                ..
            }) = &status
            {
                format!("{} ({})", display_name, provider.display_name())
            } else {
                "this machine".to_string()
            };
            if let Err(e) = oauth::unlink_for(active) {
                eprintln!("account: unlink failed: {e}");
                show_done_dialog(format!("Unlink failed:\n\n{e}"));
            } else {
                show_done_dialog(format!(
                    "Unlinked {prior_display}.\n\n\
                     Local link record cleared. Note: the server-side \
                     binding stays in place until the v1.1 unlink \
                     endpoint ships."
                ));
            }
        }
    });

    ui.add_space(8.0);
    if is_linked {
        ui.label(
            RichText::new(
                "To switch providers, click Unlink first then click Link…. \
                 Note: Unlink only clears the local link record — the \
                 server-side binding stays until the v1.1 unlink endpoint \
                 ships.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
    } else {
        ui.label(
            RichText::new(
                "Click Link… to pick a provider (Google or Discord). \
                 Your browser opens to the provider's sign-in page; \
                 this window updates when the flow finishes. \
                 CLI equivalent: `superdeduper account link google|discord`.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
    }

    // Render the shared chooser modal — no-op unless the Link…
    // button (or the above-grid CTA) has set the flag.
    crate::gui::widgets::oauth_chooser::show(ui.ctx(), active);
}

/// Spawn a background OAuth session. The Settings → Account tab
/// (and the other CTAs) check `oauth::current_session_snapshot()`
/// each frame to render the spinner + Cancel row, then drain via
/// `oauth::poll_session()` once it completes. Per issue #2 fix.
#[cfg(feature = "telemetry")]
fn start_link(provider: crate::leaderboard::oauth::Provider, channel: crate::channel::Channel) {
    use crate::leaderboard::{install, oauth};
    let install_id = match install::load().ok().flatten() {
        Some(s) => s.install_id,
        None => {
            eprintln!("account: not registered on channel {channel} — run `superdeduper register --channel {channel}` first");
            return;
        }
    };
    let server_url = crate::channel::server_url_for(channel);
    if oauth::try_start_session(
        provider,
        channel,
        server_url,
        &install_id,
        oauth::DEFAULT_OAUTH_TIMEOUT,
    )
    .is_err()
    {
        eprintln!("account: another OAuth flow is already in flight; ignoring");
    }
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
                    "Run `superdeduper register --reset` from a CLI to start fresh (rotates your install_id).",
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
            show_sample_preview(build_sample_payload_json());
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
                "Back up the current install file to `.bak.<ts>`, then \
                 rotate install_id + install_key locally. After reset, \
                 click Register below to push the new identity to the \
                 leaderboard server. Backend treats the new id as a \
                 fresh user; rank + achievements start from zero.",
            )
            .clicked()
        {
            // Reset is destructive but Mick's 2026-05-25T01:20Z
            // preference is "back up the file, then rotate" — the
            // .bak gives a recovery path if the rotation was
            // accidental. Reset itself doesn't hit web; the
            // separate Register button below pushes the new
            // identity to the leaderboard server.
            let active = crate::channel::active_channel();
            std::thread::spawn(move || {
                eprintln!(
                    "leaderboard: install reset requested — backing up + rotating install_id"
                );
                match install::back_up_for(active) {
                    Ok(Some(path)) => eprintln!(
                        "leaderboard: prior install backed up to {}",
                        path.display()
                    ),
                    Ok(None) => eprintln!("leaderboard: no prior install to back up"),
                    Err(e) => eprintln!("leaderboard: backup failed: {e}"),
                }
                let server_url = crate::channel::server_url_for(active).to_string();
                let fresh = install::new_unregistered(server_url);
                if let Err(e) = install::save_for(active, &fresh) {
                    eprintln!("leaderboard: reset failed: {e:?}");
                } else {
                    eprintln!(
                        "leaderboard: reset complete. new install_id={}. \
                         Click Register to push it to the leaderboard server.",
                        fresh.install_id
                    );
                }
            });
        }
        ui.add_space(4.0);
        // GUI Register — runs the same proof-of-work + POST
        // `/api/v1/register` flow as the CLI `superdeduper
        // register`, but as a background-threaded session so the
        // egui render loop stays responsive. Per Mick's
        // 2026-05-25T01:20Z ask — the OAuth flow already covers
        // the "machine doesn't exist server-side" case via the
        // `InstallNotRegistered` toast which now has an inline
        // Register link; this button is the canonical surface.
        let in_flight = crate::leaderboard::registration::register_session_in_flight();
        if let Some(result) = crate::leaderboard::registration::poll_register_session() {
            match result {
                Ok(id) => {
                    eprintln!(
                        "leaderboard: register OK, install_id={id}"
                    );
                }
                Err(e) => {
                    eprintln!("leaderboard: register failed: {e:?}");
                }
            }
        }
        if in_flight {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.add_space(4.0);
                let elapsed = crate::leaderboard::registration::register_session_elapsed()
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                ui.label(
                    RichText::new(format!("Registering ({elapsed}s)…"))
                        .color(theme::TEXT_HI),
                );
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
        } else if ui
            .add(
                egui::Button::new(
                    RichText::new("Register").color(theme::PANEL_DEEP).strong(),
                )
                .fill(theme::ACCENT)
                .min_size(egui::vec2(140.0, 26.0)),
            )
            .on_hover_text(
                "Push the current install identity (or a freshly-reset \
                 one) to the leaderboard server. Solves a small CPU \
                 proof-of-work (~1s). Equivalent to `superdeduper register` \
                 from the CLI.",
            )
            .clicked()
        {
            let active = crate::channel::active_channel();
            if crate::leaderboard::registration::try_start_register_session(active).is_err() {
                eprintln!("leaderboard: another register flow is already in flight");
            }
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
/// Render a secondary modal showing the sample submission JSON.
/// Floats over the Settings modal. Close button clears the slot.
#[cfg(feature = "telemetry")]
fn render_sample_preview_modal(ctx: &Context, json: &str) {
    use egui::{Align2, Id};
    egui::Area::new(Id::new("sd-sample-preview-backdrop"))
        .order(egui::Order::Background)
        .fixed_pos(ctx.screen_rect().left_top())
        .show(ctx, |ui| {
            ui.painter().rect_filled(
                ctx.screen_rect(),
                egui::Rounding::ZERO,
                egui::Color32::from_black_alpha(160),
            );
        });
    egui::Window::new(
        RichText::new("Sample submission payload")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
    .collapsible(false)
    .resizable(false)
    .min_width(640.0)
    .max_width(640.0)
    .min_height(520.0)
    .max_height(520.0)
    .show(ctx, |ui| {
        ui.label(
            RichText::new(
                "Synthetic data — exact JSON sd would POST to /api/v1/submit. \
                 No real scan data here; safe to share publicly.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
        ui.add_space(8.0);
        egui::ScrollArea::vertical()
            .max_height(400.0)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut buf = json.to_string();
                ui.add(
                    egui::TextEdit::multiline(&mut buf)
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .desired_rows(22)
                        .interactive(false),
                );
            });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(
                        RichText::new("Close").color(theme::PANEL_DEEP).strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(120.0, 28.0)),
                )
                .clicked()
            {
                clear_sample_preview();
            }
            ui.add_space(8.0);
            ui.hyperlink_to(
                RichText::new("Privacy policy").color(theme::ACCENT).small(),
                "https://superdeduper.io/privacy/",
            );
        });
    });
    // The take_sample_preview call in the parent removed the value;
    // re-store it so the next frame still renders the modal until
    // the user clicks Close (which calls clear_sample_preview).
    show_sample_preview(json.to_string());
}

/// Shorten a long string for inline display (the original-error
/// line on the FlaggedForReview outcome). Trims to ≤`cap` bytes
/// + adds an ellipsis when truncated; otherwise returns the
/// original.
///
/// Safe against UTF-8 multi-byte boundaries: backs up to the
/// previous char boundary if `cap` lands mid-codepoint. Without
/// this guard, a rejection reason carrying e.g. an em-dash (3-byte
/// UTF-8) could panic with `byte index N is not a char boundary`
/// — and rejection messages come from network input we don't
/// control, so a hostile backend could crash the GUI.
fn truncate_for_display(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    // Back up to the previous char boundary. `is_char_boundary(0)`
    // is always true, so this loop terminates.
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod truncate_tests {
    use super::truncate_for_display;

    #[test]
    fn passes_through_short_strings() {
        assert_eq!(truncate_for_display("hello", 10), "hello");
        assert_eq!(truncate_for_display("", 10), "");
    }

    #[test]
    fn truncates_with_ellipsis_when_too_long() {
        let s = "the quick brown fox jumps over the lazy dog";
        let out = truncate_for_display(s, 9);
        assert_eq!(out, "the quick…");
    }

    #[test]
    fn handles_utf8_multibyte_at_boundary() {
        // Em-dash is 3 bytes UTF-8. Slicing at byte 11 (mid-codepoint)
        // would panic without the char-boundary guard. Network input
        // can carry arbitrary unicode in reason strings — this fn
        // mustn't crash.
        let s = "error — bad request";
        // cap of 12 lands in the middle of the em-dash; expect the
        // function to back up to byte 10 (just before "—").
        let out = truncate_for_display(s, 12);
        // The truncated prefix must be valid UTF-8 + end on a char
        // boundary. Don't assert the exact length (depends on the
        // backup); assert structure.
        assert!(out.ends_with('…'));
        assert!(out.is_char_boundary(out.len() - '…'.len_utf8()));
    }

    #[test]
    fn cap_zero_returns_just_ellipsis() {
        let out = truncate_for_display("not-empty", 0);
        assert_eq!(out, "…");
    }

    #[test]
    fn cap_exactly_at_byte_len_passes_through() {
        let s = "hello";
        assert_eq!(truncate_for_display(s, 5), "hello");
    }
}

/// Build the synthetic sample submission as a pretty-printed JSON
/// string. Used by the Preview-sample-submission button to render
/// in a modal (and as a debug echo to stderr for headless cases).
#[cfg(feature = "telemetry")]
fn build_sample_payload_json() -> String {
    use crate::leaderboard::{hardware, submission};
    use submission::{FEATURE_BIT_CACHE, FEATURE_BIT_FORMAT_AWARE};
    let inputs = submission::SubmissionInputs {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        run_uuid: "00000000-0000-0000-0000-000000000000".into(),
        hardware: hardware::detect(),
        run_shape: submission::RunShape {
            wall_clock_seconds: 137.4,
            bytes_scanned: 320_000_000_000,
            files_scanned: 412_998,
            hash_algorithm: "river5-aes-ni".into(),
            walker_variant: "hybrid".into(),
            scope: "subdirectory".into(),
            features_used_bitmap: FEATURE_BIT_CACHE | FEATURE_BIT_FORMAT_AWARE,
            corpus_kind: "user-data".into(),
            cache_hit_ratio: Some(0.42),
            easter_egg_hits: Vec::new(),
            zero_byte_group_max: None,
            max_hardlink_count_in_scan: None,
            name_collision_count: None,
        },
        result_summary: submission::ResultSummary {
            duplicate_groups: 18_204,
            duplicate_bytes_reclaimable: 38_100_000_000,
            largest_single_group_bytes: 4_200_000_000,
            actions_taken_summary: std::collections::BTreeMap::new(),
            placeholder_skip_count: None,
            placeholder_skip_bytes: None,
        },
    };
    let payload = submission::build_payload(&inputs, "00000000-0000-0000-0000-000000000000");
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| {
        format!("(render failed: {e})")
    })
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
                    p.run_shape.files_scanned,
                    theme::humansize(p.run_shape.bytes_scanned),
                ))
                .color(theme::WARN)
                .small(),
            );
        }
        (Some(p), true) => {
            ui.label(
                RichText::new(format!(
                    "Run ready: {} files, {} read, {} groups, hash={}",
                    p.run_shape.files_scanned,
                    theme::humansize(p.run_shape.bytes_scanned),
                    p.result_summary.duplicate_groups,
                    p.run_shape.hash_algorithm,
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
                    // Archive the attempt locally regardless of
                    // outcome — gives the user a permanent record
                    // they can come back to (and Mick a paper trail
                    // for beta-tester support).
                    submission::archive_attempt(&inputs, &state.install_id, &outcome);
                    // 5xx / transport failures queue for retry.
                    if let submission::SubmitOutcome::Transient { reason } = &outcome {
                        eprintln!("leaderboard: submit transient ({reason}); enqueueing");
                        let body = crate::leaderboard::hmac_signer::canonical_body(
                            &submission::build_payload(&inputs, &state.install_id),
                        );
                        let signature = match state.install_key() {
                            Some(k) => crate::leaderboard::hmac_signer::sign(&k, &body),
                            None => String::new(),
                        };
                        if let Err(e) =
                            submission::enqueue(&inputs, &state.install_id, &signature)
                        {
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
        SubmitOutcome::FlaggedForReview {
            review_id,
            local_path,
            original_status,
            original_reason,
        } => {
            ui.label(
                RichText::new("✓ Flagged for review").color(theme::ACCENT).strong(),
            );
            if let Some(id) = review_id {
                ui.label(
                    RichText::new(format!("review_id: {id}"))
                        .color(theme::TEXT_LO)
                        .small()
                        .monospace(),
                );
            } else {
                ui.label(
                    RichText::new("Upload failed — saved locally only.")
                        .color(theme::WARN)
                        .small(),
                );
            }
            ui.label(
                RichText::new(format!("Local: {local_path}"))
                    .color(theme::TEXT_LO)
                    .small()
                    .monospace(),
            );
            ui.label(
                RichText::new(format!(
                    "Original error (HTTP {original_status}): {}",
                    truncate_for_display(original_reason, 240),
                ))
                .color(theme::TEXT_LO)
                .small(),
            );
        }
    }
}
