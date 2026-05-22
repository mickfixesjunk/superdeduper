//! Sortable list of confirmed duplicate groups.
//!
//! Rows show size / copy count / reclaimable bytes and the keeper's
//! path. Click a row to expand the full member list with `keep` /
//! `dupe` tagging. Per-group actions live in a row of buttons:
//!
//! * **Recycle others** — moves every dupe in the group to the
//!   Recycle Bin via `SHFileOperationW(FOF_ALLOWUNDO)`. Reversible.
//! * **Hardlink others** — replaces each dupe with a hardlink to the
//!   keeper. Frees the on-disk space without touching paths.
//! * **Open keeper** — opens the keeper in Explorer so you can sanity
//!   check it before taking destructive action.

use std::path::{Path, PathBuf};

use egui::{Color32, RichText, ScrollArea, Ui};
use egui_extras::{Column, TableBuilder};

use crate::gui::events::DuplicateGroupSummary;
use crate::gui::state::UiState;
use crate::gui::theme;

/// One action the user has asked the engine to perform on a group.
#[derive(Debug, Clone)]
pub enum GroupAction {
    /// Recycle every non-keeper in the group.
    RecycleOthers {
        keeper: PathBuf,
        dupes: Vec<PathBuf>,
    },
    /// Replace every dupe with a hardlink to the keeper (same volume).
    HardlinkOthers {
        keeper: PathBuf,
        dupes: Vec<PathBuf>,
    },
    /// Highlight the keeper in Explorer with the file selected
    /// inside its parent folder (Windows: `explorer.exe /select,<path>`).
    Reveal(PathBuf),
    /// Open the file with the user's default application — same as
    /// double-clicking it in Explorer.
    OpenFile(PathBuf),
    /// Open the enclosing directory in Explorer with no file
    /// selected. Distinct from Reveal because users sometimes want
    /// "show me this folder" vs "show me this file inside its folder".
    OpenFolder(PathBuf),
    /// Safe-mode: append `.superdeduper` to every non-keeper. Reversible
    /// via Unsuperdeduper; nothing is deleted.
    SafeRenameOthers {
        keeper: PathBuf,
        dupes: Vec<PathBuf>,
    },
    /// Safe-rename across EVERY visible duplicate group at once.
    SafeRenameAllVisible,
    /// Archive every dupe across every visible group — the GUI's
    /// roots panel used to host this as a standalone "Archive dupes"
    /// button; it now lives next to Safe-rename in the bulk-action
    /// dropdown above the results table so both bulk actions sit in
    /// the same place.
    ArchiveAllVisible,
}

/// The two bulk-action options the dropdown above the results
/// table can execute. Both operate across every visible duplicate
/// group; the user picks the action, the Go button runs it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BulkAction {
    #[default]
    SafeRenameDupes,
    ArchiveDupes,
}

impl BulkAction {
    fn label(self) -> &'static str {
        match self {
            BulkAction::SafeRenameDupes => "🛡 Safe-rename dupes",
            BulkAction::ArchiveDupes => "📦 Archive dupes",
        }
    }
}

#[derive(Default)]
pub struct GroupsTableState {
    expanded: hashbrown::HashSet<usize>,
    /// Records "this group has been acted on" so we hide the buttons
    /// after the action is queued (prevents double-clicks before the
    /// UI re-syncs).
    pub acted: hashbrown::HashSet<usize>,
    /// Sticky selection for the bulk-action dropdown; persists across
    /// re-renders so the user doesn't have to re-pick on every scan.
    pub bulk_action: BulkAction,
}

/// Render the unfiltered table. Kept for callers that don't need a
/// drive filter (the App always uses `show_filtered`).
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    table_state: &mut GroupsTableState,
) -> Option<GroupAction> {
    show_filtered(ui, state, table_state, None, &[])
}

