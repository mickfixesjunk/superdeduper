//! #27 v1 — In-app file previews for the duplicate-decision UI.
//!
//! Per design's `t2.5-ipreviewhandler-spec.md` §3.1 the full v1
//! scope is:
//!
//!   * Host the registered Windows `IPreviewHandler` for the file
//!     extension when available.
//!   * Fall back to a built-in text viewer for text-like files.
//!   * Fall back to a built-in hex viewer for binaries.
//!   * Side-by-side mode for comparing two files.
//!   * Annotation overlay ("N duplicates at: …") on top of any
//!     viewer.
//!
//! This module ships the **scaffold + fallback viewers**, plus the
//! Windows registry-lookup that resolves an extension to its
//! preview-handler CLSID. The actual `IPreviewHandler` COM hosting
//! (CoCreateInstance, IInitializeWithFile, DoPreview into an
//! offscreen WIC surface, GDI/D2D bitmap capture, upload to an egui
//! texture) needs iteration against a live Windows host — that
//! lands in a Windows-side session. For now the system-handler
//! branch surfaces "preview handler registered at {CLSID}; full
//! integration coming" so the user sees the wiring exists.
//!
//! ## Module layout
//!
//! ```text
//! src/gui/preview/
//!   mod.rs                      — public API (this file)
//!   classify.rs                 — extension → which viewer to use
//!   fallback_text.rs            — built-in text viewer (text-like files)
//!   fallback_hex.rs             — built-in hex viewer (binaries)
//!   registry_lookup.rs          — Windows registry → IPreviewHandler CLSID
//! ```

#![cfg(feature = "gui")]

pub mod classify;
pub mod fallback_hex;
pub mod fallback_text;
#[cfg(windows)]
pub mod registry_lookup;

use std::path::{Path, PathBuf};

use egui::{RichText, Ui};

use crate::gui::theme;

/// How a given file is being previewed right now. Driven by
/// [`classify::classify_path`] for the initial pick; the user can
/// flip to hex via a button if a misclassified binary slipped
/// through the UTF-8 sniff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewMode {
    /// Built-in text viewer (read-only egui TextEdit). First 64 KiB
    /// of the file, monospace, wrap-on.
    Text,
    /// Built-in hex viewer. First 4 KiB + last 4 KiB; 16-byte rows
    /// with ASCII gutter.
    Hex,
    /// File extension has a registered Windows IPreviewHandler.
    /// The CLSID resolved at lookup time. Rendering is currently
    /// a stub (lookup-only); the actual COM-hosting + bitmap
    /// capture is deferred to a Windows-side iteration session.
    SystemHandler { clsid: String },
    /// Nothing renderable — file vanished between selection +
    /// preview, permission denied, recall-on-open placeholder.
    Unavailable { reason: String },
}

/// Render the preview panel for a single file. Returns an
/// optional `PreviewAction` the host (App) should handle —
/// today only the close-button action; v2 will add side-by-
/// side toggle, hex-override, etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewAction {
    /// User clicked Close. App clears `previewed_file`.
    Close,
    /// User clicked "Show as hex" — overrides the auto-classified
    /// text mode.
    ForceHex,
    /// User clicked "Show as text" — overrides hex (e.g. binary
    /// that's actually a CSV).
    ForceText,
}

/// Hold the user's mode override across renders so the toolbar
/// state is sticky for the lifetime of a preview session. `None`
/// = auto-classify per file (the default).
#[derive(Default, Debug, Clone)]
pub struct PreviewState {
    pub mode_override: Option<PreviewMode>,
}

