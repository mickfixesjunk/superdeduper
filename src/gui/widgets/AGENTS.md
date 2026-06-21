# widgets — AGENTS guide

## Purpose
`src/gui/widgets/` is the catalog of self-contained egui render units that the
top-level `gui::app::SuperdeduperApp` composes into the live UI. Each file
owns one visual concept (a modal, a panel, a banner, a chart) and exports
either a `show(...)` function (immediate-mode render returning the user's
choice / action) or a small struct + `show(state, ctx)` pair when it needs
persistent state (e.g. `BenchUiState`, `GroupsTableState`). The `mod.rs`
re-exports every widget; some are gated behind `feature = "telemetry"` so
non-telemetry builds compile without server-coupled UI.

Widgets are intentionally state-light: they read from `gui::state::UiState`,
`gui::checkpoint::CheckpointSummary`, `leaderboard::catalog::CatalogState`,
etc. — and either return an action enum that `app.rs` dispatches, or mutate a
narrow `&mut` slice of widget-local state. The widget layer never touches
the engine pipeline directly; the dispatcher in `gui::app` translates widget
actions into worker-thread calls.

This directory sits ONE level above the egui frame loop (`gui::app`) and ONE
level below the engine and persistence layers (`pipeline::`, `leaderboard::`,
`scan_history::`, `platform::`). It is the LARGEST single directory in the
crate by line count (12,301 LOC; 28 files); `settings_modal.rs` alone is
3,179 lines.

## Files

### `mod.rs`
Module entry. Just `pub mod` declarations; documents the "small slice of
UiState" rendering convention.
- Public API: re-exports every widget module
- Who calls this: `gui::app` (every modal/panel call site), `leaderboard::ranks_poll` (`toast::push`)
- Feature gates: `telemetry` gates `bench_modal`, `badge_multiplier_detail`, `badge_wall`, `oauth_chooser`, `resubmit_prompt_modal`, `scan_complete_modal`. `bench_modal` also requires `feature = "gui"`.

### `action_progress.rs` (131 LOC)
Spinner-modal that overlays the GUI while a long-running user-requested
action (safe-rename, archive, hardlink, etc.) runs on a worker thread.
Renders a determinate progress bar when total is known, indeterminate
"N processed" counter when not.
- Public API: `show(ctx, action: &ActionState) -> bool` (true == user clicked Stop)
- Who calls this: `gui::app` (`drain_events` action-progress branch)
- Key invariants: returns true ONCE per click; worker is responsible for finishing the current item before checking the cancel flag (caller-side contract documented in the modal copy).

### `alpha_warning.rs` (124 LOC)
Startup alpha-quality warning modal. Dismissable; "Got it — don't show
again" persists via `ScanSettings::dismissed_alpha_warning`.
- Public API: `enum AlphaWarningChoice { AcknowledgeOnce, AcknowledgeForever }`; `show(ctx) -> Option<AlphaWarningChoice>`
- Who calls this: `gui::app::render_alpha_warning_overlay_if_needed`
- Notes: trash-action verb is read from `crate::platform::trash_action_verb()` so the destructive-ops sentence reflects per-OS vocab (Trash vs Recycle).

### `archive_summary_modal.rs` (202 LOC)
Post-archive summary modal showing moved vs failed-by-reason bucket totals
+ the "Actually reclaimed from source" headline that feeds leaderboard
`archived_bytes`. Includes inline tests over `ArchiveActionSummary` math.
- Public API: `enum ArchiveSummaryChoice { Done, RevealDestination }`; `show(ctx, summary: &ArchiveActionSummary) -> Option<ArchiveSummaryChoice>`
- Who calls this: `gui::app` (post-archive event)
- Notes: only `moved_bytes` (NOT moved+failed) is the leaderboard-credit figure; pinned by the `summary_totals_round_trip` test.

