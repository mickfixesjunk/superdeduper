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
//! * **Safety** — destructive-action confirmation, system-path
//!   permission
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
    /// #81 — Exclusion preset packs + custom rules. Master toggle,
    /// 8 pack-row checkboxes, custom-extension / pattern textareas.
    Exclusions,
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
            SettingsTab::Exclusions => "Exclusions",
            SettingsTab::Network => "Network",
            #[cfg(feature = "telemetry")]
            SettingsTab::Account => "Account",
            #[cfg(feature = "telemetry")]
            SettingsTab::Leaderboard => "Leaderboard",
        }
    }
    fn all() -> Vec<SettingsTab> {
        #[allow(unused_mut)]
        let mut v = vec![
            SettingsTab::Engine,
            SettingsTab::Cache,
            SettingsTab::KeepStrategy,
            SettingsTab::Safety,
            SettingsTab::Preflight,
            SettingsTab::Exclusions,
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
#[cfg(feature = "telemetry")]
static SAMPLE_PREVIEW: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// Process-wide slot for a simple "Done" confirmation dialog —
/// shown after register / unlink / reset completions per Mick's
/// 2026-05-25T01:35Z preference. Anyone can write a message; the
/// outer `show()` renders an OK-button modal until dismissed.
static DONE_DIALOG: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

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
    egui::Window::new(RichText::new("Done").color(theme::TEXT_HI).heading())
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
                        egui::Button::new(RichText::new("OK").color(theme::PANEL_DEEP).strong())
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

/// Process-wide flag for the "Reset install" confirmation modal
/// (client-spec §10.2: Reset is a destructive, identity-rotating
/// action and MUST go through a confirmation modal, not fire on a
/// single click). The Privacy-tab button sets this; the outer
/// `show()` renders the confirm dialog and only on [Reset install]
/// does the rotate actually run.
#[cfg(feature = "telemetry")]
static RESET_CONFIRM: parking_lot::Mutex<bool> = parking_lot::Mutex::new(false);

#[cfg(feature = "telemetry")]
fn request_reset_confirm() {
    *RESET_CONFIRM.lock() = true;
}

/// Run the destructive install reset on a background thread: back up
/// the current install file, rotate to a fresh unregistered identity,
/// then surface a Done dialog. Reset itself never hits the network —
/// the separate Register button pushes the new identity. Extracted so
/// both the (now gated) confirm modal and tests share one path.
#[cfg(feature = "telemetry")]
fn perform_install_reset(active: crate::channel::Channel) {
    use crate::leaderboard::install;
    std::thread::spawn(move || {
        eprintln!("leaderboard: install reset confirmed — backing up + rotating install_id");
        match install::back_up_for(active) {
            Ok(Some(path)) => {
                eprintln!("leaderboard: prior install backed up to {}", path.display())
            }
            Ok(None) => eprintln!("leaderboard: no prior install to back up"),
            Err(e) => eprintln!("leaderboard: backup failed: {e}"),
        }
        // resolve_server_url so a reset under a mock/override re-creates
        // the install pointed at the same endpoint (consistent with G1).
        let server_url = crate::channel::resolve_server_url(active);
        let fresh = install::new_unregistered(server_url);
        match install::save_for(active, &fresh) {
            Ok(()) => {
                eprintln!(
                    "leaderboard: reset complete. new install_id={}. \
                     Click Register to push it to the leaderboard server.",
                    fresh.install_id
                );
                show_done_dialog(format!(
                    "Install reset. A backup of your previous identity was saved.\n\n\
                     New install_id: {}\n\n\
                     Click Register to push this identity to the leaderboard \
                     (rank + achievements start fresh).",
                    fresh.install_id
                ));
            }
            Err(e) => {
                eprintln!("leaderboard: reset failed: {e:?}");
                show_done_dialog(format!("Install reset FAILED: {e}"));
            }
        }
    });
}

/// Render the Reset-install confirmation modal when armed. [Reset
/// install] runs the rotate; [Cancel] disarms. No-op when not armed.
#[cfg(feature = "telemetry")]
fn render_reset_confirm(ctx: &egui::Context) {
    if !*RESET_CONFIRM.lock() {
        return;
    }
    let mut decision: Option<bool> = None; // Some(true)=reset, Some(false)=cancel
    egui::Window::new(RichText::new("Reset install?").color(theme::WARN).heading())
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .default_width(440.0)
        .show(ctx, |ui| {
            ui.label(RichText::new(
                "This rotates your install_id + install_key to a brand-new \
                 identity. The leaderboard treats it as a fresh user: your \
                 rank and achievements start from zero.",
            ).color(theme::TEXT_HI));
            ui.add_space(6.0);
            ui.label(RichText::new(
                "Your current identity is backed up to a .bak file first, so \
                 the rotation is recoverable. Reset does not contact the \
                 server — click Register afterward to push the new identity.",
            ).color(theme::TEXT_LO).small());
            ui.add_space(12.0);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            RichText::new("Reset install").color(theme::PANEL_DEEP).strong(),
                        )
                        .fill(theme::WARN)
                        .min_size(egui::vec2(140.0, 28.0)),
                    )
                    .clicked()
                {
                    decision = Some(true);
                }
                ui.add_space(8.0);
                if ui
                    .add(
                        egui::Button::new(RichText::new("Cancel").color(theme::TEXT_HI))
                            .min_size(egui::vec2(100.0, 28.0)),
                    )
                    .clicked()
                {
                    decision = Some(false);
                }
            });
        });
    match decision {
        Some(true) => {
            *RESET_CONFIRM.lock() = false;
            perform_install_reset(crate::channel::active_channel());
        }
        Some(false) => *RESET_CONFIRM.lock() = false,
        None => {}
    }
}

