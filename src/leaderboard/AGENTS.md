# leaderboard - AGENTS guide

## Purpose

The `leaderboard/` directory implements the G-track gamification + leaderboards + achievements client. It is the engine side of the contract documented in `~/sd-bench-local/design/gamification-*.md`. The module is gated on the `telemetry` Cargo feature; a `--no-default-features --features gui` build strips everything here.

Responsibilities cluster into four bands:

1. Identity + transport. `install.rs` owns the per-channel install identity (UUID + 32-byte HMAC key). `hmac_signer.rs` (shim into the `superdeduper-hmac-signer` leaf crate) computes the `X-Sd-Signature` over canonical JSON bodies. `registration.rs` performs first-run server-bound PoW (D0 contract) or GUI captcha-via-loopback registration. `oauth.rs` adds G3 Google/Discord account linking + per-channel token storage. `captcha.rs` hosts the tiny stdlib HTTP loopback server used by both register + OAuth flows.
2. Submission pipeline. `submission.rs` is the engine integration layer over the `superdeduper-bench-real::submission_http` crate (which holds `build_payload` + the actual HTTP). `submission_store.rs` owns the on-disk submission queue + archive + review-queue. `gui_submission_state.rs` holds three process-wide slots (pending inputs, last outcome, pending submission_id) for the GUI handoff. `action_submission.rs` PATCHes `/submit/{id}/actions` with the action-credit summary + drives the retry-with-backoff worker. `pending_actions.rs` is the persistent action-credit disk queue that drains on the next successful submit. `ranks_poll.rs` polls `/api/v1/ranks` for 30 seconds after Accepted submissions.
3. Hardware + bracket detection. `hardware.rs` (the 2105-LOC file) detects CPU/RAM/disk_class/filesystem/os_edition cross-platform for the schema's `hardware` block, and `cpu_brackets.rs` classifies the CPU brand string against the bundled `data/cpu-brackets-catalog.json` snapshot (a frozen mirror of web's `api/data/cpu-brackets-catalog.yaml`).
4. Catalog + predicates + display. `catalog.rs` fetches `/achievements/catalog` + `/profile/{install_id}`; `predicates.rs` evaluates 11 client-claimed achievement predicates locally; `account_privacy.rs`, `account_display_name.rs`, `account_badge_summary.rs`, `vanity_slug.rs`, `payload_meta.rs` are smaller surfaces backing the GUI settings panel.

The four bench modules (`bench_client.rs`, `bench_run.rs`, plus the `mod.rs` re-exports of `bench`, `bench_corpus`, `d7_probe`) are thin shims into the `superdeduper-bench-real` crate after the Phase 0/1/2 split (2026-05-31 to 2026-06-01). Engine code keeps calling them under their old paths; the actual implementation lives cross-crate.

## Files

### `mod.rs`

Module root. Crate-level docs describe the G1-G4 phased rollout, the `telemetry` feature gate (`#![cfg(feature = "telemetry")]`), and the four bench modules whose bodies migrated to `superdeduper-bench-real`. `pub use` re-exports keep `leaderboard::bench::*`, `leaderboard::bench_corpus::*`, `leaderboard::d7_probe::*` resolving from the bench-real crate.

- Public modules: every other `.rs` file in this directory plus the `bench*` re-exports.
- Who calls this: `crate::leaderboard::*` throughout the engine (gated on `feature = "telemetry"`).
- Feature gate: `#![cfg(feature = "telemetry")]`.

### `install.rs`

Per-channel install identity store. Loads/saves `install.{prod|dev|local}.json` under `<data_dir>/install/`. Handles migration of pre-channel-aware flat `install.json` (prod-only). Owns `InstallState`, `InstallKey`, `ShareDefault`, `ShareChoice`, `BenchLane`, `InstallCounters` (re-exported from `predicates`), and the `SUPERDEDUPER_TEST_DATA_DIR` env override for hermetic test isolation.

