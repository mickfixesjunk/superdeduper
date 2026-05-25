//! Provider-chooser modal for OAuth link flows.
//!
//! Single source of truth for the "Sign in with Google / Discord"
//! provider-pick dialog. Reused by:
//!
//! - `badge_wall::show()` — the "Login & Claim" CTA above the
//!   achievements grid (always anonymous-only).
//! - `settings_modal::render_account()` — the Account tab's single
//!   `Link…` button (when anonymous on the active channel).
//!
//! Per Mick's 2026-05-25T01:20Z preference, the chooser is a
//! centered modal `egui::Window` instead of an inline button row
//! so the user's attention focuses on the provider choice. Picking
//! a provider closes the dialog + starts the OAuth flow on the
//! shared background-session manager (`oauth::try_start_session`).

#![cfg(feature = "gui")]

use egui::RichText;

use crate::channel::Channel;
use crate::gui::theme;
use crate::leaderboard::{install, oauth};

/// Embedded provider logos. 48x48 PNGs in `assets/`; reused by
/// the modal chooser, the post-scan CTAs, the linked-state row
/// above the badge grid, and the Settings → Account status row.
pub fn provider_icon(provider: oauth::Provider) -> egui::ImageSource<'static> {
    match provider {
        oauth::Provider::Google => egui::include_image!("../../../../assets/48x48-google.png"),
        oauth::Provider::Discord => egui::include_image!("../../../../assets/48x48-discord.png"),
    }
}

/// Process-wide flag — `true` while the chooser modal is showing.
/// Set by any CTA's "open chooser" click; cleared on pick/Cancel.
static CHOOSER_OPEN: parking_lot::Mutex<bool> =
    parking_lot::Mutex::new(false);

pub fn open() {
    *CHOOSER_OPEN.lock() = true;
}

pub fn is_open() -> bool {
    *CHOOSER_OPEN.lock()
}

pub fn close() {
    *CHOOSER_OPEN.lock() = false;
}

/// Render the modal if `CHOOSER_OPEN` is set. No-op otherwise.
/// Picking Google or Discord kicks off the OAuth flow against
/// `channel` (typically the active channel) + dismisses the
/// modal. Inflight spinner then renders on the next frame via
/// each CTA's `current_session_snapshot` branch.
pub fn show(ctx: &egui::Context, channel: Channel) {
    if !is_open() {
        return;
    }
    let mut close_after_render = false;
    let mut start_provider: Option<oauth::Provider> = None;
    egui::Window::new(
        RichText::new("Sign in to claim")
            .color(theme::TEXT_HI)
            .heading(),
    )
    .collapsible(false)
    .resizable(false)
    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
    .default_width(360.0)
    .show(ctx, |ui| {
        ui.label(
            RichText::new(
                "Pick a provider to sign in with. Your achievements \
                 will roll up under one account across all your machines.",
            )
            .color(theme::TEXT_LO)
            .small(),
        );
        ui.add_space(12.0);
        ui.horizontal(|ui| {
            // Icon + text per provider. egui::Button::image_and_text
            // packs the logo + label into one click target so the
            // user can hit the icon, the text, or anywhere between.
            if ui
                .add(
                    egui::Button::image_and_text(
                        egui::Image::new(provider_icon(oauth::Provider::Google))
                            .max_size(egui::vec2(20.0, 20.0)),
                        RichText::new("Sign in with Google")
                            .color(theme::PANEL_DEEP)
                            .strong(),
                    )
                    .fill(theme::ACCENT)
                    .min_size(egui::vec2(170.0, 36.0)),
                )
                .clicked()
            {
                start_provider = Some(oauth::Provider::Google);
                close_after_render = true;
            }
            if ui
                .add(
                    egui::Button::image_and_text(
                        egui::Image::new(provider_icon(oauth::Provider::Discord))
                            .max_size(egui::vec2(20.0, 20.0)),
                        RichText::new("Sign in with Discord").color(theme::TEXT_HI),
                    )
                    .min_size(egui::vec2(170.0, 36.0)),
                )
                .clicked()
            {
                start_provider = Some(oauth::Provider::Discord);
                close_after_render = true;
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("Cancel").color(theme::TEXT_LO))
                            .min_size(egui::vec2(80.0, 28.0)),
                    )
                    .clicked()
                {
                    close_after_render = true;
                }
            });
        });
    });
    if close_after_render {
        close();
    }
    if let Some(p) = start_provider {
        start_link(p, channel);
    }
}

/// Kick off an OAuth flow on the background worker. Caller stays
/// responsive — per-frame `oauth::poll_session()` drains the
/// result when the worker finishes. Used by both the badge-wall
/// CTA + the Settings Account tab.
///
/// **Fresh-install path** (Mick 2026-05-25T02:36Z): when no
/// `install.{channel}.json` exists yet, the click would otherwise
/// silently do nothing (we have no install_id to send to web).
/// Instead we stash the provider as the pending OAuth retry +
/// kick off a register session; once register completes the
/// auto-retry chain (see `registration::poll_register_session`)
/// fires OAuth with the chosen provider against the freshly-
/// registered install_id. End-to-end this means "click Link →
/// pick Google → wait → browser pops up" with no manual register
/// step.
fn start_link(provider: oauth::Provider, channel: Channel) {
    let install_id_opt = install::load().ok().flatten().map(|s| s.install_id);
    let server_url = crate::channel::server_url_for(channel);

    // Fresh-install path (Mick 2026-05-25T03:00Z): kick the
    // register session in parallel with the OAuth flow instead of
    // sequentially. PoW takes ~1s; the user takes ~10s to sign in
    // with the provider; by the time engine reaches `exchange_code`
    // the register has long since saved install.{channel}.json.
    // The exchange step has a brief wait-loop guarding the rare
    // fast-signin race.
    //
    // The browser opens immediately on click — no "did anything
    // happen?" gap.
    if install_id_opt.is_none() {
        oauth::log_oauth_event(&format!(
            "fresh_install_chain: kicking register + OAuth in PARALLEL for \
             {provider} on {channel}",
        ));
        oauth::set_pending_retry_provider(provider);
        if crate::leaderboard::registration::try_start_register_session(channel)
            .is_err()
        {
            eprintln!(
                "oauth-chooser: register session already in flight (continuing with parallel OAuth)"
            );
        }
    }

    // OAuth flow always kicks — `link_via_loopback_inner` ignores
    // its `install_id` argument (see the `let _ = install_id;` at
    // its top); the value matters only at exchange time, where
    // `exchange_code` reads install state from disk directly.
    let install_id_for_session = install_id_opt.unwrap_or_default();
    if oauth::try_start_session(
        provider,
        channel,
        server_url,
        &install_id_for_session,
        oauth::DEFAULT_OAUTH_TIMEOUT,
    )
    .is_err()
    {
        eprintln!(
            "oauth-chooser: another OAuth flow is already in flight; ignoring"
        );
    }
}