### `badge_multiplier_detail.rs` (232 LOC; `#![cfg(feature = "telemetry")]`)
Per-install detail modal for multi-machine `#77` badge multipliers. Sorts
installs by `earned_at_unix` descending, displays nickname (or 4-char
prefix), CPU class, archived prefix, ISO YYYY-MM-DD date.
- Public API: `enum BadgeMultiplierDetailChoice { Close }`; `show(ctx, achievement_name, achievement_id, &[AccountBadgeInstall]) -> Option<...>`
- Who calls this: `gui::app::handle_badge_wall_action` (`TileClickedMultiplier` arm)
- Notes: ESC closes; outside-click closes; format helpers (`display_label`, `format_earned_at`) covered by unit tests.

### `badge_wall.rs` (1447 LOC; `#![cfg(feature = "telemetry")]`)
Achievements panel: lifetime headline + Login & Claim CTA + 3-column tile
grid. Reads `CatalogState`; produces `BadgeWallAction` for click routing.
Includes the pure helper `classify_grid_entries` so widget-state tests can
pin "given this state, the grid renders these tiles in this order with
these grant flags" without driving the egui frame loop. Heavy test
coverage: schema-mismatch regression, NAS-pro locked-mask, recurring-
annual grant aggregation, PNG side-artifacts via egui_kittest.
- Public API: `const NARROW_MODE_BREAKPOINT: f32 = 900.0`; `enum BadgeWallAction { TileClicked, OpenProfile, OpenRegister, TileClickedMultiplier }`; `struct GridTile { entry, granted, locked, granted_years }`; `fn show(ui, &CatalogState) -> Option<BadgeWallAction>`; `fn show_mini(ui, &CatalogState) -> Option<BadgeWallAction>`; `fn classify_grid_entries(&CatalogState, &[CatalogEntry]) -> Vec<GridTile<'_>>`
- Who calls this: `gui::app::handle_badge_wall_action`; the show/show_mini calls live in the live tab render branch.
- Key types / invariants:
  - Locked tiles (5 hardcoded NAS-pro IDs in `NAS_PRO_LOCKED_IDS`) MUST mask off any backend grant (`granted` is forcibly false). Pinned by `nas_pro_ids_classify_as_locked`.
  - Recurring-annual grants use composite `<base>#<YYYY>` IDs; client aggregates back to the base for the visible tile. Pinned by `parse_grant_id_*` and `recurring_annual_grants_aggregate_under_base_id`.
  - Sort order: granted -> ungranted-available -> locked, then by `display_order` within each bucket.
- Feature gates: `#![cfg(feature = "telemetry")]` at module level; reuses `oauth_chooser::provider_icon` so that path must also be telemetry-gated (it is).

### `bench_modal.rs` (808 LOC; `#![cfg(all(feature = "gui", feature = "telemetry"))]`)
T-BENCH-ME consent/explainer/result modal driving the Phase 3 BenchExecutor
flow on a worker thread. Five-phase state machine: `Lane -> Linking ->
Consent -> Running -> Done`. OAuth link gate is enforced for Ranked when
the install is anonymous; Casual flow is anonymous.
- Public API: `enum Phase`; `struct Shared` (worker -> UI snapshot); `struct BenchTierChoice`; `const USER_TIERS: &[BenchTierChoice]` (single entry, `corpus-v2-quick`); `struct BenchUiState { open, phase, fresh, tier_idx, show_share_preview, lane, linking_error, shared, cancel }`; `fn show(state, ctx)`
- Who calls this: `gui::app` (header "Benchmark" button + frame loop).
- Key invariants:
  - Worker thread (`run_worker` / `link_worker`) writes ONLY to `Shared` under mutex; main thread polls each frame.
  - `BenchExecutor` trait dispatch via `superdeduper_bench_real::BenchReal::new()`; Phase 3 v0.3.21 boundary.
  - Bench results write back to `Shared::result` + `Shared::deep_link` + `Shared::ranks` + `Shared::throughput_gbps`.
- Feature gates: `#![cfg(all(feature = "gui", feature = "telemetry"))]`.