/// Top-level entry: render the preview pane for `path` inside `ui`.
pub fn show(ui: &mut Ui, path: &Path, state: &mut PreviewState) -> Option<PreviewAction> {
    let mut action: Option<PreviewAction> = None;

    // Header / annotation row: path + size + close.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("👁  {}", display_path(path)))
                .color(theme::TEXT_HI)
                .monospace(),
        );
        if let Ok(meta) = std::fs::metadata(path) {
            ui.label(
                RichText::new(format!(
                    " — {}",
                    humansize::format_size(meta.len(), humansize::BINARY)
                ))
                .color(theme::TEXT_LO)
                .small(),
            );
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .small_button(RichText::new("× Close").color(theme::TEXT_HI))
                .clicked()
            {
                action = Some(PreviewAction::Close);
            }
        });
    });
    ui.separator();

    // Toolbar — mode overrides + system-handler info.
    let auto_mode = classify::classify_path(path);
    let effective_mode = state
        .mode_override
        .clone()
        .unwrap_or_else(|| auto_mode.clone());
    ui.horizontal(|ui| {
        if ui
            .selectable_label(matches!(effective_mode, PreviewMode::Text), "📝 Text")
            .on_hover_text(
                "Force the text viewer (useful for misclassified binaries that ARE text).",
            )
            .clicked()
        {
            state.mode_override = Some(PreviewMode::Text);
            action = Some(PreviewAction::ForceText);
        }
        if ui
            .selectable_label(matches!(effective_mode, PreviewMode::Hex), "🔢 Hex")
            .on_hover_text("Force the hex viewer (always works; never lies about content).")
            .clicked()
        {
            state.mode_override = Some(PreviewMode::Hex);
            action = Some(PreviewAction::ForceHex);
        }
        if let PreviewMode::SystemHandler { clsid } = &auto_mode {
            ui.label(
                RichText::new(format!("✨ System handler: {clsid}"))
                    .color(theme::COOL)
                    .small()
                    .italics(),
            )
            .on_hover_text(
                "This file extension has a registered Windows IPreviewHandler. \
                 Full COM-hosted preview is a v2 follow-up — for now the fallback \
                 viewer renders. (See #27 spec §3.2 for the hosting plan.)",
            );
        }
    });
    ui.add_space(4.0);

    // Body — dispatch to the appropriate fallback viewer.
    match effective_mode {
        PreviewMode::Text => fallback_text::show(ui, path),
        PreviewMode::Hex => fallback_hex::show(ui, path),
        PreviewMode::SystemHandler { .. } => {
            // The lookup succeeded but actual COM hosting isn't
            // wired yet — fall through to the next-best viewer
            // (hex, since "system handler" usually means binary).
            ui.label(
                RichText::new(
                    "System-handler preview is a Windows v2 follow-up. \
                     Showing hex fallback meanwhile.",
                )
                .color(theme::TEXT_LO)
                .italics()
                .small(),
            );
            ui.add_space(2.0);
            fallback_hex::show(ui, path);
        }
        PreviewMode::Unavailable { reason } => {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    RichText::new("Preview unavailable")
                        .color(theme::WARN)
                        .heading(),
                );
                ui.label(
                    RichText::new(reason)
                        .color(theme::TEXT_LO)
                        .small()
                        .italics(),
                );
            });
        }
    }

    action
}

/// User-facing path display. Delegates to the crate-root
/// `path_display::for_user_display` so the `\\?\` strip rules
/// stay aligned across every UI surface (groups-table, Log,
/// preview-header). Per #73 — was previously a duplicate of
/// the helper in `groups_table` + `live.rs`.
fn display_path(p: &Path) -> String {
    crate::path_display::for_user_display(p)
}

/// Side-by-side preview of two files. Render `left` next to `right`
/// in two equal halves. v1 stub — v2 will wire this from the
/// groups-table when the user has two members of the same group
/// selected. For now the App doesn't call this; left as a leaf so
/// the v2 wiring is a one-line widget swap.
#[allow(dead_code)]
pub fn show_side_by_side(
    ui: &mut Ui,
    left: &Path,
    right: &Path,
    left_state: &mut PreviewState,
    right_state: &mut PreviewState,
) {
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width() / 2.0, ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                show(ui, left, left_state);
            },
        );
        ui.separator();
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), ui.available_height()),
            egui::Layout::top_down(egui::Align::Min),
            |ui| {
                show(ui, right, right_state);
            },
        );
    });
}

/// Convenience: render the panel + return the new previewed path
/// the host should set. Returns `None` when the path is unchanged;
/// `Some(None)` when the user closed the panel.
pub fn show_or_close(
    ui: &mut Ui,
    current: &Option<PathBuf>,
    state: &mut PreviewState,
) -> Option<Option<PathBuf>> {
    let Some(path) = current else {
        return None;
    };
    match show(ui, path, state) {
        Some(PreviewAction::Close) => Some(None),
        _ => None,
    }
}
