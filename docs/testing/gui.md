# GUI test harness (MV slice)

Per `~/sd-bench-local/design/gui-test-harness-spec.md`. This file documents the **minimum-viable slice** shipped in `feat/g-track`. The full Tier 1 (5 days) + Tier 2 (5 days) + Tier 3 (sdd-testwin) expansion is deferred to a separate `feat/gui-test-harness` branch alongside `feat/scan-options`.

## Current tier coverage

| Tier | Status | What's covered |
|---|---|---|
| **Tier 0 — serde layer** | shipped (`ce0ea9f`) | Live backend JSON shape pinned. If `/api/v1/profile/{install_id}` drifts (renames `id`→`achievement_id`, un-nests `lifetime`, etc), the test goes red first. See `src/leaderboard/catalog.rs::profile_deserialises_live_backend_shape`. |
| **Tier 1 widget-state** | shipped (this commit) | Pure-function grid classification. `classify_grid_entries(state, catalog)` returns tiles in render order; tests assert grant flags + sort order survive the full pipeline. See `src/gui/widgets/badge_wall.rs::badge_wall_classifies_granted_tiles_from_live_server_shape`. |
| **Tier 1 widget-render** | shipped (`03b07a8`) | True headless egui rendering via `egui_kittest`. Renders the badge wall via `Harness::new_ui()` then queries the AccessKit tree for granted-vs-ungranted labels. Also produces PNG side-artifacts (`target/test-artifacts/badge_wall-{empty,granted}.png`) when run, for visual proof without booting an EXE. Sat on top of an egui 0.28 → 0.32 upgrade landed in the same branch. |
| **Tier 2 integration** | deferred | `mockito` HTTP mock + full-app harness. ~5 eng-days in `feat/gui-test-harness`. |
| **Tier 3 visual regression** | deferred | sdd-testwin owns; nightly screenshot diffs. ~4 eng-days. |

## What the MV slice catches

The bug class that motivated the spec: server JSON shape drifts → client `Profile` struct fails to deserialise → all `granted` flags read `false` → badge wall renders all-grey "0/N badges" even though the server has grants on file. Three places this can break:

1. **Wire format change**: server renames a field, drops a field, restructures lifetime / achievements. Caught by `profile_deserialises_live_backend_shape` — locked JSON fixture pins the exact shape.
2. **Local struct edit drift**: someone renames `ProfileGrant::achievement_id` or removes the `#[serde(rename = "id")]`. Caught by same test (deserialise panics or produces wrong fields).
3. **Widget-side bug**: serde succeeds but the wall ignores the data, sorts wrong, or misreads `granted`. Caught by `badge_wall_classifies_granted_tiles_from_live_server_shape` — runs the wire-format JSON through the full deserialise + classify pipeline and asserts 3 granted tiles + correct sort order.

## What it does NOT catch (yet)

- **Render-pipeline bugs**: pixel-level colorise treatment, font fallback, animation states, repaint timing. These need true egui_kittest rendering or Tier 3 screenshot diffs.
- **Mouse interactions**: click → action dispatch. Needs egui_kittest event injection.
- **Modal lifecycle**: post-scan modal state transitions. Same.
- **Network race conditions**: simultaneous submit + ranks_poll + profile_refresh. Needs Tier 2 mock server.

If a bug surfaces in one of these areas, the canonical pattern is: **write the failing test first** (extending the harness with the smallest dep needed), then ship the fix.

## How to add a widget-state test

Pattern used by `badge_wall_classifies_granted_tiles_from_live_server_shape`:

1. Build a `CatalogState` (and any other input state) using exactly the JSON shape the live backend emits. Pin fixtures inline; don't share them across tests (clearer regression signal).
2. Call the widget's pure helper (`classify_grid_entries`, `format_path`, `display_path`, etc.). If a widget doesn't have a testable pure helper yet, extract one — the widget should call the helper and pass its output to egui primitives.
3. Assert on the helper's return value: counts, ordering, field values.

Avoid asserting on egui internals or pixel output. Those tests belong in Tier 1 widget-render / Tier 3 visual regression once the harness expands.

## How to add a serde-layer test

Pattern used by `profile_deserialises_live_backend_shape`:

1. Capture the live backend's JSON shape (curl from a real install). Trim to the smallest fixture that proves the shape.
2. Deserialise into the engine's struct.
3. Assert on every field the GUI / engine reads downstream. If a field doesn't have an assertion here, a server rename can pass through silently.

## Running

```bash
cargo test --features gui,telemetry --lib badge_wall   # widget-state tests
cargo test --features gui,telemetry --lib catalog      # serde-layer tests
cargo test --features gui,telemetry --lib              # full lib suite (~235 tests as of 64e70f2)
```

CI gates merges on `cargo test --features gui,telemetry` passing across Linux + Windows.

## egui upgrade path (history)

`egui_kittest` is the de facto headless-rendering harness for egui. We were on egui `0.28`; egui_kittest first publishes at `0.32`. Upgrade landed as commit `04297a5` on `feat/egui-0.32-upgrade`. Touched 10 widget files (mechanical Margin int conversion + Painter StrokeKind argument additions). ~1-2 hours of work; ~40 deprecation warnings remain that should clean up in a follow-on commit before bumping to 0.33+ (e.g. `Rounding` → `CornerRadius`, `menu::bar` → `MenuBar::new`, `ScrollArea::id_source` → `id_salt`).

If a future egui bump pulls more breaking changes, the same pattern: count errors first, batch the mechanical fixes, run the full suite. The 41-warning cleanup is bookkeeping but worth doing.
