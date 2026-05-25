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
    /// Recycle (send to Recycle Bin) every dupe across every visible
    /// group. Reversible from the OS recycle bin until emptied;
    /// reference paths are never touched. Same destructive-confirm
    /// gate as per-group Recycle.
    RecycleAllVisible,
    /// Permanently delete every dupe across every visible group —
    /// no recycle bin, no undo. Highest-friction destructive action;
    /// requires the standard "type DELETE" confirmation.
    NukeAllVisible,
    /// Promote a non-keeper file to the keeper slot for its group.
    /// The dispatcher swaps `state.duplicates[group_idx].files[0]`
    /// with `files[member_idx]` so every subsequent destructive
    /// action treats the promoted file as protected. `group_idx`
    /// is the index in `UiState::duplicates`; `member_idx` is the
    /// position within that group's `files` vec (must be > 0; the
    /// keeper button doesn't appear on the existing keeper).
    PromoteKeeper { group_idx: usize, member_idx: usize },
}

/// The two bulk-action options the dropdown above the results
/// table can execute. Both operate across every visible duplicate
/// group; the user picks the action, the Go button runs it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BulkAction {
    #[default]
    SafeRenameDupes,
    ArchiveDupes,
    RecycleDupes,
    NukeDupes,
}

impl BulkAction {
    fn label(self) -> &'static str {
        match self {
            BulkAction::SafeRenameDupes => "🛡 Safe-rename dupes",
            BulkAction::ArchiveDupes => "📦 Archive dupes",
            BulkAction::RecycleDupes => "♻ Recycle dupes",
            BulkAction::NukeDupes => "💀 Nuke dupes (permanent)",
        }
    }

    /// `true` for actions that permanently destroy files (no recycle
    /// bin, no .superdeduper rename). The destructive-confirm modal
    /// uses this to decide whether to show the "type DELETE" gate.
    pub fn is_destructive(self) -> bool {
        matches!(self, BulkAction::RecycleDupes | BulkAction::NukeDupes,)
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
    /// "Hide unreclaimable (0 bytes)" filter toggle. When true,
    /// hardlinked groups (link_equivalent + partial-hardlink groups
    /// where unique_inodes < 2) drop out of the visible table. The
    /// per-row "0 B reclaimable" rows still exist in the data model;
    /// only their rendering is hidden.
    pub hide_unreclaimable: bool,
}