- Public API: `CURRENT_SCHEMA_VERSION`, `STICKY_PROMPT_THRESHOLD`, `InstallKey`, `InstallState`, `BenchLane`, `ShareDefault`, `ShareChoice`; `install_path`, `install_path_for`, `data_dir_public`, `migrate_legacy_install_json`, `load`, `load_for`, `save`, `save_for`, `back_up_for`, `new_unregistered`, `bump_exclude_pattern_edits`, `bump_achievements_verify_invocations`, `record_share_prompt`.
- Who calls this: `registration.rs`, `oauth.rs` (`oauth_path_for` leans on `install_path_for`), `submission_store.rs` (queue/archive/review dirs sit next to install.json), `pending_actions.rs` (data_dir_public), `catalog.rs` (load), the CLI register subcommand, the GUI settings modal.
- Invariant: corrupted install.json fails closed; user must opt in via `--reset` to rotate identity (anti-shadowban-escape).
- Feature gate: implicit from `mod.rs` (telemetry).

### `hmac_signer.rs`

15-line re-export shim. After Phase 0 (2026-05-31) the implementation moved to the `superdeduper-hmac-signer` leaf crate so bench-real / bench-stub can depend on it without dragging the engine binary.

- Public API: `canonical_body`, `sign`, `sign_canonical` (all `pub use` from `superdeduper_hmac_signer`).
- Who calls this: every submission / register / OAuth / privacy / display-name / action-submission / ranks-poll path that needs to compute `X-Sd-Signature`.

### `registration.rs`

First-run registration. CLI path runs server-bound D0 PoW (fetch challenge from `/api/v1/pow/challenge`, solve hashcash, POST to `/api/v1/register` with the iat+signature echo). GUI path uses Cloudflare Turnstile via the loopback server in `captcha.rs` with `proof = captcha`. Owns the `RegisterSession` background-thread state (singleton via `CURRENT_REGISTER_SESSION` parking_lot mutex) plus auto-retry-OAuth chain on register success.

- Public API: `RegisterError`, `RegisterSession`, `SessionAlreadyRunning`, `CAPTCHA_TIMEOUT`, `DEFAULT_POW_DIFFICULTY`, `try_start_register_session`, `register_session_in_flight`, `register_session_elapsed`, `poll_register_session`, `register_cli`, `register_gui_via_loopback`, `compute_pow`.
- Who calls this: CLI `superdeduper register`, GUI settings modal sign-in flow.
- Invariant: server's `submit_url` hint in `/register` response is deliberately IGNORED (option 2 stance locked 2026-05-24T20:09Z). The engine resolves URLs locally via `channel::server_url_for` so a stale server hint can't leak dev installs onto prod.

### `oauth.rs` (2216 LOC)

G3 Google + Discord OAuth flow. Per-channel token store at `<data_dir>/install/oauth.{channel}.json`. Loopback HTTP server pattern reused from `captcha.rs`. Owns `OauthToken`, `OauthSession` (singleton), `Provider` enum + parser, `AccountStatus`, every error variant, plus auto-register-chain hooks (when OAuth fails with `InstallNotRegistered`, stashes the provider so register-on-success can resume).

- Public API: `Provider`, `ProviderParseError`, `OauthToken`, `OauthError`, `AccountStatus`, `OauthSession`, `DEFAULT_OAUTH_TIMEOUT`; `oauth_path`, `oauth_path_for`, `load_for`, `save_for`, `unlink_for`, `status`, `status_for`, `link_via_loopback`, `link_via_loopback_no_browser_fallback`, `link_via_loopback_cancellable`, `try_start_session`, `log_oauth_event`, `take_pending_retry_provider`; per-channel client-id helpers (`google_client_id`, `discord_client_id`).
- Who calls this: CLI `superdeduper account {link,unlink,status}`, GUI settings modal Account tab, `ranks_poll`'s retroactive profile refresh, the bench-lane Ranked-gate flow.
- Invariant: any server-supplied submit URL is dropped on the floor (same option-2 stance as `registration.rs`).

### `captcha.rs`

Tiny stdlib-only HTTP loopback server for the GUI register flow's Turnstile token capture. Binds 127.0.0.1:0; the URL given to the browser contains a per-session nonce so a third party can't blindly POST a forged token. Generic `handle_request_generic` reader/writer so tests drive it on synthetic byte streams. Cross-channel-mapping helper `web_origin_from_api` translates `dev-api.superdeduper.io` -> `dev.superdeduper.io` etc. via `channel::frontend_url_for`.