/// Render the table, filtering groups so only those with at least one
/// member under `drive_root` (or any of `reference_roots`) are shown.
/// Reference paths are exempt — they always show through any filter,
/// matching the user's expectation that source-of-truth folders are
/// always visible.
pub fn show_filtered(
    ui: &mut Ui,
    state: &UiState,
    table_state: &mut GroupsTableState,
    drive_root: Option<&std::path::Path>,
    reference_roots: &[std::path::PathBuf],
) -> Option<GroupAction> {
    let mut clicked: Option<GroupAction> = None;

    ui.label(
        RichText::new("Duplicate groups")
            .color(theme::TEXT_LO)
            .strong(),
    );
    ui.add_space(4.0);

    if state.duplicates.is_empty() {
        ui.add_space(40.0);
        ui.vertical_centered(|ui| {
            ui.label(
                RichText::new("No duplicates yet.")
                    .color(theme::TEXT_LO)
                    .italics()
                    .size(16.0),
            );
            ui.label(
                RichText::new(
                    "When a scan finishes, every confirmed duplicate group lands here. \
                     Click a row to see all the files in the group; the keeper is in green.",
                )
                .color(theme::TEXT_LO)
                .size(12.0),
            );
        });
        return clicked;
    }

    // Apply the drive filter: keep a group if any member lives under
    // drive_root OR under any reference root. (Empty filter ⇒ keep
    // every group, original behaviour.)
    let mut sorted: Vec<(usize, &DuplicateGroupSummary)> = state
        .duplicates
        .iter()
        .enumerate()
        .filter(|(_, g)| group_passes_filter(g, drive_root, reference_roots))
        .collect();
    sorted.sort_by(|a, b| {
        let sa =
            a.1.size
                .saturating_mul(a.1.files.len().saturating_sub(1) as u64);
        let sb =
            b.1.size
                .saturating_mul(b.1.files.len().saturating_sub(1) as u64);
        sb.cmp(&sa)
    });

    if sorted.is_empty() {
        ui.label(
            RichText::new("No duplicates on this drive.")
                .color(theme::TEXT_LO)
                .italics(),
        );
        return clicked;
    }

    // Bulk safe-rename header row — one button to safe-rename every
    // non-keeper across every visible group. Reversible via the
    // Unsuperdeduper button in the Roots panel; never deletes anything.
    let visible_dupe_count: usize = sorted
        .iter()
        .map(|(_, g)| g.files.len().saturating_sub(1))
        .sum();
    // Bulk-action row: a dropdown for "what to do across every
    // visible group" + a Go button to execute it. Replaces the
    // single Safe-rename-ALL button, and folds in the Archive-dupes
    // button that used to live on the roots panel — both bulk
    // actions now sit in the same place.
    ui.horizontal(|ui| {
        let action_selected = table_state.bulk_action;
        egui::ComboBox::from_id_source("bulk-action-combo")
            .selected_text(action_selected.label())
            .width(220.0)
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut table_state.bulk_action,
                    BulkAction::SafeRenameDupes,
                    BulkAction::SafeRenameDupes.label(),
                );
                ui.selectable_value(
                    &mut table_state.bulk_action,
                    BulkAction::ArchiveDupes,
                    BulkAction::ArchiveDupes.label(),
                );
            });

        let go_label = if visible_dupe_count > 0 {
            format!("Go ({} files)", visible_dupe_count)
        } else {
            "Go".to_string()
        };
        let go = egui::Button::new(RichText::new(go_label).color(theme::PANEL_DEEP).strong())
            .fill(theme::ACCENT_DIM)
            .min_size(egui::vec2(120.0, 24.0));
        let hover = match table_state.bulk_action {
            BulkAction::SafeRenameDupes => {
                "Append .superdeduper to every non-keeper across every \
                 visible group. Reversible: click Unsuperdeduper in the \
                 Roots panel to restore. Reference paths are never touched."
            }
            BulkAction::ArchiveDupes => {
                "Pick a destination folder, then move every duplicate \
                 (except the keeper per group, and never anything under a \
                 reference root) into that folder. Preserves the original \
                 directory tree under the destination so a future restore \
                 can put files back. Writes a manifest JSON alongside."
            }
        };
        if ui
            .add_enabled(visible_dupe_count > 0, go)
            .on_hover_text(hover)
            .clicked()
        {
            clicked = Some(match table_state.bulk_action {
                BulkAction::SafeRenameDupes => GroupAction::SafeRenameAllVisible,
                BulkAction::ArchiveDupes => GroupAction::ArchiveAllVisible,
            });
        }

        let trailer = match table_state.bulk_action {
            BulkAction::SafeRenameDupes => "safe-mode (no files deleted)",
            BulkAction::ArchiveDupes => "moves dupes, writes manifest",
        };
        ui.label(
            RichText::new(trailer)
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );
    });
    ui.add_space(4.0);

    ScrollArea::vertical()
        .id_source("groups-table")
        .show(ui, |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                // `Column::exact` looks like it would be resizable
                // because the table is `.resizable(true)`, but exact
                // columns deliberately don't draw a drag handle. Use
                // `Column::initial(N).resizable(true)` for every
                // data column so the user can drag any of the
                // dividers — including the one to the left of the
                // keeper-path column, which is the practical way to
                // give that column more width on a wide window.
                .column(Column::initial(36.0).resizable(true))
                .column(Column::initial(90.0).resizable(true))
                .column(Column::initial(60.0).resizable(true))
                .column(Column::initial(90.0).resizable(true))
                .column(Column::initial(220.0).resizable(true))
                .column(Column::remainder())
                .header(20.0, |mut h| {
                    h.col(|ui| {
                        ui.label(RichText::new("#").color(theme::TEXT_LO).small());
                    });
                    h.col(|ui| {
                        ui.label(RichText::new("Size").color(theme::TEXT_LO).small());
                    });
                    h.col(|ui| {
                        ui.label(RichText::new("Copies").color(theme::TEXT_LO).small());
                    });
                    h.col(|ui| {
                        ui.label(RichText::new("Reclaim").color(theme::TEXT_LO).small());
                    });
                    h.col(|ui| {
                        ui.label(RichText::new("Actions").color(theme::TEXT_LO).small());
                    });
                    h.col(|ui| {
                        ui.label(RichText::new("Keeper path").color(theme::TEXT_LO).small());
                    });
                })
                .body(|mut body| {
                    for (i, (orig_idx, g)) in sorted.iter().enumerate() {
                        let savings = g
                            .size
                            .saturating_mul(g.files.len().saturating_sub(1) as u64);
                        let is_open = table_state.expanded.contains(orig_idx);
                        let acted = table_state.acted.contains(orig_idx);
                        let keeper = g.files.first().cloned();
                        let dupes: Vec<PathBuf> = g.files.iter().skip(1).cloned().collect();

                        body.row(22.0, |mut row| {
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(format!("{:>3}", i + 1))
                                        .color(theme::TEXT_LO)
                                        .monospace(),
                                );
                            });
                            row.col(|ui| {
                                ui.label(RichText::new(theme::humansize(g.size)).monospace());
                            });
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(format!("×{}", g.files.len()))
                                        .color(theme::ACCENT)
                                        .strong(),
                                );
                            });
                            row.col(|ui| {
                                ui.label(
                                    RichText::new(theme::humansize(savings))
                                        .color(theme::HOT)
                                        .monospace(),
                                );
                            });
                            row.col(|ui| {
                                if acted {
                                    ui.label(
                                        RichText::new("✓ queued").color(theme::ACCENT).small(),
                                    );
                                } else if let Some(k) = &keeper {
                                    ui.horizontal(|ui| {
                                        if ui
                                            .small_button(
                                                RichText::new("🛡 Safe-rename").color(theme::ACCENT),
                                            )
                                            .on_hover_text(
                                                "Append .superdeduper to every dupe. Reversible \
                                             via Unsuperdeduper; nothing deleted.",
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(GroupAction::SafeRenameOthers {
                                                keeper: k.clone(),
                                                dupes: dupes.clone(),
                                            });
                                            table_state.acted.insert(*orig_idx);
                                        }
                                        if ui
                                            .small_button(
                                                RichText::new("♻ Recycle").color(theme::WARN),
                                            )
                                            .on_hover_text(
                                                "Send every dupe to the Recycle Bin. Reversible.",
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(GroupAction::RecycleOthers {
                                                keeper: k.clone(),
                                                dupes: dupes.clone(),
                                            });
                                            table_state.acted.insert(*orig_idx);
                                        }
                                        if ui
                                            .small_button(
                                                RichText::new("🔗 Hardlink").color(theme::COOL),
                                            )
                                            .on_hover_text(
                                                "Replace each dupe with a hardlink to the keeper. \
                                             Frees space without changing paths.",
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(GroupAction::HardlinkOthers {
                                                keeper: k.clone(),
                                                dupes: dupes.clone(),
                                            });
                                            table_state.acted.insert(*orig_idx);
                                        }
                                        if ui
                                            .small_button("📂")
                                            .on_hover_text(
                                                "Open the keeper file with the default app \
                                                 (same as double-clicking it in Explorer).",
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(GroupAction::OpenFile(k.clone()));
                                        }
                                        if ui
                                            .small_button("📁")
                                            .on_hover_text(
                                                "Open the enclosing folder in Explorer.",
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(GroupAction::OpenFolder(k.clone()));
                                        }
                                        if ui
                                            .small_button("🎯")
                                            .on_hover_text(
                                                "Highlight the keeper in its folder (Explorer \
                                                 with the file selected).",
                                            )
                                            .clicked()
                                        {
                                            clicked = Some(GroupAction::Reveal(k.clone()));
                                        }
                                    });
                                }
                            });
                            row.col(|ui| {
                                let label = keeper
                                    .as_ref()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let arrow = if is_open { "▾ " } else { "▸ " };
                                if ui
                                    .selectable_label(false, format!("{}{}", arrow, label))
                                    .clicked()
                                {
                                    if is_open {
                                        table_state.expanded.remove(orig_idx);
                                    } else {
                                        table_state.expanded.insert(*orig_idx);
                                    }
                                }
                            });
                        });

                        if is_open {
                            for (j, p) in g.files.iter().enumerate() {
                                body.row(18.0, |mut row| {
                                    row.col(|_| {});
                                    row.col(|_| {});
                                    row.col(|_| {});
                                    row.col(|_| {});
                                    row.col(|ui| {
                                        let (tag, color) = if j == 0 {
                                            ("keep ", Color32::from_rgb(0x9a, 0xe6, 0xb4))
                                        } else {
                                            ("dupe ", theme::TEXT_LO)
                                        };
                                        let label = ui.label(
                                            RichText::new(tag)
                                                .color(color)
                                                .small()
                                                .monospace()
                                                .strong(),
                                        );
                                        // Hover the keep / dupe tag
                                        // to see why this file got
                                        // its label. Useful when the
                                        // smart picker makes a
                                        // surprising call — the
                                        // breakdown shows every
                                        // signal that fired.
                                        let mtime =
                                            std::fs::metadata(p).and_then(|m| m.modified()).ok();
                                        let s = crate::keep::score_file(p, mtime);
                                        let mut tip =
                                            format!("Smart-keep score: {:+.1}\n", s.total);
                                        if s.breakdown.is_empty() {
                                            tip.push_str("  (no signals fired)\n");
                                        } else {
                                            for (k, v) in &s.breakdown {
                                                tip.push_str(&format!("  {:+5.1}  {}\n", v, k));
                                            }
                                        }
                                        label.on_hover_text(tip);
                                    });
                                    row.col(|ui| {
                                        let color = if j == 0 {
                                            Color32::from_rgb(0x9a, 0xe6, 0xb4)
                                        } else {
                                            theme::TEXT_LO
                                        };
                                        ui.label(
                                            RichText::new(format_path(p))
                                                .color(color)
                                                .monospace()
                                                .small(),
                                        );
                                    });
                                });
                            }
                        }
                    }
                });
        });

    clicked
}

fn format_path(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn group_passes_filter(
    g: &DuplicateGroupSummary,
    drive_root: Option<&Path>,
    reference_roots: &[std::path::PathBuf],
) -> bool {
    // No filter ⇒ everything passes.
    let Some(root) = drive_root else { return true };
    g.files
        .iter()
        .any(|p| p.starts_with(root) || reference_roots.iter().any(|r| p.starts_with(r)))
}