### `cache_banner.rs` (138 LOC)
"Cached scan available" banner above the scan controls. Reads
`UiState::cache_volume_summaries`; toggles `state.use_cache_for_next_scan`.
Suppressed when `always_use_cache` is on (silent cache reuse).
- Public API: `fn show(ui, &mut UiState, always_use_cache: bool)`
- Who calls this: `gui::app` (live tab render branch).
- Key invariants: cache existence is computed elsewhere (`App::refresh_cache_banner`); this widget renders only.

### `channel_banner.rs` (109 LOC; `#![cfg(feature = "gui")]`)
Always-on environment banner for non-prod channels. Locked geometry per
`dev-channel-spec.md` §3.4 / §5.5: 32px tall, `#ff8800` for `dev`,
`#3399ff` for `local`, never dismissable, never appears on `prod`.
- Public API: `const BANNER_HEIGHT: f32 = 32.0`; `fn show(ctx, channel: Channel)`
- Who calls this: `gui::app` (frame loop top-of-window).
- Key invariants: NOT dismissable; absence-on-prod is the calibration signal.

### `drive_scope.rs` (401 LOC)
Per-drive live scope: sparkline of bytes/s + 2D LCN-vs-time read trace.
Click drive panel = filter Groups/Treemap to that drive. Click HDD/SSD
badge = cycle render override (persisted via volume GUID).
- Public API: `fn show(ui, &UiState, selected: Option<u32>, render_overrides: &mut HashMap<u32,bool>, frozen_now: Option<Instant>) -> Option<u32>`
- Who calls this: `gui::app` live tab.
- Key invariants:
  - Badge-click rect MUST be excluded from row-click hit zone (egui resolves overlapping click rects to the later-registered widget; documented inline at the `badge_rect` block).
  - Frozen-now lets post-scan rendering not decay throughput display once reads stop arriving.
  - Render-override cycle: `None -> Some(true SSD) -> Some(false HDD) -> None`; persisted to `drive_overrides::set` keyed by volume GUID.

### `exclusions_safe_defaults_banner.rs` (93 LOC; `#![cfg(feature = "gui")]`)
One-shot v0.2.7 "Exclusions enabled with safe defaults" banner. Dismissed
via `dismissed_v0_2_7_exclusion_banner`.
- Public API: `const BANNER_HEIGHT: f32 = 60.0`; `enum BannerAction { OpenSettings, Dismiss }`; `fn show(ctx) -> Option<BannerAction>`
- Who calls this: `gui::app::render_exclusions_safe_defaults_banner_if_needed`.

### `funnel.rs` (141 LOC)
Pipeline-funnel chart with one bar per `events::Stage::ALL` entry, pulse
overlay highlighting the most-recently-ticked stage, survival-ratio
footer (Confirmed / Inventory).
- Public API: `fn show(ui, &UiState, hash_algo: HashAlgo)`
- Who calls this: `gui::app` live tab.
- Note: stage count (`Stage::ALL.len() == 8`) drives the loop.

### `groups_table.rs` (1093 LOC)
Sortable duplicate-groups table with per-group action buttons + bulk-action
dropdown (safe-rename / archive-move / archive-copy / recycle / nuke).
Virtualized via `TableBuilder::heterogeneous_rows`. Includes a per-frame
`SortCache` keyed by `(state.duplicates.len(), drive_root, reference_roots.len())`
that eliminates O(N log N) sort on every frame during scan.
- Public API: `enum GroupAction { RecycleOthers, HardlinkOthers, Reveal, OpenFile, OpenFolder, SafeRenameOthers, SafeRenameAllVisible, ArchiveAllVisible, ArchiveCopyAllVisible, RecycleAllVisible, NukeAllVisible, PromoteKeeper, Preview }`; `enum BulkAction` (`#[derive(Default)]`, `SafeRenameDupes` default) + `is_destructive() / label()`; `struct GroupsTableState { expanded, acted, bulk_action, hide_unreclaimable, sort_cache }`; `fn show(...)`; `fn show_filtered(...)`
- Who calls this: `gui::app` (live tab + dispatcher reads `GroupAction`).
- Key invariants:
  - Sort key is `inode_aware_savings`, NOT path-aware; hardlink-equivalent groups (savings == 0) sort to the bottom regardless of alias count.
  - `BulkAction::is_destructive` returns true only for `RecycleDupes | NukeDupes`; ArchiveCopy is non-destructive (no source delete) and skips the typed-confirm gate.
  - Mid-scan: bulk-action Go button is disabled (`go_enabled = visible_dupe_count > 0 && !is_scanning`).

