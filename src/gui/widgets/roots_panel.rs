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
use crate::path_display::for_user_display;

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
    /// `.superdeduper` suffix from any file that has it — the safe-mode
    /// undo. Doesn't require a prior scan.
    Unsuperdeduper,
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
                let star_color = if root.is_reference {
                    theme::WARN
                } else {
                    theme::TEXT_LO
                };
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

                let path_str = for_user_display(&root.path);
                let color = if root.is_reference {
                    theme::WARN
                } else {
                    theme::TEXT_HI
                };
                ui.add(egui::Label::new(
                    RichText::new(truncate(&path_str, 40).to_string())
                        .color(color)
                        .monospace()
                        .small(),
                ))
                .on_hover_text(&path_str);

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Big red X — was a small grey ✕ which the user
                    // couldn't see was a remove button. Bolder glyph,
                    // larger hit zone, hover state shows the danger
                    // colour so it visually reads as "destructive".
                    let btn =
                        egui::Button::new(RichText::new("✖").color(theme::HOT).size(16.0).strong())
                            .frame(false)
                            .min_size(vec2(24.0, 24.0));
                    // Disable mid-scan: removing a root while the
                    // engine is iterating over it would race the walker.
                    let response = ui.add_enabled(!is_scanning, btn);
                    let response = if is_scanning {
                        response.on_hover_text("Cancel the running scan before removing roots.")
                    } else {
                        response.on_hover_text("Remove this root from the scan list.")
                    };
                    if response.clicked() {
                        action = Some(RootsAction::Remove(i));
                    }
                });
            });
        }
    }

    ui.add_space(6.0);
    ui.horizontal(|ui| {
        // Disable add-folder buttons mid-scan: the engine has already
        // snapshotted the root list; any folder added now would be
        // silently ignored until the next scan, which is misleading.
        let add_btn = egui::Button::new(RichText::new("📁  Add folder").color(theme::TEXT_HI))
            .min_size(vec2(110.0, 24.0));
        let add_resp = ui.add_enabled(!is_scanning, add_btn);
        let add_resp = if is_scanning {
            add_resp.on_hover_text("Cancel the running scan before adding folders.")
        } else {
            add_resp.on_hover_text("Add a folder to scan.")
        };
        if add_resp.clicked() {
            action = Some(RootsAction::PickFolder);
        }
        let ref_btn =
            egui::Button::new(RichText::new("★").color(theme::WARN)).min_size(vec2(32.0, 24.0));
        let ref_resp = ui.add_enabled(!is_scanning, ref_btn);
        let ref_resp = if is_scanning {
            ref_resp.on_hover_text("Cancel the running scan before adding reference folders.")
        } else {
            ref_resp.on_hover_text("Add a folder as a reference (source of truth).")
        };
        if ref_resp.clicked() {
            action = Some(RootsAction::PickReferenceFolder);
        }
    });

    ui.add_space(6.0);

    let can_scan = !roots.is_empty() && !is_scanning;
    let can_unsuperdeduper = !roots.is_empty() && !is_scanning;
    ui.horizontal(|ui| {
        let primary_label = if is_scanning {
            "⏸  Pause"
        } else if can_resume {
            "▶  Resume scan"
        } else {
            "▶  Start scan"
        };
        let primary = egui::Button::new(
            RichText::new(primary_label)
                .color(theme::PANEL_DEEP)
                .strong(),
        )
        .fill(theme::ACCENT)
        .min_size(vec2(150.0, 28.0));
        let primary_response = ui.add_enabled(can_scan || is_scanning, primary);
        // #189 — accessibility tooltip surfaces the keyboard shortcut.
        // Screen readers read tooltip text; the hint also helps
        // power-users + UIA SendKeys harnesses that drive sd via
        // assistive-tech APIs. See gui::accessibility for the catalog.
        //
        // PERF (#191 overnight push, 2026-05-31): tooltip strings are now
        // platform-cfg const &str so we do NOT format!() / allocate a
        // String every frame the button is rendered. The Ctrl+R-vs-Cmd+R
        // distinction is the only thing the runtime tooltip needs to
        // express, and that's a compile-time platform fact.
        #[cfg(target_os = "macos")]
        const TOOLTIP_RUN: &str = "Run a duplicate scan over the listed roots (⌘R or F5).";
        #[cfg(not(target_os = "macos"))]
        const TOOLTIP_RUN: &str = "Run a duplicate scan over the listed roots (Ctrl+R or F5).";
        const TOOLTIP_PAUSE: &str = "Pause the in-flight scan (Esc).";
        let primary_response = if is_scanning {
            primary_response.on_hover_text(TOOLTIP_PAUSE)
        } else {
            primary_response.on_hover_text(TOOLTIP_RUN)
        };
        if primary_response.clicked() {
            action = Some(if is_scanning {
                RootsAction::Pause
            } else {
                RootsAction::StartScan
            });
        }

        // Unsuperdeduper sits beside Start scan — no scan required, just
        // walks the roots and strips `.superdeduper` extensions back.
        let unsuperdeduper =
            egui::Button::new(RichText::new("↩  Unsuperdeduper").color(theme::TEXT_HI))
                .fill(theme::PANEL_DEEP)
                .min_size(vec2(140.0, 28.0));
        if ui
            .add_enabled(can_unsuperdeduper, unsuperdeduper)
            .on_hover_text(
                "Walk every root and rename any *.superdeduper file back \
                 to its original. Reverses safe-mode rename. No scan \
                 required.",
            )
            .clicked()
        {
            action = Some(RootsAction::Unsuperdeduper);
        }

        // The Archive Dupes button used to live here. It moved to
        // the bulk-action dropdown above the duplicate-groups table
        // so both bulk operations (safe-rename + archive) share
        // one consolidated control. See widgets/groups_table.rs.

        if is_scanning {
            let cancel = egui::Button::new(RichText::new("⏹  Cancel").color(theme::HOT))
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
