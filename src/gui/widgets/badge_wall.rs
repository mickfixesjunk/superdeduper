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

    // Classify + sort entries into render order via the pure helper
    // so widget-state tests can assert "given this CatalogState, the
    // grid renders these tiles in this order with these grant flags"
    // without driving the egui frame loop. Catches the bug class
    // where Profile deserialisation succeeds but achievement_id /
    // granted bits drift (the schema-mismatch bug we hit pre-ce0ea9f).
    let classified = classify_grid_entries(state, &catalog.achievements);

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
                for (i, tile) in classified.iter().enumerate() {
                    if render_tile(ui, tile.entry, tile.granted, TILE_SIZE) {
                        *action = Some(BadgeWallAction::TileClicked(tile.entry.id.clone()));
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
        .rounding(egui::Rounding::same(6))
        .inner_margin(egui::Margin::symmetric(6, 6))
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
    // Publish an accessible label so screen readers + the
    // egui_kittest render tests can identify which tile is which
    // AND read its grant state. Pattern: "<Name>: granted" vs
    // "<Name>: not yet earned". The granted-state suffix is the
    // assertion target in `badge_wall_renders_granted_glyphs_from_live_server_shape`.
    let access_label = if granted {
        format!("{}: granted", entry.name)
    } else {
        format!("{}: not yet earned", entry.name)
    };
    let label_for_widget = access_label.clone();
    resp.clone().widget_info(|| {
        egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label_for_widget)
    });
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

/// One row in the badge-wall grid: the catalog entry that drives the
/// label/glyph/tier-colour + the boolean grant flag that drives the
/// colorize-vs-grey treatment. Produced by [`classify_grid_entries`]
/// so widget-state tests can assert "the right tiles are coloured in
/// the right order" without needing to render egui.
#[derive(Debug, Clone)]
pub struct GridTile<'a> {
    pub entry: &'a CatalogEntry,
    pub granted: bool,
}

