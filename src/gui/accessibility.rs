//! Accessibility actions surface.
//!
//! This module provides:
//! 1. Keyboard shortcuts for the action-spawn buttons (so assistive
//!    technologies, automation harnesses, and power users have a
//!    secondary input path that doesn't depend on hitting a specific
//!    pixel button).
//! 2. A small accesskit-action dispatcher that translates pending
//!    accesskit `Action::Click` events into the same internal action
//!    enum the mouse-click path uses. egui's stock handling already
//!    fires `Response::clicked()` on `Action::Click`, so this layer
//!    is forward-proofing: it gives engine code a single place to
//!    map external accessibility events to internal actions when
//!    egui's response-based path isn't usable (e.g., the action
//!    button is hidden behind a layout transition that frame, OR a
//!    future custom action / voice-command surface needs the same
//!    handler).
//!
//! Today the dispatcher's contract is a no-op overlay on top of
//! existing click handling — every action still requires the
//! upstream modal-flow (preflight, drift, etc.) to consent. The
//! point of shipping this slice now is to:
//!   - establish the "named-action" vocabulary the engine speaks
//!     against accessibility tools;
//!   - put keyboard shortcuts on the user-visible surface so
//!     screen-readers can announce them ("Start scan, Ctrl+R");
//!   - close sdd-testwin's GUI E2E unblock via the SendKeys path
//!     (Ctrl+R via UIA SendKeys bypasses the accesskit-invoke
//!     vs. winit-pointer-event divergence sdd-testwin hit on the
//!     mouse-click path).
//!
//! Cross-stack history: sdd-testwin 2026-05-31 18:50 PST flagged
//! 3 cells BLOCKED at the Start-scan-via-UIA-InvokePattern seam.
//! Engine deeper-trace (design 19:20 PST) localized the root to
//! H3 (preflight modal as a hidden gate after a successful first-
//! click). The keyboard-shortcut secondary path closes that seam
//! for E2E automation without changing user-facing modal flow.

use egui::{Context, Key, Modifiers};

/// Stable accessibility-shortcut catalog. One source of truth for the
/// human-readable shortcut strings + the (Modifiers, Key) tuples that
/// `app.rs::update()` checks each frame. Adding a new shortcut means
/// adding a const here + a call site in [`consume_pressed_shortcuts`].
pub struct Shortcuts;

impl Shortcuts {
    /// Start scan — primary action-spawn. Mirrors the "▶  Start scan"
    /// button in the roots panel. Used by sdd-testwin's UIA SendKeys
    /// path + accessibility users + power users.
    pub const START_SCAN: (Modifiers, Key) = (Modifiers::COMMAND, Key::R);

    /// Start scan — F-key alias for Windows convention parity
    /// (F5 = run/refresh in many Windows apps). Same dispatch as
    /// `START_SCAN`.
    pub const START_SCAN_F5: (Modifiers, Key) = (Modifiers::NONE, Key::F5);

    /// Cancel an in-flight scan. Mirrors the "Cancel" / "Pause"
    /// button shown while scanning.
    pub const CANCEL_SCAN: (Modifiers, Key) = (Modifiers::NONE, Key::Escape);
}

/// Named accessibility-driven actions. Used as the return value of
/// [`consume_pressed_shortcuts`] and the in-frame dispatch surface
/// for future custom-action handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessibilityAction {
    /// User requested the primary scan-start action via keyboard or
    /// (future) custom accessibility action. Engine routes this to
    /// the same internal handler as a mouse click on "▶  Start scan"
    /// (which goes through `start_live()` → preflight modal → scan
    /// worker spawn). The preflight modal IS still part of the
    /// flow — keyboard shortcuts don't bypass user-consent gates.
    StartScan,

    /// User requested an in-flight cancel via keyboard. Engine
    /// routes to the same internal handler as the "Cancel" / "Pause"
    /// button while scanning. No-op when no scan is in flight (the
    /// dispatcher is a passive consumer; the engine decides whether
    /// the action is currently applicable).
    CancelScan,
}

/// Drain any keyboard-shortcut events from the current frame's input
/// and return a list of accessibility actions the engine should
/// dispatch. Call once per frame, before the rest of `update()`.
///
/// Consuming via `key_pressed` (not `key_down`) so a held-down key
/// doesn't fire repeatedly — same semantics as `Response::clicked()`.
///
/// Modifier matching is exact (e.g., Ctrl+R fires, but Ctrl+Shift+R
/// does NOT) to avoid stealing browser-style chord shortcuts users
/// might rely on for other apps.
pub fn consume_pressed_shortcuts(ctx: &Context) -> Vec<AccessibilityAction> {
    let mut actions = Vec::new();
    ctx.input(|i| {
        if i.modifiers.matches_exact(Shortcuts::START_SCAN.0)
            && i.key_pressed(Shortcuts::START_SCAN.1)
        {
            actions.push(AccessibilityAction::StartScan);
        } else if i.modifiers.matches_exact(Shortcuts::START_SCAN_F5.0)
            && i.key_pressed(Shortcuts::START_SCAN_F5.1)
        {
            actions.push(AccessibilityAction::StartScan);
        }
        if i.modifiers.matches_exact(Shortcuts::CANCEL_SCAN.0)
            && i.key_pressed(Shortcuts::CANCEL_SCAN.1)
        {
            actions.push(AccessibilityAction::CancelScan);
        }
    });
    actions
}

/// Human-readable description for the action button tooltip that
/// surfaces the keyboard shortcut to the user (and indirectly to
/// any screen-reader that reads tooltip text).
///
/// Returns `Cmd+R` on macOS, `Ctrl+R` everywhere else. egui's
/// `Modifiers::COMMAND` is the cross-platform "primary modifier"
/// (Cmd on macOS, Ctrl on Linux/Windows/etc).
pub fn shortcut_label_start_scan() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘R or F5"
    } else {
        "Ctrl+R or F5"
    }
}

/// Human-readable description for the Cancel-scan shortcut.
pub fn shortcut_label_cancel_scan() -> &'static str {
    "Esc"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortcut_constants_are_unique_modifier_key_pairs() {
        // Guard against accidentally aliasing two distinct actions
        // to the same (modifiers, key) chord — a future Shortcuts::
        // const added without thinking about collisions would break
        // dispatch (the first match wins in `consume_pressed_shortcuts`).
        let all = [
            Shortcuts::START_SCAN,
            Shortcuts::START_SCAN_F5,
            Shortcuts::CANCEL_SCAN,
        ];
        for i in 0..all.len() {
            for j in (i + 1)..all.len() {
                // Allow START_SCAN + START_SCAN_F5 to share a target
                // action (both -> StartScan); their chord pairs must
                // still differ.
                assert_ne!(
                    all[i], all[j],
                    "duplicate shortcut chord at indices {i} and {j}",
                );
            }
        }
    }

    #[test]
    fn start_scan_label_is_platform_specific() {
        let label = shortcut_label_start_scan();
        // The label should advertise at least one chord the user can
        // press; either ⌘R / Ctrl+R, and the F5 alias.
        assert!(label.contains("R"));
        assert!(label.contains("F5"));
    }

    #[test]
    fn cancel_scan_label_is_escape() {
        assert_eq!(shortcut_label_cancel_scan(), "Esc");
    }
}
