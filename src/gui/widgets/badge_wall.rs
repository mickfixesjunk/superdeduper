//! Badge-wall panel — always-visible bottom-left achievements grid.
//!
//! Per client-spec §10.4. Renders the full achievement catalog as a
//! grid of badge tiles, greyed-out by default + colorized when
//! granted, with a lifetime-stats headline above. Below ~900 px window
//! width it auto-degrades to the §10.5 sidebar mini-widget.
//!
//! Data source: `crate::leaderboard::catalog` (catalog fetched at app
//! start, profile fetched if registered). The widget reads from the
//! global slot each frame; no per-widget state beyond UI ephemera.

#![cfg(feature = "telemetry")]

use std::collections::HashMap;

use egui::{Color32, RichText};

use crate::gui::theme;
use crate::leaderboard::catalog::{CatalogEntry, CatalogState};

/// Window-width breakpoint per design's §10.4 narrow-mode rule.
/// Below this, the full grid auto-degrades to the §10.5 mini-widget.
pub const NARROW_MODE_BREAKPOINT: f32 = 900.0;

/// Action a click on the badge wall triggered. Caller is the app's
/// per-frame render loop; routes the action through the rest of the
/// UI (open the catalog tile detail modal, fire the profile link,
/// open Settings to the Leaderboard tab, etc.).
#[derive(Debug, Clone)]
pub enum BadgeWallAction {
    /// User clicked a granted or greyed tile — surface the detail
    /// popup (grant date / unlock condition).
    TileClicked(String),
    /// User clicked the lifetime-stats header. Opens the live
    /// profile URL in the browser.
    OpenProfile,
    /// User clicked the "Register" link in the empty-state panel.
    /// Caller pops the Settings modal at the Leaderboard tab.
    OpenRegister,
}

/// Render the badge wall inside the caller's UI region. Returns
/// `Some(action)` if a click happened this frame.
pub fn show(ui: &mut egui::Ui, state: &CatalogState) -> Option<BadgeWallAction> {
    let mut action: Option<BadgeWallAction> = None;
    ui.vertical(|ui| {
        render_header(ui, state, &mut action);
        ui.add_space(6.0);
        render_grid(ui, state, &mut action);
    });
    action
}

/// Compact alternative for narrow windows (per §10.5). Same
/// data source, smaller renderer: lifetime headline + N-badges
/// counter, plus a click target that opens the full panel /
/// profile.
pub fn show_mini(ui: &mut egui::Ui, state: &CatalogState) -> Option<BadgeWallAction> {
    let mut action: Option<BadgeWallAction> = None;
    let (granted_count, total_count) = count_grants(state);
    let lifetime_human = lifetime_reclaimed_human(state);
    ui.vertical(|ui| {
        ui.label(
            RichText::new("Leaderboard")
                .color(theme::TEXT_HI)
                .strong()
                .small(),
        );
        ui.label(
            RichText::new(lifetime_human)
                .color(theme::ACCENT)
                .strong(),
        );
        let badges_line = match total_count {
            0 => format!("{granted_count} badges"),
            _ => format!("{granted_count} / {total_count} badges"),
        };
        ui.label(RichText::new(badges_line).color(theme::TEXT_LO).small());
        if ui
            .link(RichText::new("Open profile →").color(theme::ACCENT).small())
            .clicked()
        {
            action = Some(BadgeWallAction::OpenProfile);
        }
    });
    action
}

fn render_header(
    ui: &mut egui::Ui,
    state: &CatalogState,
    action: &mut Option<BadgeWallAction>,
) {
    let (granted_count, total_count) = count_grants(state);
    let lifetime_human = lifetime_reclaimed_human(state);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(lifetime_human)
                .color(theme::ACCENT)
                .strong()
                .size(15.0),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new("reclaimed lifetime")
                .color(theme::TEXT_LO)
                .small(),
        );
    });
    ui.horizontal(|ui| {
        let badges_line = match total_count {
            0 => format!("{granted_count} badges"),
            _ => format!("{granted_count} / {total_count} badges"),
        };
        ui.label(RichText::new(badges_line).color(theme::TEXT_HI).small());
        ui.add_space(8.0);
        if ui
            .link(
                RichText::new("View profile →")
                    .color(theme::ACCENT)
                    .small(),
            )
            .clicked()
        {
            *action = Some(BadgeWallAction::OpenProfile);
        }
    });
}

fn render_grid(
    ui: &mut egui::Ui,
    state: &CatalogState,
    action: &mut Option<BadgeWallAction>,
) {
    let catalog = match state.catalog.as_ref() {
        Some(Ok(c)) => c,
        Some(Err(e)) => {
            ui.label(
                RichText::new(format!("catalog fetch failed: {e}"))
                    .color(theme::HOT)
                    .small()
                    .italics(),
            );
            ui.label(
                RichText::new("Retries on next app start.")
                    .color(theme::TEXT_LO)
                    .small(),
            );
            return;
        }
        None => {
            ui.spinner();
            ui.label(
                RichText::new("loading catalog…")
                    .color(theme::TEXT_LO)
                    .small(),
            );
            return;
        }
    };

    // Build the grant lookup once per render. Cheap (~35 entries
    // today; backend may grow to ~100). HashMap lookup is O(1).
    let grants: HashMap<&str, bool> = match state.profile.as_ref() {
        Some(Ok(p)) => p
            .achievements
            .iter()
            .map(|g| (g.achievement_id.as_str(), g.granted))
            .collect(),
        _ => HashMap::new(),
    };

    // Sort by display_order. Catalog endpoint already returns sorted
    // but the .sort_by_key() here makes the contract local.
    let mut entries: Vec<&CatalogEntry> = catalog.achievements.iter().collect();
    entries.sort_by_key(|e| e.display_order);

    // 3-column grid. With ~35 entries this gives 12 rows. Could go to
    // 4-col on wider windows; sticking with 3 for the bottom-left
    // panel's intended ~280-360px width.
    const COLS: usize = 3;
    const TILE_SIZE: egui::Vec2 = egui::vec2(76.0, 76.0);
    egui::ScrollArea::vertical()
        .max_height(360.0)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                for (i, entry) in entries.iter().enumerate() {
                    let granted = grants.get(entry.id.as_str()).copied().unwrap_or(false);
                    if render_tile(ui, entry, granted, TILE_SIZE) {
                        *action = Some(BadgeWallAction::TileClicked(entry.id.clone()));
                    }
                    if (i + 1) % COLS == 0 {
                        ui.end_row();
                    }
                }
            });
        });
}

