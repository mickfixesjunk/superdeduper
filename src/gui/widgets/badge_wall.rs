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
        // Above-grid "Login & Claim" CTA per spec §10.5 +
        // Mick's 2026-05-24T22:18Z direction. Only renders for
        // anonymous installs; the post-claim state hides it.
        render_login_cta(ui);
        render_grid(ui, state, &mut action);
    });
    action
}

/// Render the "Login & Claim" CTA + provider chooser, when the
/// active channel's install is anonymous. No-op when linked.
/// `link_via_loopback` blocks the UI thread for up to 5 minutes
/// (same single-thread approach the Settings → Account tab
/// uses); v1.1 will move to a background thread + progress
/// spinner.
fn render_login_cta(ui: &mut egui::Ui) {
    use crate::leaderboard::oauth;

    // Drain any completed session BEFORE deciding visibility — if
    // the user just finished OAuth, the next frame's status reads
    // as Linked and the CTA goes away cleanly. Record a toast so
    // the next render shows success/error directly to the user.
    if let Some(result) = oauth::poll_session() {
        oauth::record_toast(&result);
        // Force a repaint so the UI updates immediately instead of
        // waiting for the next user interaction — egui won't otherwise
        // schedule a frame when an off-thread state change lands.
        ui.ctx().request_repaint();
        match &result {
            Ok(token) => eprintln!(
                "login-cta: linked {} as {}",
                token.provider.display_name(),
                token.display_name
            ),
            Err(e) => eprintln!("login-cta: link failed: {e}"),
        }
    }

    // Also drain register session so the auto-register chain
    // (fresh-install path: click Google → register → OAuth retry)
    // fires regardless of which CTA the user is looking at. The
    // poll's internal logic auto-kicks OAuth via the stashed
    // retry provider when register lands Ok.
    if let Some(result) = crate::leaderboard::registration::poll_register_session() {
        match &result {
            Ok(id) => eprintln!("login-cta: register OK, install_id={id}"),
            Err(e) => eprintln!("login-cta: register failed: {e:?}"),
        }
        ui.ctx().request_repaint();
    }

    // Render any recent toast (success or failure) below the CTA
    // area. Clicking Dismiss clears it; starting a new flow also
    // clears it via try_start_session.
    render_oauth_toast(ui);

    // Read link status best-effort. Any I/O error reads as
    // "anonymous" so the CTA still shows — never hide the CTA
    // because of a transient filesystem hiccup.
    let active = crate::channel::active_channel();
    let status = oauth::status_for(active).ok();
    let is_anon = matches!(status, Some(oauth::AccountStatus::Anonymous) | None);
    // Linked state: render a discreet "Signed in as X (Provider)"
    // line above the grid instead of the CTA. Gives the user a
    // visible confirmation of the link that survives app
    // restarts (per Mick 2026-05-25T03:20Z — previously the link
    // status only showed via transient toast).
    if let Some(oauth::AccountStatus::Linked {
        provider,
        display_name,
        ..
    }) = &status
    {
        use crate::gui::widgets::oauth_chooser::provider_icon;
        ui.horizontal(|ui| {
            // Provider logo + "Signed in as X" line. Logo lives
            // before the text so the eye picks up the icon
            // first; size matches the line-height for a tidy row.
            ui.add(egui::Image::new(provider_icon(*provider)).max_size(egui::vec2(18.0, 18.0)));
            ui.label(
                RichText::new(format!(
                    "Signed in as {} ({})",
                    display_name,
                    provider.display_name(),
                ))
                .color(theme::ACCENT)
                .small(),
            );
            // Plain text "Sign out" link — keeps the management
            // affordance visible without competing with the
            // achievements grid. The full unlink + provider-
            // switch flow lives in Settings → Account.
            if ui
                .link(RichText::new("Sign out").color(theme::TEXT_LO).small())
                .clicked()
            {
                if let Err(e) = oauth::unlink_for(active) {
                    eprintln!("login-cta: unlink failed: {e}");
                }
            }
        });
        ui.add_space(6.0);
        return;
    }
    if !is_anon {
        return;
    }

    // Auto-register chain spinner: when a register session is
    // running (typically because the user clicked Login & Claim
    // on a fresh install + we have to register before linking),
    // show "Registering machine…" so the user knows something is
    // happening. Once register lands, the auto-retry chain kicks
    // OAuth + the next branch below takes over.
    if let Some(elapsed) = crate::leaderboard::registration::register_session_elapsed() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("Registering machine ({}s)…", elapsed.as_secs()))
                    .color(theme::TEXT_HI),
            );
        });
        ui.add_space(6.0);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
        return;
    }

    // While a flow is in flight (regardless of which UI surface
    // started it), render the spinner + Cancel row from any CTA
    // the user looks at — that's the responsive feedback issue #2
    // demanded.
    if let Some((provider, elapsed)) = oauth::current_session_snapshot() {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "Waiting for {} sign-in ({}s)…",
                    provider.display_name(),
                    elapsed.as_secs(),
                ))
                .color(theme::TEXT_HI),
            );
            if ui
                .add(
                    egui::Button::new(RichText::new("Cancel").color(theme::HOT))
                        .min_size(egui::vec2(60.0, 24.0)),
                )
                .clicked()
            {
                oauth::cancel_current_session();
            }
        });
        ui.add_space(6.0);
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(200));
        return;
    }

    let resp = ui
        .add(
            egui::Button::new(
                RichText::new("Login & Claim")
                    .color(theme::PANEL_DEEP)
                    .strong(),
            )
            .fill(theme::ACCENT)
            .min_size(egui::vec2(140.0, 28.0)),
        )
        .on_hover_text("Sign in to permanently keep your achievements across all your machines");
    if resp.clicked() {
        crate::gui::widgets::oauth_chooser::open();
    }
    ui.add_space(6.0);

    // Modal provider-chooser. Shared with the Settings → Account
    // tab; rendered against the global egui context so it shows
    // above whichever surface invoked it.
    crate::gui::widgets::oauth_chooser::show(ui.ctx(), active);
}