/// Render the unfiltered table. Kept for callers that don't need a
/// drive filter (the App always uses `show_filtered`).
pub fn show(
    ui: &mut Ui,
    state: &UiState,
    table_state: &mut GroupsTableState,
    is_scanning: bool,
) -> Option<GroupAction> {
    show_filtered(ui, state, table_state, None, &[], is_scanning)
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
    is_scanning: bool,
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
    // Sort by inode-aware reclaim (biggest actual freeable space
    // first). Path-aware would float hardlink-equivalent groups to
    // the top on hardlink-heavy corpora even though they have 0 B
    // to reclaim — bad UX (the most-clickable row is the least
    // useful one).
    sorted.sort_by(|a, b| {
        let sa = crate::gui::state::inode_aware_savings(a.1);
        let sb = crate::gui::state::inode_aware_savings(b.1);
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
    //
    // Visible-dupe-count must respect the hide-unreclaimable toggle
    // so the Go button label matches what'll actually be acted on
    // (the app's bulk-action workers apply the same filter).
    let visible_dupe_count: usize = sorted
        .iter()
        .filter(|(_, g)| {
            !table_state.hide_unreclaimable || crate::gui::state::inode_aware_savings(g) > 0
        })
        .map(|(_, g)| g.files.len().saturating_sub(1))
        .sum();
    // Bulk-action row: a dropdown for "what to do across every
    // visible group" + a Go button to execute it. Replaces the
    // single Safe-rename-ALL button, and folds in the Archive-dupes
    // button that used to live on the roots panel — both bulk
    // actions now sit in the same place.
    ui.horizontal(|ui| {
        let action_selected = table_state.bulk_action;
        egui::ComboBox::from_id_salt("bulk-action-combo")
            .selected_text(action_selected.label())
            .width(240.0)
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
                ui.selectable_value(
                    &mut table_state.bulk_action,
                    BulkAction::RecycleDupes,
                    BulkAction::RecycleDupes.label(),
                );
                ui.selectable_value(
                    &mut table_state.bulk_action,
                    BulkAction::NukeDupes,
                    BulkAction::NukeDupes.label(),
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
            BulkAction::RecycleDupes => {
                "Send every non-keeper across every visible group to the \
                 OS Recycle Bin. Reference paths never touched. Recoverable \
                 from the recycle bin until you empty it. Requires \
                 confirmation."
            }
            BulkAction::NukeDupes => {
                "PERMANENTLY delete every non-keeper across every visible \
                 group — no recycle bin, no undo. Reference paths never \
                 touched. Requires typing DELETE to confirm. Only use when \
                 you're certain."
            }
        };
        // Gate Go on (a) something to act on AND (b) the scan having
        // finished. Mid-scan, the visible dupe set is still growing
        // and acting on it would leave the engine working against a
        // moving target.
        let go_enabled = visible_dupe_count > 0 && !is_scanning;
        let go_hover = if is_scanning {
            "Wait for the scan to finish before running bulk actions. \
             The duplicate set is still being computed."
        } else {
            hover
        };
        if ui
            .add_enabled(go_enabled, go)
            .on_hover_text(go_hover)
            .clicked()
        {
            clicked = Some(match table_state.bulk_action {
                BulkAction::SafeRenameDupes => GroupAction::SafeRenameAllVisible,
                BulkAction::ArchiveDupes => GroupAction::ArchiveAllVisible,
                BulkAction::RecycleDupes => GroupAction::RecycleAllVisible,
                BulkAction::NukeDupes => GroupAction::NukeAllVisible,
            });
        }

        let trailer = match table_state.bulk_action {
            BulkAction::SafeRenameDupes => "safe-mode (no files deleted)",
            BulkAction::ArchiveDupes => "moves dupes, writes manifest",
            BulkAction::RecycleDupes => "recoverable from recycle bin",
            BulkAction::NukeDupes => "PERMANENT delete — no undo",
        };
        ui.label(
            RichText::new(trailer)
                .color(theme::TEXT_LO)
                .small()
                .italics(),
        );

        // "Hide unreclaimable" toggle on the SAME row as the bulk-
        // action toolbar so it's visually unmissable. Renders as a
        // toggle-button (pressed when active, like a switch) rather
        // than a checkbox so it reads as a filter control.
        let hidden_count = sorted
            .iter()
            .filter(|(_, g)| crate::gui::state::inode_aware_savings(g) == 0)
            .count();
        ui.add_space(12.0);
        let toggle_text = if table_state.hide_unreclaimable {
            format!("🚫 Showing reclaimable only ({hidden_count} hidden)")
        } else if hidden_count > 0 {
            format!("👁 Showing all · {hidden_count} are 0 B")
        } else {
            "👁 Showing all".to_string()
        };
        let toggle_color = if table_state.hide_unreclaimable {
            theme::ACCENT
        } else {
            theme::TEXT_HI
        };
        ui.toggle_value(
            &mut table_state.hide_unreclaimable,
            RichText::new(toggle_text).color(toggle_color).strong(),
        )
        .on_hover_text(
            "Toggle: hide groups whose files are already hardlinked / \
             share storage on disk (nothing to free). Groups stay in \
             the data model; only the table view hides them. Doesn't \
             affect the Reclaimable total in the header.",
        );
    });
    ui.add_space(4.0);

    ScrollArea::vertical()
        .id_salt("groups-table")
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
                        // "Hide unreclaimable" toggle: skip groups
                        // whose inode-aware savings is 0
                        // (link_equivalent or partial-hardlink with
                        // unique_inodes < 2). Data still in
                        // state.duplicates; only this view skips.
                        if table_state.hide_unreclaimable
                            && crate::gui::state::inode_aware_savings(g) == 0
                        {
                            continue;
                        }
                        // Inode-aware reclaim per row — for partial-
                        // hardlink groups (some aliases of inode A +
                        // some genuine copies), counts (unique_inodes
                        // - 1) * size rather than (path_count - 1) *
                        // size. Falls back to path-aware when
                        // unique_inodes==0 (older checkpoint format).
                        // link_equivalent groups read 0 by definition.
                        let savings = crate::gui::state::inode_aware_savings(g);
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
                                use crate::pipeline::SimilarityKind;
                                match g.similarity_kind {
                                    SimilarityKind::PerceptualImage => {
                                        // Tier-4 perceptual image group —
                                        // bytes differ but the images LOOK
                                        // alike. Surface distinctly so
                                        // the user reviews carefully.
                                        ui.label(
                                            RichText::new("🖼 perceptual image")
                                                .color(theme::WARN)
                                                .small()
                                                .strong(),
                                        )
                                        .on_hover_text(
                                            "These files look perceptually similar (resize, \
                                             format-conversion, light-edit twins) but their \
                                             bytes differ. Review side-by-side before deleting \
                                             — perceptual-similar isn't byte-identical.",
                                        );
                                    }
                                    SimilarityKind::PerceptualAudio => {
                                        // Tier-4 perceptual audio (#26 v2) — same
                                        // 'review carefully' framing but with the
                                        // music-note glyph + audio-flavoured tooltip.
                                        ui.label(
                                            RichText::new("🎵 perceptual audio")
                                                .color(theme::WARN)
                                                .small()
                                                .strong(),
                                        )
                                        .on_hover_text(
                                            "These files sound acoustically similar (re-encode, \
                                             bitrate / codec change, modest editing) but their \
                                             bytes differ. Listen to a few seconds of each \
                                             before deleting — perceptual-similar isn't \
                                             byte-identical.",
                                        );
                                    }
                                    SimilarityKind::ByteIdentical => {
                                        // No special marker — falls through
                                        // to the link-equivalent / acted /
                                        // keeper-button branches below.
                                    }
                                }
                                if g.link_equivalent
                                    && !matches!(
                                        g.similarity_kind,
                                        SimilarityKind::PerceptualImage
                                            | SimilarityKind::PerceptualAudio
                                    )
                                {
                                    // Already-hardlinked groups have no
                                    // reclaimable space — surface that
                                    // distinctly so the user knows
                                    // these aren't worth acting on.
                                    ui.label(
                                        RichText::new("🔗 already hardlinked")
                                            .color(theme::COOL)
                                            .small()
                                            .strong(),
                                    )
                                    .on_hover_text(
                                        "Every file in this group is a hardlink to the same \
                                         data on disk. They occupy ONE copy worth of bytes, \
                                         not N — Reclaimable correctly shows 0. Recycle/Safe-rename \
                                         would still work but won't free any space.",
                                    );
                                } else if acted {
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
                                    row.col(|ui| {
                                        // Per-dupe action column: a
                                        // "Make keeper" button on
                                        // non-keeper rows. Lets the
                                        // user override the smart
                                        // picker when they know which
                                        // copy they want to protect.
                                        // Disabled mid-scan (Settings,
                                        // Go etc. are too) AND when
                                        // this group is in the
                                        // `acted` set (action queued).
                                        if j > 0 && !acted {
                                            let btn = egui::Button::new(
                                                RichText::new("👑")
                                                    .color(theme::WARN)
                                                    .small(),
                                            )
                                            .frame(false)
                                            .min_size(egui::vec2(20.0, 16.0));
                                            let resp = ui
                                                .add_enabled(!is_scanning, btn)
                                                .on_hover_text(
                                                    "Make this the keeper — \
                                                     promote this file to the \
                                                     protected slot in the \
                                                     group. The current keeper \
                                                     becomes a dupe.",
                                                );
                                            if resp.clicked() {
                                                clicked = Some(GroupAction::PromoteKeeper {
                                                    group_idx: *orig_idx,
                                                    member_idx: j,
                                                });
                                            }
                                        }
                                    });
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

/// User-facing path display. Strips Windows verbatim-path prefix
/// (`\\?\`) so dup-table rows + tooltips show `C:\Foo\bar` instead of
/// `\\?\C:\Foo\bar`. UNC verbatim form (`\\?\UNC\srv\share`) is
/// rewritten back to `\\srv\share`.
fn format_path(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if let Some(unc) = rest.strip_prefix("UNC\\") {
            return format!(r"\\{unc}");
        }
        return rest.to_string();
    }
    s.into_owned()
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

#[cfg(test)]
mod path_display_tests {
    use super::*;

    #[test]
    fn drops_verbatim_drive_prefix() {
        let p = Path::new(r"\\?\C:\Windows\System32\notepad.exe");
        assert_eq!(format_path(p), r"C:\Windows\System32\notepad.exe");
    }

    #[test]
    fn drops_verbatim_drive_prefix_with_lowercase_drive() {
        // Some Windows APIs return lowercase drive letters in the
        // verbatim form. Stripping the prefix must not change the
        // case of what's underneath.
        let p = Path::new(r"\\?\c:\foo\bar.txt");
        assert_eq!(format_path(p), r"c:\foo\bar.txt");
    }

    #[test]
    fn rewrites_verbatim_unc_form() {
        // \\?\UNC\server\share\file -> \\server\share\file
        let p = Path::new(r"\\?\UNC\fileserver\public\report.pdf");
        assert_eq!(format_path(p), r"\\fileserver\public\report.pdf");
    }

    #[test]
    fn passes_through_normal_windows_path() {
        let p = Path::new(r"C:\Users\Mick\Documents\thing.txt");
        assert_eq!(format_path(p), r"C:\Users\Mick\Documents\thing.txt");
    }

    #[test]
    fn passes_through_normal_unc_path() {
        // Already-displayable UNC (no \\?\ prefix) stays put.
        let p = Path::new(r"\\fileserver\share\thing");
        assert_eq!(format_path(p), r"\\fileserver\share\thing");
    }

    #[test]
    fn passes_through_unix_paths() {
        // Non-Windows paths trip neither branch — engine uses
        // format_path on the Log tab for cross-platform display.
        let p = Path::new("/home/neomatrix/file.bin");
        assert_eq!(format_path(p), "/home/neomatrix/file.bin");
    }

    #[test]
    fn handles_root_verbatim_path() {
        // Edge case: just the prefix + drive root. Should produce
        // just the drive root, not crash.
        let p = Path::new(r"\\?\D:\");
        assert_eq!(format_path(p), r"D:\");
    }

    #[test]
    fn empty_path_passes_through() {
        let p = Path::new("");
        assert_eq!(format_path(p), "");
    }
}
