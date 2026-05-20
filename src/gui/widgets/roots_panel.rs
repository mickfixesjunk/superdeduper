//! Roots panel — the list of folders to scan, with per-row controls
//! for marking a folder as a "reference" (source of truth) or removing
//! it from the list. Multi-folder is first-class: every entry feeds
//! into a single scan run; the engine pools file-system roots across
//! all drives and reports the union.
//!
//! Reference entries are never displayed as destructive-action
//! candidates in the Groups table: any group containing a reference
//! file pre-selects that reference file as the keeper.

use egui::{vec2, RichText, Ui};

use crate::gui::state::RootEntry;
use crate::gui::theme;

/// Actions the panel asks the App to take this frame.
#[derive(Debug, Clone)]
pub enum RootsAction {
    PickFolder,
    PickReferenceFolder,
    Remove(usize),
    ToggleReference(usize),
    StartScan,
    Pause,
    Cancel,
    /// Walk every root (reference included) and strip the
    /// `.superdupe` suffix from any file that has it — the safe-mode
    /// undo. Doesn't require a prior scan.
    Unsuperdupe,
}

pub fn show(
    ui: &mut Ui,
    roots: &[RootEntry],
    is_scanning: bool,
    can_resume: bool,
) -> Option<RootsAction> {
    let mut action: Option<RootsAction> = None;

    ui.label(RichText::new("Roots").color(theme::TEXT_LO).strong());
    ui.add_space(4.0);

    if roots.is_empty() {
        ui.label(
            RichText::new("No folders yet — click + Add folder.")
                .color(theme::TEXT_LO)
                .italics()
                .small(),
        );
    } else {
        for (i, root) in roots.iter().enumerate() {
            ui.horizontal(|ui| {
                let star = if root.is_reference { "★" } else { "☆" };
                let star_color = if root.is_reference { theme::WARN } else { theme::TEXT_LO };
                if ui
                    .add(
                        egui::Button::new(RichText::new(star).color(star_color))
                            .frame(false)
                            .min_size(vec2(18.0, 18.0)),
                    )
                    .on_hover_text(if root.is_reference {
                        "Reference — files here are never deleted."
                    } else {
                        "Toggle as reference / source of truth."
                    })
                    .clicked()
                {
                    action = Some(RootsAction::ToggleReference(i));
                }

                let path_str = root.path.to_string_lossy();
                let color = if root.is_reference { theme::WARN } else { theme::TEXT_HI };
                ui.add(egui::Label::new(
                    RichText::new(truncate(&path_str, 40).to_string())
                        .color(color)
                        .monospace()
                        .small(),
                ))
                .on_hover_text(path_str.as_ref());

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("✕").color(theme::TEXT_LO))
                                .frame(false)
                                .min_size(vec2(18.0, 18.0)),
                        )
                        .on_hover_text("Remove from scan list.")
                        .clicked()
                    {
                        action = Some(RootsAction::Remove(i));
                    }
                });
            });
        }
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui
            .add(
                egui::Button::new(
                    RichText::new("📁  Add folder").color(theme::TEXT_HI),
                )
                .min_size(vec2(110.0, 24.0)),
            )
            .on_hover_text("Add a folder to scan.")
            .clicked()
        {
            action = Some(RootsAction::PickFolder);
        }
        if ui
            .add(
                egui::Button::new(RichText::new("★").color(theme::WARN))
                    .min_size(vec2(32.0, 24.0)),
            )
            .on_hover_text("Add a folder as a reference (source of truth).")
            .clicked()
        {
            action = Some(RootsAction::PickReferenceFolder);
        }
    });

    ui.add_space(6.0);

    let can_scan = !roots.is_empty() && !is_scanning;
    let can_unsuperdupe = !roots.is_empty() && !is_scanning;
    ui.horizontal(|ui| {
        let primary_label = if is_scanning {
            "⏸  Pause"
        } else if can_resume {
            "▶  Resume scan"
        } else {
            "▶  Start scan"
        };
        let primary = egui::Button::new(
            RichText::new(primary_label).color(theme::PANEL_DEEP).strong(),
        )
        .fill(theme::ACCENT)
        .min_size(vec2(150.0, 28.0));
        if ui
            .add_enabled(can_scan || is_scanning, primary)
            .clicked()
        {
            action = Some(if is_scanning {
                RootsAction::Pause
            } else {
                RootsAction::StartScan
            });
        }

        // Unsuperdupe sits beside Start scan — no scan required, just
        // walks the roots and strips `.superdupe` extensions back.
        let unsuperdupe = egui::Button::new(
            RichText::new("↩  Unsuperdupe").color(theme::TEXT_HI),
        )
        .fill(theme::PANEL_DEEP)
        .min_size(vec2(140.0, 28.0));
        if ui
            .add_enabled(can_unsuperdupe, unsuperdupe)
            .on_hover_text(
                "Walk every root and rename any *.superdupe file back \
                 to its original. Reverses safe-mode rename. No scan \
                 required.",
            )
            .clicked()
        {
            action = Some(RootsAction::Unsuperdupe);
        }

        if is_scanning {
            let cancel = egui::Button::new(
                RichText::new("⏹  Cancel").color(theme::HOT),
            )
            .min_size(vec2(90.0, 28.0));
            if ui.add(cancel).clicked() {
                action = Some(RootsAction::Cancel);
            }
        }
    });

    action
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        // Keep the tail; that's where the meaningful folder name lives.
        let cut_at = s.chars().count() - (n - 2);
        let tail: String = s.chars().skip(cut_at).collect();
        format!("…{tail}")
    }
}