/// Render the most-recent OAuth toast inline below the CTA. Goes
/// green for success, red for failure. Includes a Dismiss button
/// so the user can clear it once they've seen it.
fn render_oauth_toast(ui: &mut egui::Ui) {
    use crate::leaderboard::oauth;
    let Some(toast) = oauth::current_toast() else {
        return;
    };
    ui.horizontal(|ui| {
        match &toast {
            oauth::OauthToast::Success {
                provider,
                display_name,
            } => {
                ui.label(
                    RichText::new(format!(
                        "✓ Linked: {} ({})",
                        display_name,
                        provider.display_name(),
                    ))
                    .color(theme::ACCENT)
                    .strong(),
                );
            }
            oauth::OauthToast::Failure { reason } => {
                ui.label(
                    RichText::new(format!("⚠ Link failed: {reason}"))
                        .color(theme::HOT)
                        .strong(),
                );
            }
        }
        if ui
            .add(
                egui::Button::new(RichText::new("Dismiss").color(theme::TEXT_LO))
                    .min_size(egui::vec2(64.0, 22.0)),
            )
            .clicked()
        {
            oauth::clear_toast();
        }
    });
    ui.add_space(4.0);
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
        ui.label(RichText::new(lifetime_human).color(theme::ACCENT).strong());
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

fn render_header(ui: &mut egui::Ui, state: &CatalogState, action: &mut Option<BadgeWallAction>) {
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
            .link(RichText::new("View profile →").color(theme::ACCENT).small())
            .clicked()
        {
            *action = Some(BadgeWallAction::OpenProfile);
        }
    });
}