### `header.rs` (241 LOC)
Top status bar: title + build tag (`v{CARGO_PKG_VERSION} . {SD_BUILD_SHA}`),
hash-algo pill (BLAKE3 / RIVER5), Settings button, Benchmark button
(telemetry-gated), status line, four big stat tiles.
- Public API: `enum HeaderAction { None, OpenSettings, OpenBenchmark }`; `struct HeaderOutput { action, stats_rect }`; `fn show(ui, &UiState, hash_algo: HashAlgo, is_scanning: bool) -> HeaderOutput`
- Who calls this: `gui::app` (frame loop).
- Feature gates: `feature = "telemetry"` for the Benchmark button block.

### `log_panel.rs` (145 LOC)
Engine-log panel with warn/err counters + Copy-to-clipboard button. Pins
`resume diag:` lines above a 500-entry rolling tail (per `#104` Gap 2 fix).
- Public API: `fn show(ui, &UiState)`
- Who calls this: `gui::app` (live tab).

### `oauth_chooser.rs` (201 LOC; `#![cfg(feature = "gui")]`)
Provider-chooser modal shared by badge-wall CTA + Settings Account tab.
Process-wide `CHOOSER_OPEN` flag drives visibility.
- Public API: `fn provider_icon(Provider) -> ImageSource<'static>`; `fn open()`; `fn is_open() -> bool`; `fn close()`; `fn show(ctx, channel: Channel)`
- Who calls this: `badge_wall::render_login_cta`, `settings_modal::render_account`, `scan_complete_modal::render_signin_cta`.
- Key invariants: fresh-install path (no `install.{channel}.json`) parallel-kicks register + OAuth; OAuth's `link_via_loopback_inner` ignores its `install_id` arg (exchange reads from disk).
- Feature gates: `#![cfg(feature = "gui")]`. Note that despite being `#![cfg(feature = "gui")]` only, `mod.rs` further gates it on `#[cfg(feature = "telemetry")]`. (See Refactor Hints.)

### `overall_bar.rs` (153 LOC)
Overall progress strip above the status line. Indeterminate sweep when
total unknown; determinate fill with ETA when known. Resume cache-fast-
forward swaps the fill colour to red (`Color32::from_rgb(0xc8, 0x2a, 0x3a)`).
- Public API: `struct BarRects { full, fill }`; `fn show(ui, &UiState) -> BarRects`; `fn show_with(ui, &UiState, fast_forward: bool) -> BarRects`
- Who calls this: `gui::app` (live tab; sparkle-particles anchor to `BarRects::fill`).

### `preflight_modal.rs` (507 LOC)
Pre-flight credit-report modal per `docs/preflight-spec.md`. Renders three
states: Probing (spinner), Showing (TransUnion-style scorecard with axis
bars + recommendations), Failed (advisory error).
- Public API: `fn show(ctx, &PreflightState) -> Option<PreflightAction>`
- Who calls this: `gui::app::frame_top` (`PreflightState::Showing`/`Probing`/`Failed` arms).

