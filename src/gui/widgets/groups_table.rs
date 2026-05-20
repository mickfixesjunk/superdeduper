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
    RecycleOthers { keeper: PathBuf, dupes: Vec<PathBuf> },
    /// Replace every dupe with a hardlink to the keeper (same volume).
    HardlinkOthers { keeper: PathBuf, dupes: Vec<PathBuf> },
    /// Open the keeper's containing folder in Explorer.
    Reveal(PathBuf),
}

#[derive(Default)]
pub struct GroupsTableState {
    expanded: hashbrown::HashSet<usize>,
    /// Records "this group has been acted on" so we hide the buttons
    /// after the action is queued (prevents double-clicks before the
    /// UI re-syncs).
    acted: hashbrown::HashSet<usize>,
}

/// Render the table. Returns the action the user clicked this frame,
/// if any — caller dispatches it to the engine.
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    table_state: &mut GroupsTableState,
) -> Option<GroupAction> {
    let mut clicked: Option<GroupAction> = None;

    ui.label(RichText::new("Duplicate groups").color(theme::TEXT_LO).strong());
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

    let mut sorted: Vec<(usize, &DuplicateGroupSummary)> =
        state.duplicates.iter().enumerate().collect();
    sorted.sort_by(|a, b| {
        let sa = a.1.size.saturating_mul(a.1.files.len().saturating_sub(1) as u64);
        let sb = b.1.size.saturating_mul(b.1.files.len().saturating_sub(1) as u64);
        sb.cmp(&sa)
    });

    ScrollArea::vertical().id_source("groups-table").show(ui, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::exact(36.0))
            .column(Column::exact(90.0))
            .column(Column::exact(60.0))
            .column(Column::exact(90.0))
            .column(Column::exact(220.0))
            .column(Column::remainder())
            .header(20.0, |mut h| {
                h.col(|ui| { ui.label(RichText::new("#").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Size").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Copies").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Reclaim").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Actions").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Keeper path").color(theme::TEXT_LO).small()); });
            })
            .body(|mut body| {
                for (i, (orig_idx, g)) in sorted.iter().enumerate() {
                    let savings = g.size.saturating_mul(g.files.len().saturating_sub(1) as u64);
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
                                    RichText::new("✓ queued")
                                        .color(theme::ACCENT)
                                        .small(),
                                );
                            } else if let Some(k) = &keeper {
                                ui.horizontal(|ui| {
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
                                        .on_hover_text("Open the keeper in Explorer.")
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
                                    ui.label(
                                        RichText::new(tag)
                                            .color(color)
                                            .small()
                                            .monospace()
                                            .strong(),
                                    );
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
