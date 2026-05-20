//! `superdupe` desktop GUI built on `egui` / `eframe`.
//!
//! Architecture: the engine runs on its own thread (or a real one once
//! wired up) and emits [`events::EngineEvent`] over a crossbeam channel.
//! The UI thread owns [`state::UiState`] and drains the channel each
//! frame. This keeps the render loop allocation-light and prevents the
//! engine from ever blocking the UI.
//!
//! The interesting widgets live in [`widgets`]: a live LCN-versus-time
//! read trace, an animated pipeline funnel, per-drive throughput scopes,
//! and a squarified treemap of reclaimable space.

pub mod app;
pub mod checkpoint;
pub mod demo;
pub mod events;
pub mod live;
pub mod state;
pub mod theme;
pub mod widgets;

pub use app::SuperdupeApp;