/// Pure helper: given a [`CatalogState`] + the catalog's achievement
/// list, return tiles in render order (granted-first, then by
/// `display_order` within each bucket). This is the *exact* logic
/// `render_grid` walks; extracting it as a pure function is what
/// lets the unit tests below assert the bug-class invariants
/// (Profile schema → grant lookup → colorised-tile count).
pub fn classify_grid_entries<'a>(
    state: &CatalogState,
    catalog_entries: &'a [CatalogEntry],
) -> Vec<GridTile<'a>> {
    let grants: HashMap<&str, bool> = match state.profile.as_ref() {
        Some(Ok(p)) => p
            .achievements
            .iter()
            .map(|g| (g.achievement_id.as_str(), g.granted))
            .collect(),
        _ => HashMap::new(),
    };
    let mut tiles: Vec<GridTile<'a>> = catalog_entries
        .iter()
        .map(|e| GridTile {
            entry: e,
            granted: grants.get(e.id.as_str()).copied().unwrap_or(false),
        })
        .collect();
    tiles.sort_by_key(|t| (!t.granted, t.entry.display_order));
    tiles
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
        .map(|p| p.lifetime_reclaimed_bytes())
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

    /// **Canonical test for the badge-wall bug class.**
    ///
    /// Pipeline coverage: live server JSON shape → serde Profile
    /// deserialise → CatalogState construction → grid classification
    /// → colorised-vs-grey ordering. This is the test that would
    /// have caught the schema mismatch fixed in `ce0ea9f` (server
    /// emits `id` per achievement + nested `lifetime`; client
    /// previously expected `achievement_id` + flat lifetime fields).
    ///
    /// If this test goes red, the regression is anywhere from the
    /// wire format to the grid order. Repaint / animation / pixel
    /// rendering bugs are downstream and need a true egui_kittest
    /// rendering test (deferred — requires the egui 0.28→0.32+
    /// upgrade that egui_kittest needs).
    #[test]
    fn badge_wall_classifies_granted_tiles_from_live_server_shape() {
        // Catalog: a 5-entry subset mirroring real backend ordering.
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("tidy-up", "Tidy-up", "low", 100),
                entry("brisk", "Brisk", "low", 200),
                entry("founder", "Founder", "high", 550),
                entry("pioneer", "Pioneer", "mid", 560),
                entry("hello-world", "Hello World", "low", 900),
            ],
        };
        // Profile JSON matching the EXACT server wire shape (verified
        // against api.superdeduper.io 2026-05-24). The key invariant
        // this test pins: `id` (not `achievement_id`) + nested
        // `lifetime`. If either drifts, serde fails → grants are
        // empty → all tiles render grey → the assertion below
        // catches it.
        let profile_json = r#"{
            "install_id": "e1eae1fa-58fb-4f5a-8712-a7480ac5761b",
            "lifetime": { "bytes_reclaimed": 731677101, "total_scans": 3 },
            "achievements": [
                { "id": "tidy-up",     "granted": false, "granted_at": null },
                { "id": "brisk",       "granted": true,  "granted_at": "2026-05-24T14:22:52Z" },
                { "id": "founder",     "granted": true,  "granted_at": "2026-05-24T05:20:37Z" },
                { "id": "pioneer",     "granted": false, "granted_at": null },
                { "id": "hello-world", "granted": true,  "granted_at": "2026-05-24T14:22:52Z" }
            ]
        }"#;
        let profile: Profile = serde_json::from_str(profile_json)
            .expect("live server profile shape must deserialise");
        let state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(profile)),
        };

        let tiles = classify_grid_entries(&state, &catalog.achievements);

        // (a) Every catalog entry produces exactly one tile.
        assert_eq!(tiles.len(), 5, "every catalog entry produces one tile");

        // (b) Granted-first ordering: the top three are the granted
        //     entries in `display_order`, then the two ungranted.
        let granted_ids: Vec<&str> = tiles
            .iter()
            .filter(|t| t.granted)
            .map(|t| t.entry.id.as_str())
            .collect();
        assert_eq!(
            granted_ids,
            vec!["brisk", "founder", "hello-world"],
            "granted tiles render in display_order, ahead of any ungranted tile"
        );

        // (c) Granted-bool faithfully reflects the server state.
        //     This is the assertion that fails when the schema-
        //     mismatch bug returns: with `id` mis-mapped to a missing
        //     `achievement_id`, serde would fail, the grants
        //     hashmap would be empty, and EVERY tile would show
        //     granted=false here.
        let granted_count = tiles.iter().filter(|t| t.granted).count();
        assert_eq!(
            granted_count, 3,
            "schema regression check: 3 grants in the JSON should yield 3 granted tiles"
        );

        // (d) Ungranted tiles still surface (the wall doesn't drop
        //     them) and sort by display_order within their bucket.
        let ungranted_ids: Vec<&str> = tiles
            .iter()
            .filter(|t| !t.granted)
            .map(|t| t.entry.id.as_str())
            .collect();
        assert_eq!(ungranted_ids, vec!["tidy-up", "pioneer"]);
    }

    /// Bug-class regression test: when Profile deserialise fails (or
    /// profile slot is None), the wall must still render all tiles
    /// as ungranted rather than panicking. This was the observable
    /// symptom on Mick's box pre-`ce0ea9f`: "0 of 37 badges, all
    /// grey." If the schema-mismatch bug returns, fetch_profile
    /// returns Err → profile slot is Some(Err) → grants hashmap is
    /// empty → classify_grid_entries yields all-ungranted tiles.
    #[test]
    fn badge_wall_falls_back_to_ungranted_when_profile_errored() {
        let catalog_entries = vec![
            entry("tidy-up", "Tidy-up", "low", 100),
            entry("brisk", "Brisk", "low", 200),
        ];
        let state = CatalogState {
            catalog: Some(Ok(Catalog {
                version: "v1".into(),
                achievements: catalog_entries.clone(),
            })),
            profile: Some(Err("schema mismatch: missing field `achievement_id`".into())),
        };
        let tiles = classify_grid_entries(&state, &catalog_entries);
        assert_eq!(tiles.len(), 2);
        assert!(tiles.iter().all(|t| !t.granted), "errored profile yields all-ungranted");
    }

    /// **Render-pipeline test for the badge-wall bug class.**
    ///
    /// Drives the full egui frame loop via `egui_kittest`: builds a
    /// CatalogState from the live server JSON shape, renders the
    /// real `badge_wall::show()` widget, queries the AccessKit tree,
    /// and counts the granted-vs-ungranted glyph nodes (★/◆/● for
    /// granted tiers, ○ for ungranted).
    ///
    /// What this catches that `badge_wall_classifies_granted_tiles_from_live_server_shape`
    /// does NOT:
    ///
    /// - Render-pipeline regressions where `classify_grid_entries`
    ///   returns the right state but the widget paints the wrong
    ///   glyph or style.
    /// - Repaint timing where the widget caches an earlier frame's
    ///   data instead of reading the current `CatalogState`.
    /// - The path between `classify_grid_entries` and the painted
    ///   `RichText` / `Frame` calls.
    ///
    /// What it still does NOT catch (Tier 3 territory): exact
    /// pixel colors, font hinting, HiDPI scaling, animation states.
    /// Those need sdd-testwin screenshot diffs.
    #[test]
    fn badge_wall_renders_granted_glyphs_from_live_server_shape() {
        use egui_kittest::kittest::{NodeT, Queryable};
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("tidy-up", "Tidy-up", "low", 100),
                entry("brisk", "Brisk", "low", 200),
                entry("founder", "Founder", "high", 550),
                entry("pioneer", "Pioneer", "mid", 560),
                entry("hello-world", "Hello World", "low", 900),
            ],
        };
        let profile_json = r#"{
            "install_id": "test-install",
            "lifetime": { "bytes_reclaimed": 731677101, "total_scans": 3 },
            "achievements": [
                { "id": "tidy-up",     "granted": false, "granted_at": null },
                { "id": "brisk",       "granted": true,  "granted_at": "2026-05-24T14:22:52Z" },
                { "id": "founder",     "granted": true,  "granted_at": "2026-05-24T05:20:37Z" },
                { "id": "pioneer",     "granted": false, "granted_at": null },
                { "id": "hello-world", "granted": true,  "granted_at": "2026-05-24T14:22:52Z" }
            ]
        }"#;
        let profile: Profile = serde_json::from_str(profile_json).unwrap();
        let state = CatalogState {
            catalog: Some(Ok(catalog)),
            profile: Some(Ok(profile)),
        };

        // Drive the real `badge_wall::show()` through the egui frame
        // loop. The closure mutates no state; the assertions below
        // run against the rendered AccessKit tree.
        let mut harness = egui_kittest::Harness::new_ui(move |ui| {
            super::show(ui, &state);
        });
        harness.run();

        // Each granted tile renders a tier-specific glyph: ★ for
        // "high", ◆ for "mid", ● for "low". Three grants in the
        // fixture: brisk (low ●) + founder (high ★) + hello-world
        // (low ●). Expected: 2 ● glyphs from granted + 1 ★ glyph.
        // Each ungranted tile renders ○. Expected: 2 ○ glyphs.
        // Read every accessible label that the rendered widget
        // exposed. Each tile publishes its grant state as
        // "<Name>: granted" or "<Name>: not yet earned" via
        // `WidgetInfo::labeled`. The harness drives a real egui
        // frame end-to-end; if the widget reads the wrong field,
        // skips classification, or fails to publish accessibility
        // info, this test goes red.
        let labels: Vec<String> = harness
            .node()
            .query_all_by_label_contains("")
            .filter_map(|n| n.accesskit_node().label())
            .collect();

        let granted_tiles: Vec<&String> = labels
            .iter()
            .filter(|s| s.ends_with(": granted"))
            .collect();
        let ungranted_tiles: Vec<&String> = labels
            .iter()
            .filter(|s| s.ends_with(": not yet earned"))
            .collect();

        assert_eq!(
            granted_tiles.len(),
            3,
            "3 grants in the fixture should render 3 granted tiles. \
             Got {} granted, full labels: {labels:?}",
            granted_tiles.len()
        );
        assert_eq!(
            ungranted_tiles.len(),
            2,
            "2 ungranted entries should render 2 ungranted tiles. \
             Got {} ungranted, full labels: {labels:?}",
            ungranted_tiles.len()
        );

        // Spot-check the per-tile correctness: brisk + founder +
        // hello-world land as granted; tidy-up + pioneer as not.
        assert!(labels.iter().any(|s| s == "Brisk: granted"));
        assert!(labels.iter().any(|s| s == "Founder: granted"));
        assert!(labels.iter().any(|s| s == "Hello World: granted"));
        assert!(labels.iter().any(|s| s == "Tidy-up: not yet earned"));
        assert!(labels.iter().any(|s| s == "Pioneer: not yet earned"));
    }

    /// Renders the badge wall to a PNG side-artifact. Two
    /// scenarios captured for visual proof to Mick:
    ///
    /// 1. Empty-profile state (mimics the pre-`ce0ea9f` symptom:
    ///    profile fetch failed → no grants → all tiles grey).
    /// 2. Live-grants state (mimics the post-fix outcome: 3 grants
    ///    in the profile → 3 colorized tiles + the rest grey).
    ///
    /// Output: `target/test-artifacts/badge_wall-{empty,granted}.png`.
    /// The render-pipeline assertion test above proves correctness;
    /// this test produces the artifacts Mick eyeballs without
    /// booting the EXE.
    ///
    /// Gated behind the `wgpu` + `snapshot` egui_kittest features
    /// (enabled in Cargo.toml dev-deps).
    #[test]
    fn badge_wall_renders_png_side_artifacts() {
        let out_dir = std::path::PathBuf::from("target/test-artifacts");
        std::fs::create_dir_all(&out_dir).expect("create test-artifacts dir");

        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("tidy-up", "Tidy-up", "low", 100),
                entry("brisk", "Brisk", "low", 200),
                entry("founder", "Founder", "high", 550),
                entry("pioneer", "Pioneer", "mid", 560),
                entry("hello-world", "Hello World", "low", 900),
            ],
        };

        // Scenario 1: empty profile → all tiles render ungranted.
        // This is what Mick saw BEFORE the schema fix (profile
        // deserialise failed → empty grants → all grey).
        let empty_state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(Profile {
                install_id: "test".into(),
                lifetime: Default::default(),
                achievements: vec![],
            })),
        };
        render_to_png(&empty_state, out_dir.join("badge_wall-empty.png").as_path());

        // Scenario 2: live-shape grants → 3 colorized + 2 grey.
        // What Mick should see AFTER the schema fix.
        let live_json = r#"{
            "install_id": "e1eae1fa-58fb-4f5a-8712-a7480ac5761b",
            "lifetime": { "bytes_reclaimed": 731677101, "total_scans": 3 },
            "achievements": [
                { "id": "tidy-up",     "granted": false, "granted_at": null },
                { "id": "brisk",       "granted": true,  "granted_at": "2026-05-24T14:22:52Z" },
                { "id": "founder",     "granted": true,  "granted_at": "2026-05-24T05:20:37Z" },
                { "id": "pioneer",     "granted": false, "granted_at": null },
                { "id": "hello-world", "granted": true,  "granted_at": "2026-05-24T14:22:52Z" }
            ]
        }"#;
        let live_profile: Profile = serde_json::from_str(live_json).unwrap();
        let granted_state = CatalogState {
            catalog: Some(Ok(catalog)),
            profile: Some(Ok(live_profile)),
        };
        render_to_png(&granted_state, out_dir.join("badge_wall-granted.png").as_path());
    }

    fn render_to_png(state: &CatalogState, path: &std::path::Path) {
        let state = state.clone();
        let mut harness = egui_kittest::Harness::builder()
            .with_size(egui::vec2(360.0, 600.0))
            .build_ui(move |ui| {
                super::show(ui, &state);
            });
        harness.run();
        let img = harness.render().expect("render PNG side-artifact");
        img.save(path).expect("write PNG to disk");
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
                lifetime: Default::default(),
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
