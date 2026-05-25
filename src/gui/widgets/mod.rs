//! Custom widgets that make the GUI worth its silicon. Each widget
//! takes a small slice of [`UiState`] and renders one concept.

pub mod action_progress;
pub mod alpha_warning;
pub mod archive_summary_modal;
#[cfg(feature = "telemetry")]
pub mod badge_multiplier_detail;
pub mod exclusions_safe_defaults_banner;
#[cfg(feature = "telemetry")]
pub mod badge_wall;
pub mod cache_banner;
pub mod channel_banner;
pub mod drive_scope;
pub mod funnel;
pub mod groups_table;
pub mod header;
pub mod log_panel;
#[cfg(feature = "telemetry")]
pub mod oauth_chooser;
pub mod overall_bar;
pub mod preflight_modal;
#[cfg(feature = "telemetry")]
pub mod resubmit_prompt_modal;
pub mod resume_modal;
pub mod roots_panel;
#[cfg(feature = "telemetry")]
pub mod scan_complete_modal;
pub mod scan_history_panel;
pub mod scan_mode_picker;
pub mod settings_drift_modal;
pub mod settings_modal;
pub mod toast;
pub mod treemap;