### `resubmit_prompt_modal.rs` (147 LOC; `#![cfg(all(feature = "gui", feature = "telemetry"))]`)
App-start "Resubmit N pending scans?" modal. Reads
`scan_history::list_pending_older_than` rows; three choices.
- Public API: `enum ResubmitPromptChoice { ResubmitAll, OpenHistory, NotNow }`; `fn show(ctx, &[ScanRecord]) -> Option<ResubmitPromptChoice>`
- Who calls this: `gui::app::poll_resubmit_prompt`.
- Note: calls `super::scan_history_panel::format_unix_local` (which is `pub(crate)`).

### `resume_modal.rs` (188 LOC)
Launch-time "Resume / Start fresh?" modal. Tier-specific copy from
`ResumeTier` (`Full | Warm | InventoryOnly | Marker | Fresh`).
- Public API: `enum ResumeChoice { Resume, StartFresh }`; `fn show(ctx, &CheckpointSummary, ResumeTier) -> Option<ResumeChoice>`
- Who calls this: `gui::app::poll_resume_modal`.

### `roots_panel.rs` (236 LOC)
Roots list with per-row reference-toggle (★/☆), remove button, "Add
folder" / "Add reference folder" / Start/Pause/Cancel scan / Unsuperdeduper
controls. Mid-scan gates: adding/removing roots is disabled.
- Public API: `enum RootsAction { PickFolder, PickReferenceFolder, Remove(usize), ToggleReference(usize), StartScan, Pause, Cancel, Unsuperdeduper }`; `fn show(ui, &[RootEntry], is_scanning: bool, can_resume: bool) -> Option<RootsAction>`
- Who calls this: `gui::app` (left sidebar render branch).
- Note: keyboard tooltips (Ctrl+R/Cmd+R/F5/Esc) use platform-cfg const `&str` to avoid per-frame `format!` allocation (#191 perf push).

### `scan_complete_modal.rs` (809 LOC; `#![cfg(feature = "telemetry")]`)
Post-scan leaderboard modal per spec §10.1. Five states:
`Hidden -> Ready -> Submitting -> Done` plus `Preview` sub-modal. Includes
pre-sign-in CTA (`render_signin_cta`) shared with `badge_wall`.
- Public API: `enum ScanCompleteState`; `struct ScanCompleteData { elapsed_seconds, reclaimable_bytes, files_scanned, bytes_read, duplicate_groups, throughput_mbps }` + `from_engine_event`; `enum ScanCompleteAction`; `fn show(ctx, state, &ScanCompleteData, Option<&SubmitOutcome>, payload_preview, sticky_last_prompt) -> Option<ScanCompleteAction>`
- Who calls this: `gui::app` (post-scan event handler).

### `scan_history_panel.rs` (590 LOC; `#![cfg(feature = "gui")]`)
History tab listing past scans from `scan_history::list`. v2 adds per-row
Resubmit + Delete + a global "last outcome" banner with dismissable
sticky state via a `OnceLock<Mutex<Option<LastOutcome>>>`.
- Public API: `fn show(ui)`; `pub(crate) fn format_unix_local(secs: u64) -> String`
- Who calls this: `gui::app` (history tab); `resubmit_prompt_modal` calls `format_unix_local` via `super::`.
- Key invariants:
  - Per-frame disk read of scan-history (cost is small; <100 rows typical).
  - `format_unix_local` is named for "local time intent" but emits UTC (acknowledged in the doc comment) — pinned to `1970-01-01 00:00 / 2024-01-01 00:00` by tests.

### `scan_mode_picker.rs` (131 LOC; `#![cfg(feature = "gui")]`)
Scan-mode dropdown picker (Exact / Image / Audio). Per Mick directive
2026-06-20, Image + Audio rows are gated behind the
`experimental-similarity-in-gui` feature (default OFF); CLI continues to
expose them.
- Public API: `fn show(ui, &mut ScanMode, is_scanning: bool)`
- Who calls this: `gui::app` (live tab).
- Feature gates: `#![cfg(feature = "gui")]`; Image+Audio gated by `experimental-similarity-in-gui`.

### `settings_drift_modal.rs` (139 LOC)
"Settings changed since the paused scan" modal (#51). Three choices:
ContinueWithNew / RevertToPaused / Cancel.
- Public API: `enum SettingsDriftChoice`; `fn show(ctx, &CheckpointSummary) -> Option<SettingsDriftChoice>`
- Who calls this: `gui::app` (start-scan gating).

### `settings_modal.rs` (3179 LOC)
The omnibus settings modal. Nine tabs: Engine, Cache, KeepStrategy, Safety,
Pre-flight, Exclusions, Network, Account (telemetry), Leaderboard
(telemetry). State persists via `SettingsModalState { tab,
pending_channel, channel_switch_confirm }`. Process-wide `SAMPLE_PREVIEW`
slot drives the "What gets shared?" sub-window. Exposes JSON-payload
sample builders consumed by other widgets.
- Public API: `enum SettingsTab`; `struct SettingsModalState`; `fn show_done_dialog(message: String)`; `fn show(...)` (the main render); `fn build_sample_payload_json() -> String`; `fn build_bench_sample_payload_json() -> String`
- Who calls this: `gui::app` (frame loop); `bench_modal::sample_share_json` calls `build_bench_sample_payload_json`.

### `toast.rs` (164 LOC)
Transient corner notifications. Cross-thread push via a static
`OnceLock<Mutex<VecDeque<Toast>>>`. TTL-based fade; auto-dismiss.
- Public API: `struct Toast { heading, lines, spawned_at, ttl }`; `fn push(heading, lines, ttl)`; `fn show(ctx)`
- Who calls this: `gui::app::update`; `leaderboard::ranks_poll`; any background worker via `widgets::toast::push`.
- Key invariants: thread-safe push; egui repaint requested while any toast alive (drives fade animation without an external timer).

### `treemap.rs` (516 LOC)
Squarified treemap of reclaimable space. Tiles sized by inode-aware
reclaim (NOT path-aware — hardlinked groups would dominate the canvas
otherwise). Hover tooltip shows full member list.
- Public API: `fn show(ui, &UiState)`; `fn show_filtered(ui, &UiState, drive_root: Option<&Path>, reference_roots: &[PathBuf])`
- Who calls this: `gui::app` (live tab).
- Key invariants: squarify algorithm preserves total-area coverage and non-overlap (pinned by `tiles_cover_the_rect` + `tiles_dont_overlap_or_escape` + `tile_area_proportional_to_savings` tests).

## Invariants / Gotchas

- **Widget action enums are exhaustively dispatched in `gui::app`**: every variant of `GroupAction`, `RootsAction`, `BadgeWallAction`, etc. must have a corresponding match arm in `app.rs`. Adding a variant without updating the dispatcher silently breaks the UI.
- **`feature = "telemetry"` gating is split across two layers**: the module file may declare `#![cfg(feature = "telemetry")]` at the top AND `mod.rs` may further gate the `pub mod` line. `oauth_chooser.rs` is gated `#![cfg(feature = "gui")]` internally but `pub mod oauth_chooser` in `mod.rs` is `#[cfg(feature = "telemetry")]` — the latter is the effective gate.
- **`SortCache` in `groups_table.rs`** invalidates monotonically on `state.duplicates.len()` bumps. Mutations that REMOVE groups (none today; duplicates is append-only during scan) would break the cache and need an explicit invalidation knob.
- **Click-rect resolution order** (`drive_scope.rs`): egui resolves overlapping click rects to the LATER-registered widget. The `badge_rect` exclusion in `draw_drive_panel` is load-bearing — removing it makes the HDD/SSD badge appear inert.
- **Channel banner suppression on prod is structural**: spec forbids any prod-time banner so absence calibrates the user. Adding a prod fall-through arm to `show` violates the spec.
- **Bench-modal worker thread** writes ONLY to `Shared` under mutex; main thread reads via `ctx.request_repaint()` polling. Don't add direct UI calls from worker bodies.
- **`build_sample_payload_json` vs `build_bench_sample_payload_json`**: the bench preview MUST use the bench variant — generic scan sample shows `corpus_kind=user-data` + `bytes_scanned=320GB` which would misrepresent the synthetic bench and contradict the modal's "no personal files" claim (documented at `bench_modal.rs` `sample_share_json`).
- **`badge_wall::NAS_PRO_LOCKED_IDS`** is a hardcoded 5-element list; if the backend ever ships any of `schedule-master | polite-citizen | multi-share-maestro | snapshot-sage | email-report-veteran`, this list MUST be updated atomically with the engine's NP1 ship. Forcibly masking off backend grants is the cross-track defence.
- **Recurring-annual badge IDs use `<base>#<YYYY>` composite keys**; client aggregates back to the base in `parse_grant_id` + `classify_grid_entries`. Malformed suffixes (`#`, `#abc`, multi-`#`) fall through as plain — pinned by `parse_grant_id_rejects_malformed_suffix`.
- **`format_unix_local` in `scan_history_panel.rs`** is documented as "local-time intent but emits UTC" — the misleading name is acknowledged; renaming risks breaking `resubmit_prompt_modal::show` (cross-module call via `super::`).

## Dependencies

INCOMING (callers of this dir):
- `crate::gui::app` — the dominant caller; dispatches every widget action.
- `crate::leaderboard::ranks_poll` — calls `widgets::toast::push` to surface async rank-poll completion.
- (no other modules import from `widgets`)

OUTGOING (what this dir uses):
- `crate::gui::state` — `UiState`, `ScanSettings`, `RootEntry`, `LogEntry`, `CacheVolumeSummary`, etc.
- `crate::gui::theme` — all colours / `humansize` formatter.
- `crate::gui::events` — `Stage`, `LogLevel`, `OverallStage`, `DuplicateGroupSummary`, `ActionState`.
- `crate::gui::checkpoint` — `CheckpointSummary` (resume / settings-drift modals).
- `crate::gui::resume_tier` — `ResumeTier` (resume modal copy).
- `crate::gui::archive` — `ArchiveActionSummary`.
- `crate::gui::preflight` — `PreflightState`, `PreflightAction`, `Grade`, axis types.
- `crate::gui::resubmit` (cfg-telemetry) — `drain_outcome`.
- `crate::leaderboard::{catalog, oauth, install, registration, submission, hardware, account_badge_summary, bench_run}` — (telemetry-gated)
- `crate::scan_history` — `ScanRecord`, `SubmissionState`, list/delete.
- `crate::channel` — `Channel`, `active_channel`, `frontend_url_for`, `server_url_for`.
- `crate::pipeline::hash::HashAlgo`, `crate::pipeline::SimilarityKind`.
- `crate::platform` — `open_url`, `trash_action_verb`.
- `crate::path_display::for_user_display`.
- `crate::time::unix_to_ymdhms`.
- `crate::cli::ScanMode`.
- `crate::diagnose` (preflight modal).
- `crate::gui::drive_overrides` (drive scope).
- External: `egui`, `egui_extras`, `eframe`, `parking_lot`, `hashbrown`, `serde_json`, `humansize`, `egui_kittest` (tests), `superdeduper_bench_iface`, `superdeduper_bench_real`.

## Refactor Hints

- **`settings_modal.rs` is 3179 LOC** — by far the largest file in the directory. Tab-content sub-functions (`render_account`, `render_leaderboard`, `render_exclusions`, etc.) could move to `settings_modal/` sub-module files; the public surface is small (4 items + `SettingsTab` + `SettingsModalState`).
- **Glyph fallback comments are scattered**: `groups_table.rs` lines ~110-135 + 671-693 carry `#156 -- A-bmp-glyph-fallback` comments. The pattern is "no SMP code-points in egui labels"; would be cleaner as a `glyph_constants` module instead of inline `cfg!(target_os)` snippets.
- **Stage::ALL count drift** (`funnel.rs`): module doc says "8 pipeline stages" (matches `Stage::ALL` len), but the function-doc hover-text on line 21 still says "Five-stage funnel". Stale inner comment.
- **`oauth_chooser.rs`** declares `#![cfg(feature = "gui")]` at the module level but is reached only when telemetry is on (`mod.rs` line 22 gates `pub mod oauth_chooser` by `feature = "telemetry"`). The internal cfg is redundant; either drop the inner `#![cfg(feature = "gui")]` or change the `mod.rs` gate to `all(feature = "gui", feature = "telemetry")` for clarity.
- **Resume modal hover text** (`resume_modal.rs` line 149) mentions `BLAKE3 / DDH-128 cache` — DDH-128 is the old name; current code uses `River5` (see `pipeline/hash/algo.rs` line 5: `River5 (formerly DDH-128)`). Stale UI string.
- **`badge_wall.rs` test comments around line 1003-1006** describe tier-specific glyphs (★ / ◆ / ●) that no longer drive the render — the actual code uses shield PNGs (`sdd-color-shield.png` / `sdd-bw-shield.png`) and AccessKit labels. Test logic still works because it reads AccessKit labels, but the doc-comment narrative is stale.
- **`bench_modal::USER_TIERS`** is a single-entry slice; the surrounding code defensively walks it as a `&[BenchTierChoice]` for "future tiers". The doc comment on line 87 calls out `corpus-v2-full` slotting in here when web hosts it. No dead-code; intentional placeholder.
- **`SettingsTab::all()`** uses `#[allow(unused_mut)]` to handle the cfg-telemetry conditional push. A cleaner approach would be a const array per cfg branch, but this is purely cosmetic.
- **`groups_table.rs::show` (line 198)** is documented as "kept for callers that don't need a drive filter" but the doc admits "the App always uses `show_filtered`". Confirmed via grep — `show` has zero non-test callers in `src/`. Suspect dead code; verify with `grep -rn "groups_table::show(" src/ --include='*.rs'` (returns no hits outside the file itself). Could be removed in favour of `show_filtered`-only.

## Wire Surfaces

The widgets dir does NOT own any HTTP / JSON / CLI surfaces directly, but it reads / propagates several:

- **Bench submission deep-link URL shape** (`bench_modal::run_worker` line 328): `{frontend}/leaderboard?tab={tab}&highlight={submission_id}`. Server contract.
- **OAuth provider PNGs** (`oauth_chooser.rs`): `assets/48x48-google.png`, `assets/48x48-discord.png` — embedded via `egui::include_image!`.
- **Badge wall shield PNGs** (`badge_wall.rs` line 488): `assets/sdd-color-shield.png`, `assets/sdd-bw-shield.png` — embedded via `egui::include_image!`.
- **Build identification env vars** (`header.rs` line 36): `CARGO_PKG_VERSION` (cargo), `SD_BUILD_SHA` (from `build.rs`). Compile-time only.
- **Persisted settings keys** (read/written via `ScanSettings`):
  - `dismissed_alpha_warning`
  - `dismissed_v0_2_7_exclusion_banner`
  - `use_cache_for_next_scan`, `always_use_cache`
  - `last_bench_lane` (in `install.json`, not `ScanSettings`)
- **Scan-history JSON keys touched** (read-only via `ScanRecord`):
  - `started_at_unix`, `roots`, `total_dups`, `channel`, `submission_channel`, `attempt_count`, `reclaim_at_unix`, `reclaim_updated_at_unix`, `action_breakdown.{deleted_to_recycle_bytes, deleted_permanently_bytes, hardlink_replaced_bytes, reflink_replaced_bytes, archived_bytes}`
- **Action-breakdown labels** (`scan_history_panel::action_breakdown_tooltip`): order is `recycle -> permanent -> hardlink -> reflink -> archive`; key on the wire is fixed (`deleted_to_recycle_bytes`) but the user-visible label flips per OS (Trash vs Recycle).

## Non-source artifacts
None — all 28 files are `.rs` source.
