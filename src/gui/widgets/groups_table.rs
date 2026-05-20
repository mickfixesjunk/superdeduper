//! Sortable list of confirmed duplicate groups. Click a row to expand
//! the file list inline; the keeper (per current `--strategy`) is
//! highlighted.

use egui::{RichText, ScrollArea, Ui};
use egui_extras::{Column, TableBuilder};

use crate::gui::events::DuplicateGroupSummary;
use crate::gui::state::UiState;
use crate::gui::theme;

#[derive(Default)]
pub struct GroupsTableState {
    expanded: hashbrown::HashSet<usize>,
}

pub fn show(ui: &mut Ui, state: &UiState, table_state: &mut GroupsTableState) {
    ui.label(RichText::new("Duplicate groups").color(theme::TEXT_LO).strong());
    ui.add_space(4.0);

    if state.duplicates.is_empty() {
        ui.label(RichText::new("no duplicates yet").color(theme::TEXT_LO).italics());
        return;
    }

    let mut sorted: Vec<(usize, &DuplicateGroupSummary)> = state.duplicates.iter().enumerate().collect();
    sorted.sort_by(|a, b| {
        let sa = a.1.size.saturating_mul(a.1.files.len().saturating_sub(1) as u64);
        let sb = b.1.size.saturating_mul(b.1.files.len().saturating_sub(1) as u64);
        sb.cmp(&sa)
    });

    ScrollArea::vertical().id_source("groups-table").show(ui, |ui| {
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::exact(40.0))
            .column(Column::exact(90.0))
            .column(Column::exact(50.0))
            .column(Column::exact(90.0))
            .column(Column::remainder())
            .header(20.0, |mut h| {
                h.col(|ui| { ui.label(RichText::new("#").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Size").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Copies").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("Reclaim").color(theme::TEXT_LO).small()); });
                h.col(|ui| { ui.label(RichText::new("First path").color(theme::TEXT_LO).small()); });
            })
            .body(|mut body| {
                for (i, (orig_idx, g)) in sorted.iter().enumerate() {
                    let savings = g.size.saturating_mul(g.files.len().saturating_sub(1) as u64);
                    let is_open = table_state.expanded.contains(orig_idx);

                    body.row(20.0, |mut row| {
                        row.col(|ui| {
                            ui.label(RichText::new(format!("{:>3}", i + 1)).color(theme::TEXT_LO).monospace());
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
                            let label = g.files.first().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
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
                            body.row(16.0, |mut row| {
                                row.col(|_| {});
                                row.col(|_| {});
                                row.col(|_| {});
                                row.col(|ui| {
                                    let tag = if j == 0 { "keep" } else { "dupe" };
                                    let color = if j == 0 { theme::ACCENT } else { theme::TEXT_LO };
                                    ui.label(RichText::new(tag).color(color).small().monospace());
                                });
                                row.col(|ui| {
                                    ui.label(
                                        RichText::new(format!("    {}", p.display()))
                                            .color(theme::TEXT_LO)
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
}
