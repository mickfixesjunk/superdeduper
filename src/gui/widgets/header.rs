//! Top status bar — title, settings button, status line, big stat tiles.

use egui::{vec2, Align, Color32, CornerRadius, Frame, Layout, RichText, Sense, Stroke, Ui};

use crate::gui::state::UiState;
use crate::gui::theme;
use crate::pipeline::hash::HashAlgo;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum HeaderAction {
    None,
    OpenSettings,
    /// T-BENCH-ME: open the "Benchmark your machine" consent modal.
    OpenBenchmark,
}

/// Output of one header render: action requested + (optionally) the
/// rectangle where the per-stat row was drawn. The caller anchors
/// the cache-fast-forward sparkle particles to this rect so they
/// drift up off the running totals when resume kicks in.
pub struct HeaderOutput {
    pub action: HeaderAction,
    pub stats_rect: Option<egui::Rect>,
}

pub fn show(ui: &mut Ui, state: &UiState, hash_algo: HashAlgo, is_scanning: bool) -> HeaderOutput {
    let mut action = HeaderAction::None;
    let mut stats_rect: Option<egui::Rect> = None;
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new("superdeduper").color(theme::ACCENT).heading());
        // Version + git SHA: makes "which build am I running?"
        // visually answerable without uniquely-named EXEs. SHA comes
        // from build.rs at compile-time; falls back to "dev" if the
        // build host has no git checkout (e.g. tarball install).
        let build_tag = format!("v{} · {}", env!("CARGO_PKG_VERSION"), env!("SD_BUILD_SHA"),);
        ui.label(RichText::new(build_tag).color(theme::TEXT_LO).monospace())
            .on_hover_text(
                "Cargo version (Cargo.toml) and git short-SHA of the commit \
                 this binary was built from. Click-paste me into any bug \
                 report so we know exactly which build is producing the issue.",
            );
        ui.add_space(8.0);

        // Hash-algo pill — shows the active content-hash algo and
        // doubles as a fast-path to the Settings modal (clicking
        // opens settings). Disabled mid-scan; changing the algo while
        // the engine is running would split the same scan's results
        // across two hash schemes.
        if !is_scanning && hash_algo_pill(ui, hash_algo) {
            action = HeaderAction::OpenSettings;
        } else if is_scanning {
            hash_algo_pill_disabled(ui, hash_algo);
        }
        ui.add_space(8.0);

        let settings_btn = egui::Button::new(RichText::new("⚙  Settings").color(theme::TEXT_HI))
            .fill(theme::PANEL_DEEP)
            .min_size(vec2(110.0, 28.0));
        let settings_resp = ui.add_enabled(!is_scanning, settings_btn);
        let settings_resp = if is_scanning {
            settings_resp.on_hover_text("Cancel the running scan before changing settings.")
        } else {
            settings_resp.on_hover_text(
                "Engine options (size filters, glob patterns, format-aware, threads).",
            )
        };
        if settings_resp.clicked() {
            action = HeaderAction::OpenSettings;
        }

        // T-BENCH-ME: "Benchmark" button beside Settings. Opens the
        // consent/explainer modal (default opt-out — nothing downloads,
        // runs, or submits until the user clicks an action inside it).
        // Telemetry-gated; only present in builds that can submit.
        //
        // A-prod-undark-bench (2026-05-30 19:02 PST): prod V3 corpus live
        // (web Mick GO override). PROD-DARK button-hide removed; the
        // Benchmark button now shows on every channel.
        #[cfg(feature = "telemetry")]
        {
            ui.add_space(8.0);
            let bench_btn = egui::Button::new(RichText::new("🏁  Benchmark").color(theme::TEXT_HI))
                .fill(theme::PANEL_DEEP)
                .min_size(vec2(118.0, 28.0));
            let bench_resp = ui.add_enabled(!is_scanning, bench_btn);
            let bench_resp = if is_scanning {
                bench_resp.on_hover_text("Finish the running scan before benchmarking.")
            } else {
                bench_resp.on_hover_text(
                    "Time your machine on a standard synthetic corpus and \
                     compare on the public Dedupe Hall of Fame. Touches no \
                     personal files.",
                )
            };
            if bench_resp.clicked() {
                action = HeaderAction::OpenBenchmark;
            }
        }

        ui.separator();
        let status = if state.status.is_empty() {
            "Idle — add folders in the sidebar, then click Start scan."
        } else {
            state.status.as_str()
        };
        ui.label(RichText::new(status).color(theme::TEXT_HI));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let resp = ui
                .scope(|ui| {
                    big_stat(
                        ui,
                        "Reclaimable",
                        &theme::humansize(state.totals.reclaimable_bytes),
                        theme::HOT,
                    );
                    big_stat(
                        ui,
                        "Duplicates",
                        &state.totals.duplicates.to_string(),
                        theme::ACCENT,
                    );
                    big_stat(
                        ui,
                        "Read",
                        &theme::humansize(state.totals.bytes_read),
                        theme::COOL,
                    );
                    if let Some(elapsed) = state.scan_elapsed() {
                        big_stat(
                            ui,
                            "Elapsed",
                            &crate::gui::state::fmt_wallclock(elapsed),
                            theme::TEXT_HI,
                        );
                    }
                })
                .response;
            stats_rect = Some(resp.rect);
        });
    });
    HeaderOutput { action, stats_rect }
}

