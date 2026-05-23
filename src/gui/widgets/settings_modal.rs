//! Settings modal — exposes the engine knobs the spec defines as CLI
//! flags. Persisted via egui's `persistence` feature so the settings
//! survive app restarts.

use egui::{Context, RichText, Window};

use crate::gui::state::ScanSettings;
use crate::gui::theme;

/// Returns `true` if the user clicked Close / Apply this frame.
pub fn show(ctx: &Context, open: &mut bool, settings: &mut ScanSettings) -> bool {
    let mut closed = false;
    Window::new(RichText::new("⚙ Settings").color(theme::TEXT_HI).heading())
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(440.0)
        .show(ctx, |ui| {
            ui.label(
                RichText::new("Knobs apply to the next scan.")
                    .color(theme::TEXT_LO)
                    .small(),
            );
            ui.add_space(8.0);

            ui.heading("Size filters");
            ui.horizontal(|ui| {
                ui.label("Min size (bytes):");
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

            ui.add_space(8.0);
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

            ui.add_space(8.0);
            ui.heading("Engine");
            // RIVER5 description shown when the user hovers the
            // selector OR the active row in the dropdown. Kept
            // close to one screenful so it doesn't dominate the
            // modal. Explicitly names what RIVER5 is NOT (a
            // cryptographic hash) so users picking between the
            // two understand the tradeoff in one read.
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
                ui.label("Content hash:");
                let mut algo = settings.hash_algo;
                let combo = egui::ComboBox::from_id_source("hash-algo")
                    .selected_text(match algo {
                        crate::pipeline::hash::HashAlgo::Blake3 => {
                            "BLAKE3 (32-byte, cryptographic)"
                        }
                        crate::pipeline::hash::HashAlgo::River5 => {
                            "RIVER5 (16-byte, AES-NI, default)"
                        }
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
                // Tooltip on the closed combo so users see the
                // description without having to open the dropdown.
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
                    "Default since v0.2: RIVER5. Switch to BLAKE3 only if \
                     you need cryptographic-strength collision resistance. \
                     The cache stores the algo per row so switching doesn't \
                     pull stale hashes.",
                )
                .color(theme::TEXT_LO)
                .small(),
            );
            ui.add_space(4.0);
            ui.checkbox(
                &mut settings.use_format_aware,
                "Tier 0 format-aware fingerprints",
            );
            ui.checkbox(
                &mut settings.use_cache,
                "Persistent cache (USN delta + last hashes)",
            );
            // Indent the always-use sub-setting so the relationship
            // to the cache toggle above it reads visually.
            ui.indent("always-use-cache-indent", |ui| {
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
            ui.checkbox(
                &mut settings.paranoid,
                "Paranoid byte-by-byte confirm before reporting",
            );
            ui.checkbox(
                &mut settings.follow_links,
                "Follow reparse points / symlinks",
            );
            ui.checkbox(
                &mut settings.allow_system_paths,
                "Permit scanning system paths (C:\\Windows etc.)",
            );
            ui.checkbox(
                &mut settings.skip_preflight,
                "Skip pre-flight modal before each scan",
            )
            .on_hover_text(
                "When ON, scans start immediately without the score-card modal. \
                 You can still trigger Diagnose manually from the CLI to see \
                 your machine's profile.",
            );

            ui.horizontal(|ui| {
                ui.label("Threads:");
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

            ui.add_space(12.0);
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
                            .fill(theme::ACCENT),
                        )
                        .clicked()
                    {
                        closed = true;
                    }
                });
            });

            ui.add_space(12.0);
            ui.heading("Safety");
            // Snapshot the value before the &mut borrow so the label
            // colour can read it without colliding with the checkbox's
            // mutable reference.
            let bypass_on = settings.bypass_destructive_confirmation;
            let bypass_check = ui.checkbox(
                &mut settings.bypass_destructive_confirmation,
                RichText::new("Bypass \"type DELETE\" confirmation for destructive actions").color(
                    if bypass_on {
                        theme::HOT
                    } else {
                        theme::TEXT_HI
                    },
                ),
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
        });
    closed
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
