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
            ui.horizontal(|ui| {
                ui.label("Content hash:");
                let mut algo = settings.hash_algo;
                egui::ComboBox::from_id_source("hash-algo")
                    .selected_text(match algo {
                        crate::pipeline::hash::HashAlgo::Blake3 => "BLAKE3 (32-byte, cryptographic)",
                        crate::pipeline::hash::HashAlgo::River5 => "River5 (16-byte, AES-NI)",
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut algo,
                            crate::pipeline::hash::HashAlgo::Blake3,
                            "BLAKE3 (32-byte, cryptographic)",
                        );
                        ui.selectable_value(
                            &mut algo,
                            crate::pipeline::hash::HashAlgo::River5,
                            "River5 (16-byte, AES-NI hardware-accelerated)",
                        );
                    });
                if algo != settings.hash_algo {
                    settings.hash_algo = algo;
                }
            });
            ui.label(
                RichText::new(
                    "River5 outputs are 16 bytes vs BLAKE3's 32. The cache stores \
                     the algo per row so switching doesn't pull stale hashes.",
                )
                .color(theme::TEXT_LO)
                .small(),
            );
            ui.add_space(4.0);
            ui.checkbox(&mut settings.use_format_aware, "Tier 0 format-aware fingerprints");
            ui.checkbox(&mut settings.use_cache, "Persistent cache (USN delta + last hashes)");
            ui.checkbox(&mut settings.paranoid, "Paranoid byte-by-byte confirm before reporting");
            ui.checkbox(&mut settings.follow_links, "Follow reparse points / symlinks");
            ui.checkbox(&mut settings.allow_system_paths, "Permit scanning system paths (C:\\Windows etc.)");

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
        });
    closed
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