fn big_stat(ui: &mut Ui, label: &str, value: &str, color: egui::Color32) {
    ui.allocate_ui_with_layout(vec2(140.0, 38.0), Layout::top_down(Align::Max), |ui| {
        ui.label(RichText::new(value).color(color).strong().size(18.0));
        ui.label(RichText::new(label).color(theme::TEXT_LO).small());
    });
    ui.add_space(12.0);
}

/// Coloured rounded "pill" showing the currently-selected content-
/// hash algorithm. Whole pill is one click target; clicking returns
/// `true` so the caller can flip its `HeaderAction` to OpenSettings.
/// The pill is the prominent way to see "what am I hashing with right
/// now?" without diving into Settings.
fn hash_algo_pill(ui: &mut Ui, algo: HashAlgo) -> bool {
    let (label, fill) = match algo {
        HashAlgo::Blake3 => ("BLAKE3", theme::COOL),
        HashAlgo::River5 => ("RIVER5", theme::ACCENT),
    };
    let tip = match algo {
        HashAlgo::Blake3 => {
            "Content hash: BLAKE3 (32-byte cryptographic). Click to change in Settings."
        }
        HashAlgo::River5 => {
            "Content hash: RIVER5 (16-byte, AES-NI hardware-accelerated). Click to change in Settings."
        }
    };
    // Pill colours: a translucent fill of the algo's theme colour
    // plus a contrasting border so it reads at a glance even on
    // the dark background.
    let bg = Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 32);
    let frame = Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(10))
        .stroke(Stroke::new(1.0, fill))
        .inner_margin(egui::Margin::symmetric(10, 4));
    let resp = frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("#")
                        .color(fill)
                        .monospace()
                        .strong()
                        .size(13.0),
                );
                ui.label(
                    RichText::new(label)
                        .color(fill)
                        .monospace()
                        .strong()
                        .size(13.0),
                );
            });
        })
        .response
        .interact(Sense::click())
        .on_hover_text(tip);
    resp.clicked()
}

/// Mid-scan variant: same pill, but inert (no click handler, tip
/// explains why). Keeps the visual continuity (user still sees the
/// algo) without letting them open Settings to switch it.
fn hash_algo_pill_disabled(ui: &mut Ui, algo: HashAlgo) {
    let (label, fill) = match algo {
        HashAlgo::Blake3 => ("BLAKE3", theme::COOL),
        HashAlgo::River5 => ("RIVER5", theme::ACCENT),
    };
    let bg = Color32::from_rgba_unmultiplied(fill.r(), fill.g(), fill.b(), 32);
    let frame = Frame::new()
        .fill(bg)
        .corner_radius(CornerRadius::same(10))
        .stroke(Stroke::new(1.0, fill))
        .inner_margin(egui::Margin::symmetric(10, 4));
    frame
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("#")
                        .color(fill)
                        .monospace()
                        .strong()
                        .size(13.0),
                );
                ui.label(
                    RichText::new(label)
                        .color(fill)
                        .monospace()
                        .strong()
                        .size(13.0),
                );
            });
        })
        .response
        .on_hover_text("Hash algorithm can't change mid-scan.");
}