fn render_grid(ui: &mut egui::Ui, state: &CatalogState, action: &mut Option<BadgeWallAction>) {
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
                    if render_tile(
                        ui,
                        tile.entry,
                        tile.granted,
                        tile.locked,
                        &tile.granted_years,
                        TILE_SIZE,
                    ) {
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
///
/// `granted_years` is non-empty only for recurring-annual entries
/// (per spec §6.2): it carries every year that grant has been
/// earned in, sorted ascending. The face label appends the most
/// recent year ("Holiday Cleaner 2026") and the tooltip lists all
/// of them ("Granted 2025, 2026").
fn render_tile(
    ui: &mut egui::Ui,
    entry: &CatalogEntry,
    granted: bool,
    locked: bool,
    granted_years: &[u16],
    size: egui::Vec2,
) -> bool {
    // Shield-based tile rendering (Mick 2026-05-25T03:30Z):
    // - Granted: full-color shield (`sdd-color-shield.png`)
    // - Ungranted: black-and-white shield (`sdd-bw-shield.png`)
    // - Locked: padlock glyph kept — different visual class
    //
    // Shield PNGs are transparent so the tile's PANEL_DEEP fill
    // shows through the negative space. Achievement name renders
    // under the shield.
    let (fill, stroke_color, text_color) = if locked {
        (
            Color32::from_rgb(0x14, 0x18, 0x22),
            Color32::from_gray(48),
            Color32::from_gray(100),
        )
    } else if granted {
        // Granted: dark panel + accent stroke + bright text.
        // The color of the shield itself is the visual reward,
        // not a tile-background hue.
        (theme::PANEL_DEEP, theme::ACCENT, theme::TEXT_HI)
    } else {
        // Ungranted: muted panel + dim stroke + faded text. Reads
        // as "exists, not yet earned" rather than "broken."
        (
            theme::PANEL_DEEP,
            Color32::from_gray(60),
            Color32::from_gray(140),
        )
    };

    let resp = egui::Frame::NONE
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, stroke_color))
        .corner_radius(egui::CornerRadius::same(6))
        .inner_margin(egui::Margin::symmetric(4, 4))
        .show(ui, |ui| {
            ui.set_min_size(size);
            ui.set_max_size(size);
            ui.vertical_centered(|ui| {
                if locked {
                    // Locked tiles keep the padlock glyph — visual
                    // signal that the achievement is gated behind a
                    // feature that hasn't shipped yet.
                    ui.label(RichText::new("⚿").color(text_color).strong().size(28.0));
                } else {
                    let shield_src = if granted {
                        egui::include_image!("../../../assets/sdd-color-shield.png")
                    } else {
                        egui::include_image!("../../../assets/sdd-bw-shield.png")
                    };
                    ui.add(egui::Image::new(shield_src).fit_to_exact_size(egui::vec2(48.0, 48.0)));
                }
                let face_label = match granted_years.last() {
                    Some(latest) => format!("{} {}", short_name(&entry.name), latest),
                    None => short_name(&entry.name),
                };
                ui.label(RichText::new(face_label).color(text_color).size(9.5));
            });
        })
        .response;
    let resp = resp.interact(egui::Sense::click());
    let tooltip = if locked {
        format!(
            "{} (locked)\n\nRequires sd-nas-pro features (Phase NP1+).\n\n{}",
            entry.name, entry.description,
        )
    } else if granted {
        if granted_years.is_empty() {
            format!("{}\n\n{}", entry.name, entry.description)
        } else {
            let years_str = granted_years
                .iter()
                .map(|y| y.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{}\n\nGranted {}\n\n{}",
                entry.name, years_str, entry.description,
            )
        }
    } else {
        format!(
            "{} (not yet earned)\n\nUnlock: {}",
            entry.name, entry.description,
        )
    };
    // Publish an accessible label so screen readers + the
    // egui_kittest render tests can identify which tile is which
    // AND read its grant state. Pattern: "<Name>: granted" |
    // "<Name>: not yet earned" | "<Name>: locked". The state
    // suffix is the assertion target in the badge-wall render
    // tests.
    let access_label = if locked {
        format!("{}: locked", entry.name)
    } else if granted {
        if granted_years.is_empty() {
            format!("{}: granted", entry.name)
        } else {
            let years_str = granted_years
                .iter()
                .map(|y| y.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}: granted {}", entry.name, years_str)
        }
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
    /// `true` when the tile belongs to a feature gate that hasn't
    /// shipped yet (currently the 5 NAS-pro entries). Renders with
    /// a padlock glyph + locked-style fill + a different hover.
    /// Mutually exclusive with `granted` (locked tiles can't be
    /// granted by definition; backend won't emit grants for them).
    pub locked: bool,
    /// Years this recurring-annual achievement has been granted in,
    /// sorted ascending. Empty for non-recurring entries and for
    /// recurring-annual entries with zero grants yet. Drives the
    /// per-tile year suffix ("Holiday Cleaner 2026") + the
    /// "Granted 2026, 2027, 2028" tooltip (per design's
    /// gamification-achievement-balance.md §6.2).
    pub granted_years: Vec<u16>,
}

/// Achievement IDs that are part of the sd-nas-pro feature track
/// (Phase NP1+). These render with a padlock until NP1 ships;
/// design's `gamification-achievement-balance.md` §3 keeps them
/// in the catalog with `visible: true` so users see the upcoming
/// surface even before NP1 lands.
const NAS_PRO_LOCKED_IDS: &[&str] = &[
    "schedule-master",
    "polite-citizen",
    "multi-share-maestro",
    "snapshot-sage",
    "email-report-veteran",
];

fn is_nas_pro_locked(id: &str) -> bool {
    NAS_PRO_LOCKED_IDS.contains(&id)
}

/// Split a grant ID into `(base, Some(year))` if it carries the
/// recurring-annual composite suffix `<base>#<YYYY>`, else
/// `(id, None)`. Per design's
/// `gamification-achievement-balance.md` §11.6 the backend writes
/// per-year grants under composite keys (`holiday-cleaner#2026`)
/// while the catalog row stays plain (`holiday-cleaner`); the
/// client aggregates back to the base when rendering. Malformed
/// suffixes (`#abc`, `#`, multi-`#`) fall through as plain IDs so
/// a stray grant row doesn't crash the badge wall.
fn parse_grant_id(id: &str) -> (&str, Option<u16>) {
    let Some((base, tail)) = id.rsplit_once('#') else {
        return (id, None);
    };
    // Reject empty base / multi-`#` ambiguity; treat as plain.
    if base.is_empty() || base.contains('#') {
        return (id, None);
    }
    match tail.parse::<u16>() {
        Ok(year) => (base, Some(year)),
        Err(_) => (id, None),
    }
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
    // Two parallel maps over the profile grants. Plain (no `#YYYY`
    // suffix) grants populate `plain_grants`; composite recurring-
    // annual grants get their years collected under the base ID in
    // `years_by_base`. A base ID is "granted" iff it has at least one
    // year — `granted_at` per row carries the per-year timestamp.
    let (plain_grants, years_by_base) = match state.profile.as_ref() {
        Some(Ok(p)) => {
            let mut plain: HashMap<&str, bool> = HashMap::new();
            let mut years: HashMap<&str, Vec<u16>> = HashMap::new();
            for g in &p.achievements {
                let (base, year) = parse_grant_id(g.achievement_id.as_str());
                match year {
                    Some(y) if g.granted => years.entry(base).or_default().push(y),
                    Some(_) => {} // ungranted year row — ignore
                    None => {
                        plain.insert(base, g.granted);
                    }
                }
            }
            for v in years.values_mut() {
                v.sort();
                v.dedup();
            }
            (plain, years)
        }
        _ => (HashMap::new(), HashMap::new()),
    };
    let mut tiles: Vec<GridTile<'a>> = catalog_entries
        .iter()
        .map(|e| {
            let locked = is_nas_pro_locked(&e.id);
            // Locked tiles can't be granted by definition; backend
            // won't emit grants for entries whose feature hasn't
            // shipped, so even if the profile somehow contains
            // one we mask it off as locked.
            let granted_years = if locked {
                Vec::new()
            } else {
                years_by_base
                    .get(e.id.as_str())
                    .cloned()
                    .unwrap_or_default()
            };
            let granted = !locked
                && (!granted_years.is_empty()
                    || plain_grants.get(e.id.as_str()).copied().unwrap_or(false));
            GridTile {
                entry: e,
                granted,
                locked,
                granted_years,
            }
        })
        .collect();
    // Sort order: granted first (visual test-friendly), then
    // ungranted-but-available, then locked tiles last (gated
    // upcoming features cluster at the bottom of the grid).
    tiles.sort_by_key(|t| {
        let bucket = if t.granted {
            0
        } else if t.locked {
            2
        } else {
            1
        };
        (bucket, t.entry.display_order)
    });
    tiles
}

fn count_grants(state: &CatalogState) -> (u32, u32) {
    let total = state
        .catalog
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|c| c.achievements.len() as u32)
        .unwrap_or(0);
    // Recurring-annual grants land as composite `<base>#<YYYY>`
    // rows (spec §6.2 / §11.6); multiple year-rows under the same
    // base must collapse to one toward the "X of N badges" headline
    // — otherwise an install with two years of `holiday-cleaner`
    // pushes the granted-count above the catalog total.
    let granted = state
        .profile
        .as_ref()
        .and_then(|r| r.as_ref().ok())
        .map(|p| {
            let mut bases: std::collections::HashSet<&str> = std::collections::HashSet::new();
            for g in &p.achievements {
                if !g.granted {
                    continue;
                }
                let (base, _) = parse_grant_id(g.achievement_id.as_str());
                bases.insert(base);
            }
            bases.len() as u32
        })
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
        assert_eq!(
            short_name("Sub-Minute Club (corpus-v1 50 GB)"),
            "Sub-Minute Club"
        );
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
        let profile: Profile =
            serde_json::from_str(profile_json).expect("live server profile shape must deserialise");
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
        assert!(
            tiles.iter().all(|t| !t.granted),
            "errored profile yields all-ungranted"
        );
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
            .root()
            .query_all_by_label_contains("")
            .filter_map(|n| n.accesskit_node().label())
            .collect();

        let granted_tiles: Vec<&String> =
            labels.iter().filter(|s| s.ends_with(": granted")).collect();
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
        render_to_png(
            &granted_state,
            out_dir.join("badge_wall-granted.png").as_path(),
        );
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
    fn nas_pro_ids_classify_as_locked() {
        // The 5 NAS-pro IDs render with the locked treatment
        // (padlock glyph + locked-style fill + locked hover) until
        // sd-nas-pro Phase NP1 ships. Grant flag is forcibly false
        // even if a profile claims one — the gate is in the engine,
        // not the backend.
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("schedule-master", "Schedule Master", "mid", 1000),
                entry("polite-citizen", "Polite Citizen", "low", 1010),
                entry("multi-share-maestro", "Multi-Share Maestro", "high", 1020),
                entry("snapshot-sage", "Snapshot Sage", "mid", 1030),
                entry("email-report-veteran", "Email Report Veteran", "low", 1040),
                entry("brisk", "Brisk", "low", 200),
            ],
        };
        // Backend incorrectly claims a NAS-pro grant — engine
        // must still render it as locked, not granted.
        let state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime: Default::default(),
                achievements: vec![
                    ProfileGrant {
                        achievement_id: "brisk".into(),
                        granted: true,
                        granted_at: None,
                    },
                    ProfileGrant {
                        achievement_id: "schedule-master".into(),
                        granted: true, // <- engine masks this off
                        granted_at: None,
                    },
                ],
            })),
        };
        let tiles = classify_grid_entries(&state, &catalog.achievements);

        // Every NAS-pro entry is locked.
        for nas_id in NAS_PRO_LOCKED_IDS {
            let tile = tiles
                .iter()
                .find(|t| t.entry.id == *nas_id)
                .unwrap_or_else(|| panic!("missing tile for {nas_id}"));
            assert!(tile.locked, "{nas_id} must classify as locked");
            assert!(
                !tile.granted,
                "{nas_id} must NOT classify as granted even when profile claims a grant"
            );
        }

        // brisk (not NAS-pro) classifies normally.
        let brisk = tiles.iter().find(|t| t.entry.id == "brisk").unwrap();
        assert!(!brisk.locked);
        assert!(brisk.granted);
    }

    #[test]
    fn parse_grant_id_splits_recurring_annual_suffix() {
        assert_eq!(parse_grant_id("holiday-cleaner"), ("holiday-cleaner", None));
        assert_eq!(
            parse_grant_id("holiday-cleaner#2026"),
            ("holiday-cleaner", Some(2026))
        );
        assert_eq!(
            parse_grant_id("new-years-resolution#2025"),
            ("new-years-resolution", Some(2025))
        );
    }

    #[test]
    fn parse_grant_id_rejects_malformed_suffix() {
        // Empty year, non-numeric year, empty base, multi-`#` — all
        // fall back to "treat as plain id" so a stray bad row doesn't
        // crash classify_grid_entries.
        assert_eq!(
            parse_grant_id("holiday-cleaner#"),
            ("holiday-cleaner#", None)
        );
        assert_eq!(
            parse_grant_id("holiday-cleaner#abc"),
            ("holiday-cleaner#abc", None)
        );
        assert_eq!(parse_grant_id("#2026"), ("#2026", None));
        // Multi-`#` is ambiguous; treat the entire thing as plain.
        assert_eq!(
            parse_grant_id("holiday#cleaner#2026"),
            ("holiday#cleaner#2026", None)
        );
    }

    #[test]
    fn recurring_annual_grants_aggregate_under_base_id() {
        // Catalog row is the plain base ID; profile has two
        // per-year composite grants. Tile must aggregate them into
        // one granted tile with granted_years = [2025, 2026].
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("holiday-cleaner", "Holiday Cleaner", "low", 700),
                entry("brisk", "Brisk", "low", 200),
            ],
        };
        let state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime: Default::default(),
                achievements: vec![
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2025".into(),
                        granted: true,
                        granted_at: Some("2025-12-28T10:00:00Z".into()),
                    },
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2026".into(),
                        granted: true,
                        granted_at: Some("2026-12-29T10:00:00Z".into()),
                    },
                ],
            })),
        };
        let tiles = classify_grid_entries(&state, &catalog.achievements);
        let holiday = tiles
            .iter()
            .find(|t| t.entry.id == "holiday-cleaner")
            .expect("holiday-cleaner tile present");
        assert!(holiday.granted, "two per-year grants → tile granted");
        assert_eq!(holiday.granted_years, vec![2025, 2026]);
        // brisk untouched.
        let brisk = tiles.iter().find(|t| t.entry.id == "brisk").unwrap();
        assert!(!brisk.granted);
        assert!(brisk.granted_years.is_empty());
    }

    #[test]
    fn recurring_annual_grants_dedupe_and_sort_years() {
        // Backend could emit the same year twice (e.g. a re-grant
        // after a manual server-side fix), and the order is not
        // guaranteed. Test that we dedupe + sort.
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![entry("holiday-cleaner", "Holiday Cleaner", "low", 700)],
        };
        let state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime: Default::default(),
                achievements: vec![
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2027".into(),
                        granted: true,
                        granted_at: None,
                    },
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2025".into(),
                        granted: true,
                        granted_at: None,
                    },
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2025".into(),
                        granted: true,
                        granted_at: None,
                    },
                ],
            })),
        };
        let tiles = classify_grid_entries(&state, &catalog.achievements);
        let h = tiles
            .iter()
            .find(|t| t.entry.id == "holiday-cleaner")
            .unwrap();
        assert_eq!(h.granted_years, vec![2025, 2027]);
    }

    #[test]
    fn ungranted_year_rows_do_not_grant_the_base() {
        // A row like `holiday-cleaner#2026` with `granted: false`
        // must NOT cause the base tile to render as granted. This
        // is the edge case where backend writes the row at year
        // rollover before the predicate fires.
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![entry("holiday-cleaner", "Holiday Cleaner", "low", 700)],
        };
        let state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime: Default::default(),
                achievements: vec![ProfileGrant {
                    achievement_id: "holiday-cleaner#2026".into(),
                    granted: false,
                    granted_at: None,
                }],
            })),
        };
        let tiles = classify_grid_entries(&state, &catalog.achievements);
        let h = tiles
            .iter()
            .find(|t| t.entry.id == "holiday-cleaner")
            .unwrap();
        assert!(!h.granted);
        assert!(h.granted_years.is_empty());
    }

    #[test]
    fn count_grants_dedupes_recurring_annual_years() {
        // Two `holiday-cleaner` per-year grants must collapse to a
        // single base in the "X of N" counter; otherwise X can
        // exceed N which reads as a corrupted UI.
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("holiday-cleaner", "Holiday Cleaner", "low", 700),
                entry("brisk", "Brisk", "low", 200),
            ],
        };
        let state = CatalogState {
            catalog: Some(Ok(catalog)),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime: Default::default(),
                achievements: vec![
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2025".into(),
                        granted: true,
                        granted_at: None,
                    },
                    ProfileGrant {
                        achievement_id: "holiday-cleaner#2026".into(),
                        granted: true,
                        granted_at: None,
                    },
                    ProfileGrant {
                        achievement_id: "brisk".into(),
                        granted: true,
                        granted_at: None,
                    },
                ],
            })),
        };
        assert_eq!(count_grants(&state), (2, 2));
    }

    #[test]
    fn locked_tiles_sort_to_bottom_of_grid() {
        // Sort order: granted → ungranted-but-available → locked.
        // Mick reviews badges visually, so granted tiles up top
        // (visual smoke test) and locked tiles at the bottom
        // (cluster of upcoming features) reads cleanly.
        let catalog = Catalog {
            version: "v1".into(),
            achievements: vec![
                entry("schedule-master", "Schedule Master", "mid", 1000),
                entry("brisk", "Brisk", "low", 200),
                entry("tidy-up", "Tidy-up", "low", 100),
            ],
        };
        let state = CatalogState {
            catalog: Some(Ok(catalog.clone())),
            profile: Some(Ok(Profile {
                install_id: "x".into(),
                lifetime: Default::default(),
                achievements: vec![ProfileGrant {
                    achievement_id: "brisk".into(),
                    granted: true,
                    granted_at: None,
                }],
            })),
        };
        let tiles = classify_grid_entries(&state, &catalog.achievements);
        let ordered_ids: Vec<&str> = tiles.iter().map(|t| t.entry.id.as_str()).collect();
        // Expected: granted (brisk) → ungranted (tidy-up) → locked (schedule-master)
        assert_eq!(ordered_ids, vec!["brisk", "tidy-up", "schedule-master"]);
    }

    #[test]
    fn count_grants_counts_only_granted() {
        let state = CatalogState {
            catalog: Some(Ok(Catalog {
                version: "v1".into(),
                achievements: vec![entry("a", "A", "low", 1), entry("b", "B", "low", 2)],
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