- Public API: `CaptchaError`, `await_captcha_token`.
- Who calls this: `registration::register_gui_via_loopback`; OAuth flow uses its own loopback in `oauth.rs` but follows the same pattern.

### `submission.rs` (795 LOC)

Engine-side submission layer. Most code (build_payload, the HMAC + POST, parse_ok/parse_error, effective_lane) moved to `superdeduper-bench-real::submission_http` in Phase 2-B (2026-06-01). This file is now mostly re-exports + the two thin `submit` / `submit_recorded_payload` wrappers that unpack `InstallState`. Plus the `wire_schema_json` schemars derive for `submit.schema.json` regeneration (#144).

- Public API: `ACTION_BYTES_KEY_DELETED_TO_RECYCLE`, `ACTION_BYTES_KEY_DELETED_PERMANENTLY`, `ACTION_BYTES_KEY_HARDLINK_REPLACED`, `FEATURE_BIT_*` (CACHE/FORMAT_AWARE/FOLLOW_LINKS/ALLOW_SYSTEM_PATHS/ALLOW_RECALL_ON_READ/REFERENCE_ROOTS/INCLUDE_GLOB/EXCLUDE_GLOB; bit 2 reserved), `wire_schema_json`, `submit`, `submit_recorded_payload`. Re-exports `SubmissionInputs`, `CanonicalBench`, `RunShape`, `ResultSummary`, `SubmitOutcome`, `RankEntry`, `build_payload` (all from `superdeduper-bench-iface` / `superdeduper-bench-real`). Re-exports queue/archive/review helpers from `submission_store` + GUI slots from `gui_submission_state`.
- Who calls this: CLI submit, GUI submit worker, `action_submission`'s auto-submit-then-patch chain, every bench flow.
- Invariant: `bit 2` of `features_used_bitmap` is RESERVED (formerly `FEATURE_BIT_PARANOID`, removed in #131 v0.2.16). Do not re-use; historical submissions stored it semantically.

### `submission_store.rs`

On-disk submission queue + archive + review storage extracted from `submission.rs` per codex-review item 3 (v0.3.27 2026-06-02). Three directories sit next to install.json: `submission-queue/` (50-entry cap, retry-on-next-launch), `submission-archive/` (permanent record of every attempt, ~4 KB per entry, no cap), `submission-review/` (rejected entries the user flagged for admin review; best-effort upload via `try_upload_review`).

- Public API: `queue_dir`, `archive_dir`, `review_dir`, `enqueue`, `archive_attempt`, `flag_for_review`. Crate-private: `now_iso8601`, `outcome_kind_tag`, `SerializableOutcome`.
- Who calls this: every submission code path (archive_attempt fires on every outcome); the GUI's Flag-for-review button; the next-launch retry path.

### `gui_submission_state.rs`

Three process-wide `OnceLock<Mutex<Option<...>>>` slots backing the engine-GUI handoff: `PENDING` (latest scan's `SubmissionInputs`), `LAST_OUTCOME` (`SubmitOutcome` for "rank #4 / +2 achievements" inline), `PENDING_SUBMISSION_ID` (most-recent Accepted submission_id for #79 PATCH).

- Public API: `store_pending`, `peek_pending`, `take_pending`, `store_last_outcome`, `peek_last_outcome`, `clear_last_outcome`, `store_pending_submission_id`, `peek_pending_submission_id`, `clear_pending_submission_id`, `update_last_outcome_ranks`.
- Who calls this: engine submit worker thread (write), GUI render thread (peek), `ranks_poll` (update_last_outcome_ranks), `action_submission::spawn_auto_submit_then_patch` (take_pending).

### `action_submission.rs`

PATCH `/api/v1/submit/{id}/actions` client + retry-with-backoff worker (500ms / 2s / 5s / 15s; #79 spec). Owns `ActionSubmissionStatus` (Submitting/Retrying/Credited/Queued) accessible to the GUI modal via process-wide `STATUS` slot. Has the two-phase `spawn_auto_submit_then_patch` (silent /submit then PATCH) used when the user skipped the post-scan modal but later took an action. Also drains the pending_actions disk queue on next /submit Accepted.

- Public API: `ActionSubmitOutcome`, `ActionSubmissionStatus`; `build_actions_body`, `actions_summary_from_dedupe`, `actions_summary_from_archive` (cfg `feature = "gui"`), `submit_actions`, `store_status`, `peek_status`, `clear_status`, `spawn_auto_submit_then_patch`, `drain_pending_after_submit`, `spawn_submit_worker`.
- Who calls this: dedupe action completion in app.rs, GUI archive action completion.

### `pending_actions.rs`

Persistent action-credit disk queue at `<data_dir>/install/pending_actions.json`. Drains via `drain_aggregated` on next /submit Accepted; entries older than `STALE_AGE_SECS` (30 days) drop silently.

- Public API: `STALE_AGE_SECS`, `PendingAction`, `append`, `peek`, `is_empty`, `drain_aggregated`.
- Who calls this: `action_submission::queue_to_disk` + `drain_pending_after_submit`.

### `ranks_poll.rs`

Background poller for `/api/v1/ranks`. Polls 30 times at 1s cadence after `Accepted`; on success merges ranks into `LAST_OUTCOME`, pushes a toast (GUI only), and fires a profile refresh in case the ranks unlocked a retroactive achievement.

- Public API: `POLL_INTERVAL`, `POLL_TIMEOUT_TOTAL`, `PollResult`, `poll_once`, `spawn_ranks_poll_worker`.
- Who calls this: submission Accepted path.
- Invariant: canonical HMAC input is `${install_id}|${submission_id}` (pipe-separated, no whitespace, no trailing newline) per web's contract.

### `hardware.rs` (2105 LOC)

Hardware fingerprint detection. Top-level `detect_with_root_hint(Option<&Path>)` builds a `HardwareFingerprint` (re-exported from `superdeduper-bench-iface` after the Phase 2-A move). Subsystems: `cpu_brand_string` (per-OS source: `/proc/cpuinfo` on Linux, `sysctl machdep.cpu.brand_string` on macOS, registry `ProcessorNameString` on Windows) + `normalize_cpu_brand` (strips AMD's "<N>-Core Processor" suffix). `cpu_isa_flags` via inline CPUID. `ram_gb` via `/proc/meminfo` / `sysctl hw.memsize` / `GlobalMemoryStatusEx` + `snap_ram_bucket`. `os_version` + `os_edition_enum` (registry `EditionID` on Windows, `Other` elsewhere by spec convention). `disk_class` via per-platform probing of the workdir's underlying device (NVMe-Gen5/4/3, SATA-SSD, HDD, USB-SSD/HDD, network, mixed, WSL2; with SAT pass-through ATA IDENTIFY refinement on Windows USB). `filesystem` via `GetVolumeInformationW` (Win), `statfs.f_type` magic mapping (Linux), `statfs.f_fstypename` mapping (macOS). `is_dev_drive` via `FSCTL_QUERY_PERSISTENT_VOLUME_STATE` on Windows-only, always false elsewhere.

- Public API: `HardwareFingerprint` (re-export from `superdeduper-bench-iface`); `detect`, `detect_with_root_hint`, `debug_raw_cpu_brand`. Crate-private (`pub(crate)`): `is_wsl` (Linux), `parse_ata_identify_rotation_rate` (Windows + tests), `rotation_rate_to_disk_class_kind` (Windows + tests).
- Who calls this: every submission build (CLI + GUI), `superdeduper debug cpu-brand`.
- Invariant: payload must emit EXACTLY the keys in the backend schema's `hardware.required` (and no extras: `additionalProperties: false`). The `detect_serialises_with_no_extra_keys` test pins this.
- Feature gate: WSL detection + workdir disk class are Linux-only via `cfg`. SAT pass-through is Windows-only via `cfg`. macOS disk-class shells out to `diskutil info -plist` (no IOKit FFI yet).

### `cpu_brackets.rs`

CPU bracket classifier (`flagship` / `high-end` / `mid-range` / `older` / `legacy` / `unknown`) against the bundled `../../data/cpu-brackets-catalog.json` snapshot. Lazily parses via `OnceLock`. Mirrors web-side `api/src/buckets/hardware-class.ts` at commit 65b91d1+acfc3f8.

- Public API: `Catalog`, `Bracket`, `BracketId`; `catalog`, `classify_cpu`, `classify_cpu_with`, `bracket_display_name`, `strip_trademark_markers`.
- Who calls this: GUI submit hint render, CLI bracket display, settings preview, post-scan modal.
- Invariant: snapshot intentionally lags web by ~one engine release; the `classifier_version` field is the contract version pin (currently 4 per the test).

### `predicates.rs` (1052 LOC)

11 client-claimed achievement predicates evaluated against scan data + `InstallCounters`. Each predicate returns `Option<&'static str>`; `evaluate_all` collects every match into a stable-ordered `Vec<String>` for `run_shape.easter_egg_hits`. Predicates: `abyss-walker`, `download-archaeology`, `format-fanatic` (T1.2-gated stub), `git-repo-detected`, `picky-eater` (counter-driven), `polyglot-paths` (unicode-script), `screenshot-graveyard`, `time-capsule`, `verify-veteran` (counter-driven), `recursive` (#161), `shadercache-hoarder` (#161). `compute_easter_egg_hits` is the entry point for both CLI + GUI scan paths so they can't drift.

- Public API: `PredicateContext`, `InstallCounters`, `evaluate_all`, `compute_easter_egg_hits` (cfg `telemetry`).
- Who calls this: CLI run_scan + GUI live scan finalization.

### `catalog.rs`

Achievement catalog + per-install profile fetch. Process-wide `OnceLock<Mutex<CatalogState>>` slot the badge wall reads each frame. `spawn_initial_fetch` (app start: catalog + profile + #77 cross-install badge summary), `spawn_profile_refresh` (post-submit cache-bust with `Cache-Control: no-cache` and `?_t=` ts).

- Public API: `Catalog`, `CatalogEntry`, `Profile`, `Lifetime`, `ProfileGrant`, `FetchError`, `CatalogState`; `fetch_catalog`, `fetch_profile`, `fetch_profile_fresh`, `peek_state`, `set_catalog`, `set_profile`, `set_account_badge_summary`, `spawn_initial_fetch`, `spawn_profile_refresh`.
- Who calls this: app start (initial fetch worker), submission Accepted path (`spawn_profile_refresh`), `ranks_poll` (refresh after ranks land).

### `account_privacy.rs`

`PrivacyFlags` 6-toggle state + GET (`/api/v1/profile/me`'s `privacy_applied`) + PATCH (`/api/v1/account/privacy`) client. Tolerates four wire shapes: `{flags: {...}}` (prod), `{privacy_applied: {...}}` (originally documented), `{privacy: {...}}` (transitional alias), bare `{show_*}` (PATCH echo). Has a critical no-silent-clobber guard: every flag is `#[serde(default)]` so an unknown wrapper would parse as all-OFF and clobber the optimistic toggle; the code requires at least one recognised key before trusting the parse.

- Public API: `PrivacyFlags`, `PrivacyOutcome`; `fetch`, `update`.
- Who calls this: GUI Settings -> Privacy panel.

### `account_display_name.rs`

`POST /api/v1/account/display_name` (set) + GET subset of `/profile/me` (get) for the nickname / `display_name_source` flow. Validates against the 1-32 char allowlist + the reserved `user-XXXXXXXX` auto-anon pattern. Server-side rate limit is 5 changes per account per 24h.

- Public API: `NICKNAME_MIN_LEN`, `NICKNAME_MAX_LEN`, `DisplayNameInfo`, `DisplayNameSource`, `DisplayNameOutcome`; `validate_nickname`, `fetch`, `set`.
- Who calls this: CLI `superdeduper account nickname set`, GUI Settings -> Account -> Nickname row.

### `account_badge_summary.rs`

#77 cross-install badge multiplier client. `GET /api/v1/account/badge-summary` (server resolves account from install_id + signature). Empty Vec on any failure (endpoint may not be deployed yet; badge wall renders with no multiplier overlay).

- Public API: `AccountBadgeEntry`, `AccountBadgeInstall`, `BadgeSummaryOutcome`; `fetch`.
- Who calls this: `catalog::spawn_initial_fetch`.

### `vanity_slug.rs`

#47 vanity-slug issuance for public profile URLs. Slug rules locked at 3..=32 chars, lowercase ASCII + digits + hyphens, must start with a letter, no trailing hyphen, no `--`, not on the reserved list. Engine generates a candidate from display_name + posts to the server for uniqueness check; server is source-of-truth.

- Public API: `VanityError`; plus generator + validate functions (the rest of the file).
- Who calls this: OAuth account-claim flow (post-link).

### `payload_meta.rs`

Submission-payload metadata helpers shared by CLI (`run_scan`) and GUI (`gui::live`). Scope classification (selection/whole-volume/subdirectory), corpus_kind (system/user-data), share-count heuristics. #142 moved these out of `gui::live` so the CLI scan_history rows finally carried a payload (without it `submit-pending` skipped every CLI row).

- Public API: `classify_scope`, `classify_corpus_kind`, `is_drive_root`, `is_network_share_path`, `count_distinct_share_roots`, plus the GUI-facing inventory summarizers.
- Who calls this: `gui/live.rs`, `main::run_scan`.

### `bench_client.rs`, `bench_run.rs`

Re-export shims. Phase 2-B (2026-06-01) moved the implementation bodies into the `superdeduper-bench-real` crate. Every existing call site continues to resolve via `pub use *::*`. See `crates/superdeduper-bench-real/src/{bench_client,bench_run}.rs` for the actual code.

### Non-source artifacts

None in this directory. The frozen JSON catalog snapshots used by `cpu_brackets.rs` live at `data/cpu-brackets-catalog.json` (two levels up from this dir, included via `include_str!`).

## Invariants / Gotchas

- **Telemetry gate**: every file here is `#![cfg(feature = "telemetry")]`. A `--no-default-features --features gui` build strips everything. `cpu_brackets.rs` is the only file NOT explicitly gated (it's referenced from gated parents). Anything that lives outside the leaderboard module and references it MUST guard with `#[cfg(feature = "telemetry")]`.
- **Schema additionalProperties:false**: `hardware.rs` payload MUST emit exactly the keys the backend schema lists in `hardware.required`. The `detect_serialises_with_no_extra_keys` test pins this. The `submit.schema.json` regeneration flow (#144 / `wire_schema_json`) is the forcing function for cross-cut schema changes.
- **PATCH UPSERT semantics**: every `actions_taken_summary` PATCH carries the COMPLETE map (never a delta); server treats the latest as source-of-truth. `pending_actions::drain_aggregated` sums by key under this rule. Do not change to delta semantics without coordinating with web.
- **Bit 2 of features_used_bitmap is reserved**: removed in #131 (v0.2.16) along with `--paranoid`. Historical submissions stored it semantically; do not reuse.
- **Server submit_url hint IGNORED**: per option-2 stance (2026-05-24T20:09Z) the engine resolves backend URLs locally via `channel::server_url_for`. `registration::submit_registration`, `oauth` exchange, etc. all drop any server-supplied URL. This prevents a stale hardcoded server URL from leaking a dev install onto prod telemetry.
- **install.json fails closed on corruption**: a parse failure surfaces as `Err` not `Ok(None)`; the user must explicitly opt in to `--reset` to rotate identity (anti-shadowban-escape per spec §4.5).
- **Reserved bit invariant**: `share_default == AskNThenSticky` plus `share_prompt_count` plus `share_last_choice` form a 3-way state machine; the GUI scan-complete modal + CLI must consult all three or sticky-mode regresses.
- **No-silent-clobber on privacy parse**: `account_privacy::parse_flags_body` requires at least one recognised flag key. A successful HTTP response that doesn't carry recognised keys returns `Transient`, preserving the caller's optimistic state instead of clobbering to all-OFF.
- **Canonical HMAC input format for /ranks**: `${install_id}|${submission_id}`, pipe-separated, NO whitespace, NO trailing newline. The `canonical_input_matches_web_contract` test pins the bytes.
- **CPU bracket catalog snapshot intentionally lags web**: the engine's `data/cpu-brackets-catalog.json` is the FROZEN snapshot for a given engine release. New web patterns reach engine on the next engine release. Same staleness model as the achievements catalog.
- **WSL disk_class returns "WSL2" not "HDD"**: the .vhdx virtual block device's `queue/rotational` flag is unreliable. The `is_wsl()` early-return suppresses the rotational-misclassify-as-HDD path; downstream io-thread cap fires on `disk_class.contains("HDD")` so a misclassify would cripple WSL scans.
- **`d7_probe`, `bench`, `bench_corpus` re-exports**: implementations live in `superdeduper-bench-real`. Engine call sites that wrote `leaderboard::bench::*` resolve via `pub use`. The shim modules contain ONLY `pub use ...::*;` (see `bench_client.rs`, `bench_run.rs`).
- **OAuth account_id != install_id**: per-machine identity is `install_id` (UUID, per channel). Cross-machine account roll-up is `account_id` (server-issued at OAuth link time). Confusing the two would break the badge multiplier overlay (#77).

## Dependencies

INCOMING (from elsewhere in the engine):
- `src/main.rs` (CLI subcommands: `register`, `account {link,unlink,status,nickname}`, `submit-pending`, `achievements {list,verify}`, `debug cpu-brand`, `bench --bench-me`)
- `src/gui/live.rs` (scan-finalize -> SubmissionInputs build + store_pending)
- `src/gui/widgets/{settings_modal,scan_complete_modal,badge_wall,toast,...}` (every account/privacy/badge surface)
- `src/scan_history.rs` (uses build_payload + submit_recorded_payload for resubmits)
- `src/app.rs` (action completion -> action_submission worker spawn)
- `src/channel.rs` (channel awareness)

OUTGOING:
- `superdeduper-bench-iface` (HardwareFingerprint, SubmissionInputs, CanonicalBench, RunShape, ResultSummary, SubmitOutcome, RankEntry: struct definitions only)
- `superdeduper-bench-real::submission_http` (build_payload, submit_inner, submit_recorded_payload_inner, parse helpers)
- `superdeduper-bench-real::{bench,bench_corpus,bench_client,bench_run,d7_probe}` (post-Phase-2 implementations)
- `superdeduper-hmac-signer` (canonical_body, sign, sign_canonical)
- `serde`, `serde_json`, `ureq`, `parking_lot`, `sha2`, `blake3`, `regex`, `uuid`, `unicode-script`, `schemars`, `libc`, `windows` crate (Win32 FS / Registry / Storage / SystemInformation), `tracing`
- `crate::{channel, dedupe, gui, inventory, log, log_warn, log_info, log_err, path_display, platform, test_serial, time, winapi_wrappers}`

## Refactor Hints

- **bench_client.rs / bench_run.rs are trivial shims** (14/15 LOC). Future cleanup: when the last leftover engine-internal caller migrates to `superdeduper_bench_real::*` direct imports, the shims can be deleted. Same applies to `hmac_signer.rs`.
- **mod.rs has dual styles of bench re-export**: `bench` / `bench_corpus` / `d7_probe` are `pub use` of an external module path; `bench_client` and `bench_run` are still `pub mod` with their own re-export shim files. The two are functionally equivalent; converging on one form (probably `pub use` since it's 14 fewer LOC) would clean this up at the cost of git-blame trail to the shim docs.
- **`is_wsl()` doc comment is structurally stranded**: the paragraph at hardware.rs:238-242 was originally for `workdir_disk_class_platform` (Linux) but visually attaches to `is_wsl()` since the latter has no blank line between them and the doc paragraph. Adding a blank line OR moving the Linux description down to `workdir_disk_class_platform` would unblock readers.
- **`account_badge_summary` doc references `fetch_or_mock` but the function is `fetch`**: the module rustdoc mentions a `fetch_or_mock` that doesn't exist. Rename in docs to `fetch`.
- **`bus_type_to_disk_class` doc references `refine_usb_with_sat_rotation_rate`**: this function doesn't exist; the actual refiner is `query_sat_rotation_rate` (Windows-only). Doc-link is dangling.
- **`predicates.rs` top-of-file table is stale**: lists `picky-eater` and `verify-veteran` as "stub (needs persistent counter)" but they're fully implemented (lines 337-357). Also doesn't list `recursive` or `shadercache-hoarder` (#161). Refresh the table.
- **`pending_actions` doc references `install/pending_actions.json` but the dir is created by `pending_actions::pending_file()`**: it sits at `<data_dir>/install/pending_actions.json` exactly as documented; fine.
- **`install::data_dir_public` doc mentions "the crate::log persister + the #99 pending_actions queue"**: only `pending_actions::pending_file` is an external caller (the log persister doesn't call this function). Mild stale-comment.
- **`install::fill_random` Windows path**: doc claims BCryptGenRandom but body uses the same `/dev/urandom-ish` xorshift fallback. The comment acknowledges "Real BCrypt wiring is a quick follow-up" — known TODO but worth tracking.
- **`cpu_brackets::log_pattern_error` per-process dedup**: the `HashSet<String>` in a static `Mutex` grows unbounded across the process lifetime if many distinct bad patterns ship. Today the snapshot has zero bad patterns; if a future re-vendor ships dozens of broken patterns this would be a slow leak. Bound by `Vec<(bracket_id, pattern)>` count or LRU if it ever matters.
- **`compute_pow` cap of 1u64 << (bits + 6)**: 64x expected at difficulty 22 = 2^28 = ~256M iters. The hashcash search runs serially on the calling thread; could be parallelised across `available_parallelism` for sub-second wins on multi-core. Not urgent — register fires once per install.
- **`hardware::macos_diskutil_info` is NOT timeout-bound**: documented at hardware.rs:670-676. A hung diskutil wedges the calling thread. Bounded join wrapper would close this.
- **predicates table missing #161 additions**: refresh.
- **Two `pub use` re-export groups in submission.rs**: `submission_store` items and `gui_submission_state` items are listed via `pub use super::...::{...}`. After the codex-review item 3 refactor, several callers could be migrated to import from the new modules directly; the shim re-exports could then shrink or vanish.

## Wire Surfaces

HTTP endpoints (all under `{server_url}/api/v1`):
- `POST /pow/challenge` (registration D0 server-bound PoW)
- `POST /register` (PoW or captcha proof)
- `POST /submit` (full payload)
- `PATCH /submit/{submission_id}/actions` (action-credit UPSERT)
- `POST /submit/review` (flag-for-review)
- `GET /ranks?install_id=...&submission_id=...` (post-submit poller)
- `GET /achievements/catalog` (badge catalog)
- `GET /cpu-brackets/catalog` (live mirror of the engine's bundled snapshot)
- `GET /profile/{install_id}` (per-install profile + grants)
- `GET /profile/me` (account-private view; `privacy_applied`, `display_name`, `display_name_source`)
- `PATCH /account/privacy` (6 privacy toggles)
- `POST /account/display_name` (nickname set + WRITE-TIME BACKFILL of submission rows)
- `GET /account/badge-summary` (#77 cross-install multiplier rollup; Option B)
- `POST /oauth/{provider}/start` (Google + Discord; loopback callback shape)

Headers added by engine: `X-Sd-Install-Id`, `X-Sd-Signature` (HMAC-SHA256 over `canonical_body(json_value)` using the install_key).

On-disk format versions:
- `install.{channel}.json`: schema_version 1 (`install::CURRENT_SCHEMA_VERSION`). Newer files refuse to load.
- `oauth.{channel}.json`: unversioned; new optional fields use `#[serde(default)]`.
- `pending_actions.json`: unversioned; entries older than `STALE_AGE_SECS` (30 days) drop on drain.
- `submission-queue/` cap: 50 entries (per spec §6.5). Eviction is oldest-first.

Environment variables read:
- `SUPERDEDUPER_TEST_DATA_DIR` (override `<data_dir>` to a tempdir for hermetic tests; verbatim, no `superdeduper/` subdir appended).
- `crate::channel::SERVER_URL_ENV_VAR` (`SUPERDEDUPER_SERVER_URL`): on load, overrides `state.server_url`. Lets a test redirect an already-registered install at a mock backend.
- `SUPERDEDUPER_OAUTH_TIMEOUT_SECS` (CLI driver; not read by `oauth.rs` directly).

Bundled assets:
- `data/cpu-brackets-catalog.json` (`include_str!` from `cpu_brackets.rs`).

CLI flags this dir owns (via wiring in `main.rs`):
- `superdeduper register` / `register --reset` / `register --channel`
- `superdeduper account {link,unlink,status,nickname}`
- `superdeduper submit-pending`
- `superdeduper achievements {list,verify}`
- `superdeduper debug cpu-brand`
- `superdeduper bench --bench-me` (drives `bench_run`)
- `--share` flag (post-scan share-default override)