/// Render a single tile. Returns `true` if clicked this frame.
fn render_tile(
    ui: &mut egui::Ui,
    entry: &CatalogEntry,
    granted: bool,
    size: egui::Vec2,
) -> bool {
    let (fill, stroke_color, text_color) = if granted {
        match entry.tier.as_str() {
            "high" => (theme::ACCENT, theme::PANEL_DEEP, theme::PANEL_DEEP),
            "mid" => (theme::ACCENT_DIM, theme::TEXT_HI, theme::TEXT_HI),
            _ => (theme::PANEL_DEEP, theme::ACCENT, theme::TEXT_HI),
        }
    } else {
        // Greyed-out: muted panel fill + faded text. Reads as
        // "exists, not yet earned" rather than "broken / missing."
        (theme::PANEL_DEEP, Color32::from_gray(60), Color32::from_gray(120))
    };

    let resp = egui::Frame::none()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke_color))
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(6.0, 6.0))
        .show(ui, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            ui.vertical_centered(|ui| {
                // Tier icon as the "badge glyph." Bronze / silver /
                // gold per tier. Greyed when not granted (single
                // muted icon to avoid "spoiler" of tier rank).
                let glyph = match (granted, entry.tier.as_str()) {
                    (true, "high") => "★",
                    (true, "mid") => "◆",
                    (true, _) => "●",
                    (false, _) => "○",
                };
                ui.label(
                    RichText::new(glyph)
                        .color(text_color)
                        .strong()
                        .size(22.0),
                );
                ui.label(
                    RichText::new(short_name(&entry.name))
                        .color(text_color)
                        .size(9.5),
                );
            });
        })
        .response;
    let resp = resp.interact(egui::Sense::click());
    let tooltip = if granted {
        format!("{}\n\n{}", entry.name, entry.description)
    } else {
        format!(
            "{} (not yet earned)\n\nUnlock: {}",
            entry.name, entry.description,
        )
    };
    resp.on_hover_text(tooltip).clicked()
}

/// Trim long names so they fit two lines in a 76 px tile without
/// being obnoxiously truncated. Most names are already short
/// ("Tidy-up", "Brisk"); the prefix-heavy ones ("Pathfinder: Dev
/// Drive", "Sub-Minute Club (corpus-v1 50 GB)") get the prefix
/// chopped.
fn short_name(name: &str) -> String {
    if let Some((_, rest)) = name.split_once(": ") {
        return rest.to_string();
    }
    if let Some((head, _)) = name.split_once(" (") {
        return head.to_string();
    }
    name.to_string()
}

fn count_grants(state: &CatalogState) -> (u32, u32) {
    let total = state
        .catalog
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|c| c.achievements.len() as u32)
        .unwrap_or(0);
    let granted = state
        .profile
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|p| p.achievements.iter().filter(|g| g.granted).count() as u32)
        .unwrap_or(0);
    (granted, total)
}

fn lifetime_reclaimed_human(state: &CatalogState) -> String {
    let bytes = state
        .profile
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|p| p.lifetime_reclaimed_bytes)
        .unwrap_or(0);
    theme::humansize(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaderboard::catalog::{Catalog, CatalogEntry, Profile, ProfileGrant};

    fn entry(id: &str, name: &str, tier: &str, order: i32) -> CatalogEntry {
        CatalogEntry {
            id: id.to_string(),
            name: name.to_string(),
            description: "d".to_string(),
            tier: tier.to_string(),
            unlock_kind: "single-run".to_string(),
            display_order: order,
        }
    }

    #[test]
    fn short_name_strips_pathfinder_prefix() {
        assert_eq!(short_name("Pathfinder: ReFS"), "ReFS");
        assert_eq!(short_name("Sub-Minute Club (corpus-v1 50 GB)"), "Sub-Minute Club");
        assert_eq!(short_name("Tidy-up"), "Tidy-up");
    }

    #[test]
    fn count_grants_handles_missing_profile() {
        let state = CatalogState {
            catalog: Some(Ok(Catalog {
                version: "v1".into(),
                achievements: vec![entry("a", "A", "low", 1)],
            })),
            profile: None,
        };
        assert_eq!(count_grants(&state), (0, 1));
    }

    #[test]
    fn count_grants_counts_only_granted() {
        let state = CatalogState {
            catalog: Some(Ok(Catalog {
                version: "v1".into(),
                achievements: vec![
                    entry("a", "A", "low", 1),
                    entry("b", "B", "low", 2),
                ],
            })),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime_reclaimed_bytes: 0,
                lifetime_scans: 0,
                achievements: vec![
                    ProfileGrant {
                        achievement_id: "a".into(),
                        granted: true,
                        granted_at: None,
                    },
                    ProfileGrant {
                        achievement_id: "b".into(),
                        granted: false,
                        granted_at: None,
                    },
                ],
            })),
        };
        assert_eq!(count_grants(&state), (1, 2));
    }
}