#[cfg(feature = "telemetry")]
fn show_sample_preview(json: String) {
    *SAMPLE_PREVIEW.lock() = Some(json);
}

#[cfg(feature = "telemetry")]
fn take_sample_preview() -> Option<String> {
    SAMPLE_PREVIEW.lock().clone()
}

#[cfg(feature = "telemetry")]
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

    // Reset-install confirmation modal (§10.2): destructive identity
    // rotation is gated behind an explicit confirm, rendered above the
    // settings layer like the other secondary modals.
    #[cfg(feature = "telemetry")]
    render_reset_confirm(ctx);

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
                egui::CornerRadius::ZERO,
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
                .corner_radius(egui::CornerRadius::same(6))
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
        ui.label(RichText::new("⚙ Settings").color(theme::TEXT_HI).heading());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(
                    // #143 — `×` (U+00D7 MULTIPLICATION SIGN) is in
                    // every Latin font egui bundles by default;
                    // the previous `✕` (U+2715 HEAVY MULTIPLICATION
                    // X, Dingbats block) was tofu-ing on Mick's
                    // Windows install because the Dingbats block
                    // isn't in Ubuntu-Light + isn't reliably in
                    // NotoEmoji's bitmap-emoji subset either.
                    egui::Button::new(RichText::new("×").color(theme::TEXT_HI))
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
    ui.allocate_ui_with_layout(
        body_size,
        egui::Layout::left_to_right(egui::Align::TOP),
        |ui| {
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
                            RichText::new(tab.label()).color(theme::TEXT_HI).size(14.0)
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
                                SettingsTab::Exclusions => render_exclusions(ui, settings),
                                SettingsTab::Network => render_network(ui, state),
                                #[cfg(feature = "telemetry")]
                                SettingsTab::Account => render_account(ui),
                                #[cfg(feature = "telemetry")]
                                SettingsTab::Leaderboard => render_leaderboard(ui),
                            }
                        });
                },
            );
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
                    egui::Button::new(RichText::new("Done").color(theme::PANEL_DEEP).strong())
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
        let combo = egui::ComboBox::from_id_salt("hash-algo")
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
            if ui
                .add(egui::DragValue::new(&mut v).range(1..=256))
                .changed()
            {
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
        (
            KeepStrategy::InReference,
            "In-reference — favour the copy under a reference root",
            "Reference roots (mark them in the Roots panel via the \
             Ref checkbox) are NEVER destructive-targeted regardless \
             of strategy — that protection runs at action time. This \
             strategy is the keeper-pick layer on top: when a group \
             contains both reference and non-reference files, the \
             reference-root file is the keeper. For groups where no \
             member is under a reference root, the Smart picker fires \
             as a fallback so you still get a sensible pick instead \
             of an error. Pair this with reference-root marking when \
             you have a curated canonical tree (photo archive, \
             gold-master backup) and want everything outside it \
             treated as a dupe.",
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
    // #131 — "Paranoid byte-by-byte confirm" checkbox removed:
    // the underlying pipeline stage was a no-op stub that never
    // verified anything. Real byte-by-byte verification is a
    // v0.3.x feature scope; when it lands it gets a fresh name +
    // hover-text that matches what the code actually does.

    ui.heading("Destructive actions");
    let bypass_on = settings.bypass_destructive_confirmation;
    let bypass_check = ui.checkbox(
        &mut settings.bypass_destructive_confirmation,
        RichText::new("Bypass action-confirmation prompts").color(if bypass_on {
            theme::HOT
        } else {
            theme::TEXT_HI
        }),
    );
    bypass_check.on_hover_text(
        "OFF (default): every destructive action shows a modal asking you to \
         type the matching verb before it fires (DELETE for Recycle / Nuke, \
         RENAME for Safe-rename, ARCHIVE for Archive (Move), HARDLINK for \
         Hardlink).\n\n\
         ON: actions fire immediately on click — no prompt. Use only when \
         you trust the dedup picks (eg. running Smart-keep against the same \
         corpus repeatedly and reviewing results before clicking each \
         action).\n\n\
         Reveal-in-Explorer, Unsuperdeduper, and Archive (Copy) never prompt \
         regardless of this setting — Reveal touches nothing, Unsuperdeduper \
         is a reversal, and Archive (Copy) doesn't touch the source files.",
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
    ui.add_space(12.0);

    // #41 — Scan history retention. Lives in Safety tab rather than
    // Privacy (which is telemetry-gated) so it's reachable on
    // telemetry-off builds too — scan_history is a local-storage
    // concept independent of leaderboard submission.
    ui.heading("Scan history retention");
    ui.label(
        RichText::new(
            "How long to keep local scan-history rows before \
             auto-pruning on app start. Forever (default) matches v1 \
             behaviour — nothing is auto-deleted. The History tab's \
             Delete button is always available regardless of this \
             setting.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Keep history for:").color(theme::TEXT_HI));
        let label = match settings.history_retention_days {
            0 => "Forever".to_string(),
            30 => "30 days".to_string(),
            90 => "90 days".to_string(),
            365 => "1 year".to_string(),
            n => format!("{n} days (custom)"),
        };
        egui::ComboBox::from_id_salt("history-retention-days")
            .selected_text(label)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut settings.history_retention_days, 0, "Forever");
                ui.selectable_value(&mut settings.history_retention_days, 30, "30 days");
                ui.selectable_value(&mut settings.history_retention_days, 90, "90 days");
                ui.selectable_value(&mut settings.history_retention_days, 365, "1 year");
            });
    });
    if settings.history_retention_days > 0 {
        ui.label(
            RichText::new(format!(
                "Rows older than {} days will be removed on next app launch.",
                settings.history_retention_days
            ))
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
    }
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

/// #81 — Settings → Exclusions tab.
///
/// Master toggle + 8 preset-pack rows + custom-extension textarea +
/// custom-pattern textarea + Restore-safe-defaults button. All
/// mutations land back on `settings.exclusion_config`; the scan
/// launch path recompiles the policy each run so changes take
/// effect on the next scan without an app restart.
fn render_exclusions(ui: &mut egui::Ui, settings: &mut ScanSettings) {
    use crate::exclusions::{presets::BuiltinPresets, PresetPackId, PresetSource};
    ui.heading("Exclusions");
    ui.label(
        RichText::new(
            "Skip files that are dangerous or pointless to dedupe — system libraries, \
             OS-protected paths, .git internals, AV signature databases. Mick's directive \
             after the v0.2.6 archive incident: stop flagging .dll / .sys / etc.",
        )
        .color(theme::TEXT_LO)
        .small(),
    );
    ui.add_space(6.0);

    let cfg = &mut settings.exclusion_config;
    ui.checkbox(&mut cfg.enabled, "Enable exclusion filter")
        .on_hover_text(
            "Master toggle. When OFF, no exclusions apply regardless of \
         the pack checkboxes or custom rules below.",
        );

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(RichText::new("Preset packs").color(theme::TEXT_HI).strong());
    ui.add_space(4.0);

    // Per-pack row. Each pack ships with a fixed extension/path set
    // we show the counts of so users see what they're toggling.
    let presets = BuiltinPresets;
    for pack_id in PresetPackId::ALL {
        let pack = presets.get(pack_id);
        let mut active = cfg.active_packs.contains(&pack_id);
        let n_ext = pack.extensions.len();
        let n_paths = pack.paths.len();
        let label = format!(
            "{}  ({} extension{}, {} path pattern{})",
            pack_id.label(),
            n_ext,
            if n_ext == 1 { "" } else { "s" },
            n_paths,
            if n_paths == 1 { "" } else { "s" },
        );
        let safe = PresetPackId::SAFE_DEFAULTS.contains(&pack_id);
        let resp = ui.checkbox(&mut active, &label);
        if safe {
            resp.on_hover_text(
                "Safe-defaults pack (on by default for new installs in v0.2.7+). \
                 Recommended to leave ON unless you specifically want to find \
                 duplicates among files of this kind.",
            );
        }
        if active && !cfg.active_packs.contains(&pack_id) {
            cfg.active_packs.push(pack_id);
            // Preserve canonical order so the persisted TOML is
            // deterministic across saves.
            cfg.active_packs.sort_by_key(|p| {
                PresetPackId::ALL
                    .iter()
                    .position(|x| x == p)
                    .unwrap_or(usize::MAX)
            });
        }
        if !active {
            cfg.active_packs.retain(|p| *p != pack_id);
        }
    }

    ui.add_space(8.0);
    ui.separator();
    ui.add_space(8.0);
    ui.label(
        RichText::new("Custom extensions")
            .color(theme::TEXT_HI)
            .strong(),
    );
    ui.label(
        RichText::new("One per line; leading dot optional (\".tmp\" or \"tmp\" both work).")
            .color(theme::TEXT_LO)
            .small(),
    );
    let mut ext_text = cfg.custom_extensions.join("\n");
    let ext_resp = ui.add(
        egui::TextEdit::multiline(&mut ext_text)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .font(egui::TextStyle::Monospace),
    );
    if ext_resp.changed() {
        cfg.custom_extensions = ext_text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
    }

    ui.add_space(8.0);
    ui.label(
        RichText::new("Custom path patterns")
            .color(theme::TEXT_HI)
            .strong(),
    );
    ui.label(
        RichText::new("Glob syntax: `**/node_modules/**`, `/tmp/**`, etc. One pattern per line.")
            .color(theme::TEXT_LO)
            .small(),
    );
    let mut pat_text = cfg.custom_patterns.join("\n");
    let pat_resp = ui.add(
        egui::TextEdit::multiline(&mut pat_text)
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .font(egui::TextStyle::Monospace),
    );
    if pat_resp.changed() {
        cfg.custom_patterns = pat_text
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
    }

    ui.add_space(10.0);
    ui.separator();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .button(RichText::new("Restore safe defaults").color(theme::TEXT_HI))
            .on_hover_text(
                "Reset to the v0.2.7 defaults: master ON, 4 safe packs active, \
                 custom lists empty.",
            )
            .clicked()
        {
            *cfg = crate::exclusions::ExclusionConfig::default();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let total_ext: usize = cfg
                .active_packs
                .iter()
                .map(|p| presets.get(*p).extensions.len())
                .sum::<usize>()
                + cfg.custom_extensions.len();
            let total_paths: usize = cfg
                .active_packs
                .iter()
                .map(|p| presets.get(*p).paths.len())
                .sum::<usize>()
                + cfg.custom_patterns.len();
            ui.label(
                RichText::new(format!(
                    "Active: {} ext rules · {} path rules",
                    if cfg.enabled { total_ext } else { 0 },
                    if cfg.enabled { total_paths } else { 0 },
                ))
                .color(theme::TEXT_LO)
                .small(),
            );
        });
    });
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
    egui::ComboBox::from_id_salt("sd_network_channel_dropdown")
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
        let color = if registered {
            theme::TEXT_LO
        } else {
            theme::HOT
        };
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
                    if oauth::try_start_session(
                        provider,
                        active,
                        server_url,
                        id,
                        oauth::DEFAULT_OAUTH_TIMEOUT,
                    )
                    .is_err()
                    {
                        eprintln!("account: couldn't auto-retry OAuth (session already in flight)");
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
            let install_id = install::load().ok().flatten().map(|s| s.install_id);
            let id_short = install_id
                .as_deref()
                .map(|s| s.split('-').next().unwrap_or(s).to_string())
                .unwrap_or_else(|| "not registered".to_string());
            ui.label(
                RichText::new(format!("Status: Anonymous (UUID {id_short}…)"))
                    .color(theme::TEXT_HI),
            );
        }
        Some(oauth::AccountStatus::Linked {
            provider,
            display_name,
            ..
        }) => {
            use crate::gui::widgets::oauth_chooser::provider_icon;
            ui.horizontal(|ui| {
                ui.add(egui::Image::new(provider_icon(*provider)).max_size(egui::vec2(20.0, 20.0)));
                ui.label(
                    RichText::new(format!(
                        "Status: Linked — {display_name} ({})",
                        provider.display_name()
                    ))
                    .color(theme::TEXT_HI),
                );
            });
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
                RichText::new(format!("Registering machine ({}s)…", elapsed.as_secs()))
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
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
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
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
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

    ui.label(RichText::new("Privacy").color(theme::TEXT_HI).strong());
    ui.add_space(4.0);

    // Load mutable state. If the install isn't present (not
    // registered yet), the controls show a hint but stay disabled.
    let state_opt: Option<install::InstallState> = install::load().unwrap_or_default();

    let current_share = state_opt
        .as_ref()
        .map(|s| s.share_default)
        .unwrap_or(install::ShareDefault::AlwaysAsk);

    ui.horizontal(|ui| {
        ui.label(RichText::new("Submit frequency").color(theme::TEXT_HI));
        let enabled = state_opt.is_some();
        let label_text = match current_share {
            install::ShareDefault::AlwaysAsk => "Always ask",
            install::ShareDefault::AskNThenSticky => "Ask 3 times, then remember",
            install::ShareDefault::AutoOptIn => "Auto-submit",
            install::ShareDefault::Never => "Never",
        };
        ui.add_enabled_ui(enabled, |ui| {
            egui::ComboBox::from_id_salt("sd_share_frequency")
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
                            current_share == install::ShareDefault::AskNThenSticky,
                            "Ask 3 times, then remember",
                        )
                        .on_hover_text(
                            "Ask after the next few scans, then stick with your last choice: \
                             if you submitted, auto-submit going forward; if you skipped, stop \
                             asking. The 3rd prompt tells you it's being remembered.",
                        )
                        .clicked()
                    {
                        chosen = Some(install::ShareDefault::AskNThenSticky);
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
                        .selectable_label(current_share == install::ShareDefault::Never, "Never")
                        .on_hover_text(
                            "Never attempt to submit; never show the post-scan modal. \
                             Engine still builds the payload locally for diagnostic \
                             logging — set to Never to fully opt out.",
                        )
                        .clicked()
                    {
                        chosen = Some(install::ShareDefault::Never);
                    }
                    if let (Some(chosen), Some(mut s)) = (chosen, state_opt.clone()) {
                        if chosen != s.share_default {
                            s.share_default = chosen;
                            if let Err(e) = install::save(&s) {
                                eprintln!("leaderboard: failed to persist share preference: {e:?}");
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

    ui.add_space(12.0);
    render_public_profile_visibility(ui, state_opt.as_ref());

    ui.add_space(8.0);
    ui.label(
        RichText::new(
            "Submission fields: every hardware field in the submission is currently REQUIRED \
             for leaderboard ranking — none can be dropped without giving up your rank. \
             \"Preview a sample submission\" shows the complete payload (full transparency). \
             A privacy opt-in to omit GPU details will arrive together with GPU detection. \
             (The toggles above control PUBLIC PROFILE visibility, a separate account-level \
             surface.)",
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
                egui::Button::new(RichText::new("Reset install").color(theme::WARN))
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
            // §10.2: gate the destructive identity rotation behind an
            // explicit confirmation modal (rendered by the outer
            // show()), rather than firing on this single click. The
            // backup-then-rotate itself happens in perform_install_reset
            // once the user confirms.
            request_reset_confirm();
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
                    eprintln!("leaderboard: register OK, install_id={id}");
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
                ui.label(RichText::new(format!("Registering ({elapsed}s)…")).color(theme::TEXT_HI));
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        } else if ui
            .add(
                egui::Button::new(RichText::new("Register").color(theme::PANEL_DEEP).strong())
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
/// Render the 6-toggle "Public profile visibility" section
/// (#67). Each toggle drives one privacy flag on the
/// account-level rollup; toggling fires an async PATCH against
/// `/api/v1/account/privacy`. Local state is cached so the UI
/// doesn't flicker between clicks; the canonical server-side
/// state replaces the cache when the PATCH returns.
#[cfg(feature = "telemetry")]
fn render_public_profile_visibility(
    ui: &mut egui::Ui,
    install_state: Option<&crate::leaderboard::install::InstallState>,
) {
    use crate::leaderboard::account_privacy::PrivacyFlags;
    use parking_lot::Mutex;
    use std::sync::OnceLock;

    // Process-wide cache so the toggles render the same state
    // across opens of the Settings modal. None = haven't fetched
    // yet (and unregistered installs stay None forever).
    fn cache() -> &'static Mutex<Option<PrivacyFlags>> {
        static SLOT: OnceLock<Mutex<Option<PrivacyFlags>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }
    /// True while a PATCH worker is in flight; gates concurrent
    /// updates to a single one at a time.
    fn in_flight() -> &'static Mutex<bool> {
        static SLOT: OnceLock<Mutex<bool>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(false))
    }
    /// Most-recent outcome surfaced inline.
    fn last_outcome() -> &'static Mutex<Option<String>> {
        static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    ui.label(
        RichText::new("Public profile visibility")
            .color(theme::TEXT_HI)
            .strong(),
    );
    ui.add_space(2.0);
    ui.label(
        RichText::new(
            "What anonymous visitors see on your public profile page at \
             superdeduper.io/profile/<slug>. All default OFF — your achievement \
             grid + aggregate stats are always visible; everything else is \
             opt-in.",
        )
        .color(theme::TEXT_LO)
        .small()
        .italics(),
    );
    ui.add_space(6.0);

    if install_state.is_none() {
        ui.label(
            RichText::new("Register + link an account above to manage these.")
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );
        return;
    }

    // §10.2 UX fix (design 2026-05-29): these flags are ACCOUNT-level —
    // an unlinked (anonymous) install has no account row, so a PATCH
    // /account/privacy 401s and the toggle would silently revert (the
    // "looks dead" control Mick saw). Gate on link status: when unlinked,
    // show a Link CTA and render the toggles DISABLED rather than letting
    // them no-op. Don't even kick the fetch (it would 401 too).
    let linked = matches!(
        crate::leaderboard::oauth::status_for(crate::channel::active_channel()),
        Ok(crate::leaderboard::oauth::AccountStatus::Linked { .. })
    );
    if !linked {
        ui.label(
            RichText::new(
                "Link a Google or Discord account (Settings \u{2192} Account) to manage \
                 public-profile visibility. These control an account-level public profile, \
                 so they're available once this install is linked.",
            )
            .color(theme::TEXT_LO)
            .small()
            .italics(),
        );
        ui.add_space(4.0);
        // Render the toggles disabled so users see WHAT is configurable
        // without being able to fire a doomed 401 PATCH.
        let mut preview = PrivacyFlags::default();
        ui.add_enabled_ui(false, |ui| {
            let row = |ui: &mut egui::Ui, f: &mut bool, label: &str| {
                ui.checkbox(f, RichText::new(label).color(theme::TEXT_LO));
            };
            row(ui, &mut preview.show_display_name, "Display name");
            row(ui, &mut preview.show_provider, "Linked-provider badge");
            row(ui, &mut preview.show_avatar, "Discord avatar");
            row(ui, &mut preview.show_install_breakdown, "Per-install grant breakdown");
            row(ui, &mut preview.show_hardware_history, "Hardware history table");
            row(ui, &mut preview.show_recent_runs, "Recent runs table");
        });
        return;
    }

    // First render after install becomes available: kick a fetch.
    if cache().lock().is_none() && !*in_flight().lock() {
        if let Some(state) = install_state {
            *in_flight().lock() = true;
            let state_owned = state.clone();
            let server_url = state.server_url.clone();
            std::thread::Builder::new()
                .name("superdeduper-privacy-fetch".into())
                .spawn(move || {
                    use crate::leaderboard::account_privacy::{self, PrivacyOutcome};
                    let outcome = account_privacy::fetch(&state_owned, &server_url);
                    match outcome {
                        PrivacyOutcome::Ok(flags) => {
                            *cache().lock() = Some(flags);
                            *last_outcome().lock() = None;
                        }
                        PrivacyOutcome::Unauthorised(reason) => {
                            *last_outcome().lock() = Some(format!("auth: {reason}"));
                        }
                        PrivacyOutcome::Rejected(reason) => {
                            *last_outcome().lock() = Some(format!("rejected: {reason}"));
                        }
                        PrivacyOutcome::Transient(reason) => {
                            *last_outcome().lock() = Some(format!("transient: {reason}"));
                        }
                    }
                    *in_flight().lock() = false;
                })
                .ok();
        }
    }

    let mut current = cache().lock().clone().unwrap_or_default();
    let busy = *in_flight().lock();

    let mut changed = false;
    let toggle =
        |ui: &mut egui::Ui, field: &mut bool, label: &str, hover: &str, changed: &mut bool| {
            let resp = ui.checkbox(field, RichText::new(label).color(theme::TEXT_HI));
            if resp.on_hover_text(hover).changed() {
                *changed = true;
            }
        };

    ui.add_enabled_ui(!busy, |ui| {
        toggle(
            ui,
            &mut current.show_display_name,
            "Display name",
            "Show your display name (e.g., \"MickFixesJunk\") on the public profile.",
            &mut changed,
        );
        toggle(
            ui,
            &mut current.show_provider,
            "Linked-provider badge",
            "Show the Google / Discord badge next to your display name.",
            &mut changed,
        );
        toggle(
            ui,
            &mut current.show_avatar,
            "Discord avatar",
            "Show your Discord avatar at the top of the public profile.",
            &mut changed,
        );
        toggle(
            ui,
            &mut current.show_install_breakdown,
            "Per-install grant breakdown",
            "Show which of your linked installs earned each achievement \
             (e.g., \"Storage Crusader x3 from MacBook + Desktop + Laptop\").",
            &mut changed,
        );
        toggle(
            ui,
            &mut current.show_hardware_history,
            "Hardware history table",
            "Show the per-install hardware-class table (CPU model, threads, \
             drive type, RAM tier). Hardware-bracket leaderboard ranks are \
             always anonymous and not affected by this toggle.",
            &mut changed,
        );
        toggle(
            ui,
            &mut current.show_recent_runs,
            "Recent runs table",
            "Show recent scan timings + scope on the public profile. Timing \
             patterns can be identifying; default OFF.",
            &mut changed,
        );
    });

    if busy {
        ui.label(
            RichText::new("syncing…")
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );
    }
    if let Some(outcome) = last_outcome().lock().clone() {
        ui.label(RichText::new(outcome).color(theme::WARN).small().italics());
    }

    // Optimistic update + async PATCH.
    if changed && !busy {
        *cache().lock() = Some(current.clone());
        *last_outcome().lock() = None;
        if let Some(state) = install_state {
            *in_flight().lock() = true;
            let state_owned = state.clone();
            let server_url = state.server_url.clone();
            let flags_to_patch = current.clone();
            std::thread::Builder::new()
                .name("superdeduper-privacy-patch".into())
                .spawn(move || {
                    use crate::leaderboard::account_privacy::{self, PrivacyOutcome};
                    let outcome =
                        account_privacy::update(&state_owned, &server_url, &flags_to_patch);
                    match outcome {
                        PrivacyOutcome::Ok(canonical) => {
                            *cache().lock() = Some(canonical);
                            *last_outcome().lock() = None;
                        }
                        PrivacyOutcome::Unauthorised(reason) => {
                            *last_outcome().lock() = Some(format!("auth: {reason}"));
                        }
                        PrivacyOutcome::Rejected(reason) => {
                            *last_outcome().lock() = Some(format!("rejected: {reason}"));
                        }
                        PrivacyOutcome::Transient(reason) => {
                            *last_outcome().lock() = Some(format!("transient: {reason}"));
                        }
                    }
                    *in_flight().lock() = false;
                })
                .ok();
        }
    }
}

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
                egui::CornerRadius::ZERO,
                egui::Color32::from_black_alpha(160),
            );
        });
    egui::Window::new(
        RichText::new("Sample submission payload")
            .color(theme::TEXT_HI)
            .heading(),
    )
    // #84 — force the preview Window above the settings modal.
    // Without the explicit Order::Foreground hint, both windows
    // live in the default Middle layer and paint order put the
    // settings modal on top — making "Preview Sample Submission"
    // appear to do nothing from the user's perspective.
    .order(egui::Order::Foreground)
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
                    egui::Button::new(RichText::new("Close").color(theme::PANEL_DEEP).strong())
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
/// and adds an ellipsis when truncated; otherwise returns the
/// original.
///
/// Safe against UTF-8 multi-byte boundaries: backs up to the
/// previous char boundary if `cap` lands mid-codepoint. Without
/// this guard, a rejection reason carrying e.g. an em-dash (3-byte
/// UTF-8) could panic with `byte index N is not a char boundary`
/// — and rejection messages come from network input we don't
/// control, so a hostile backend could crash the GUI.
#[cfg(feature = "telemetry")]
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

#[cfg(all(test, feature = "telemetry"))]
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
pub fn build_sample_payload_json() -> String {
    use crate::leaderboard::{hardware, submission};
    use submission::{FEATURE_BIT_CACHE, FEATURE_BIT_FORMAT_AWARE};
    let inputs = submission::SubmissionInputs {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        run_uuid: "00000000-0000-0000-0000-000000000000".into(),
        scan_id: None,
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
            share_count_in_scope: None,
            dry_run: None,
            groups_reviewed_count: None,
        },
        result_summary: submission::ResultSummary {
            duplicate_groups: 18_204,
            duplicate_bytes_reclaimable: 38_100_000_000,
            largest_single_group_bytes: 4_200_000_000,
            actions_taken_summary: std::collections::BTreeMap::new(),
            placeholder_skip_count: None,
            placeholder_skip_bytes: None,
            client_found_dupsets: None,
        },
        bench: None,
    };
    let payload = submission::build_payload(&inputs, "00000000-0000-0000-0000-000000000000");
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| format!("(render failed: {e})"))
}

/// Build a synthetic CANONICAL-BENCH sample payload (pretty JSON) for the
/// bench "What exactly gets shared?" preview. The generic scan sample
/// (build_sample_payload_json) misrepresents the bench — it shows
/// corpus_kind="user-data" + bytes_scanned=320GB, contradicting the
/// bench's own "no personal files / synthetic" callout. This renders the
/// REAL bench shape: synthetic run_shape (corpus_kind=canonical-bench, the
/// ~2.4GB synthetic numbers) plus the bench fields (build_payload lifts
/// bench_proof/bench_run_id/corpus_version/protocol_version/tier
/// top-level). Numbers are representative of the corpus-v2-quick tier.
#[cfg(feature = "telemetry")]
pub fn build_bench_sample_payload_json() -> String {
    use crate::leaderboard::{hardware, submission};
    let inputs = submission::SubmissionInputs {
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        run_uuid: "00000000-0000-0000-0000-000000000000".into(),
        scan_id: None,
        hardware: hardware::detect(),
        run_shape: submission::RunShape {
            wall_clock_seconds: 1.05,
            bytes_scanned: 2_410_000_000,
            files_scanned: 8_600,
            hash_algorithm: "blake3".into(),
            walker_variant: "walker".into(),
            scope: "canonical-bench".into(),
            features_used_bitmap: 0,
            corpus_kind: "canonical-bench".into(),
            cache_hit_ratio: None,
            easter_egg_hits: Vec::new(),
            zero_byte_group_max: None,
            max_hardlink_count_in_scan: None,
            name_collision_count: None,
            share_count_in_scope: None,
            dry_run: None,
            groups_reviewed_count: None,
        },
        result_summary: submission::ResultSummary {
            duplicate_groups: 80,
            duplicate_bytes_reclaimable: 0,
            largest_single_group_bytes: 8_388_608,
            actions_taken_summary: std::collections::BTreeMap::new(),
            placeholder_skip_count: None,
            placeholder_skip_bytes: None,
            client_found_dupsets: None,
        },
        bench: Some(submission::CanonicalBench {
            protocol_version: "tcorpus-1".into(),
            corpus_version: "corpus-v2-quick".into(),
            tier: "quick".into(),
            bench_run_id: "00000000-0000-0000-0000-000000000000".into(),
            bench_proof: serde_json::json!({
                "answers": [
                    { "path_index": 12, "byte_offset": 53248, "byte_length": 4096,
                      "challenge_hash": "<blake3-of-corpus-bytes-at-this-range>" }
                ],
                "result_digest": "<blake3-of-your-dedupe-result>"
            }),
        }),
    };
    let payload = submission::build_payload(&inputs, "00000000-0000-0000-0000-000000000000");
    serde_json::to_string_pretty(&payload).unwrap_or_else(|e| format!("(render failed: {e})"))
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
                            submission::store_last_outcome(submission::SubmitOutcome::Rejected {
                                status: 0,
                                reason: "install not registered".into(),
                            });
                            return;
                        }
                    };
                    let inputs = match submission::take_pending() {
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
                        if let Err(e) = submission::enqueue(&inputs, &state.install_id, &signature)
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
            ui.label(RichText::new("Accepted").color(theme::ACCENT).strong());
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
                ui.hyperlink_to(RichText::new("view profile").color(theme::ACCENT), url);
            }
        }
        SubmitOutcome::DuplicateNoChange { .. } => {
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
                RichText::new("✓ Flagged for review")
                    .color(theme::ACCENT)
                    .strong(),
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
