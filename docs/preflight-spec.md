# Pre-flight modal — design spec

> **Status:** spec for the GUI agent to implement against.
> Engine work (Block P `diagnose` subcommand) is shipped — the
> modal consumes its JSON output.
>
> **Aesthetic target:** Transunion / FICO credit-report feel.
> Score-card surface for a technical product. Memorable.

---

## Trigger

User clicks **Scan** in the GUI for a set of folders.

For each folder in the scan set, check the **pre-flight cache** (per-folder, per-machine):

* **Cache hit** (≤ N days old, machine identity unchanged) → skip preflight, scan immediately.
* **Cache miss** → run preflight before scan starts. Block scan launch until user dismisses the modal.

A global setting controls preflight overall. Off by default for power users? On by default for first-time users. **Recommend: on by default with a "Don't show again for this folder" + a global toggle in Settings.**

## Cache key

`(canonicalized_folder_path, machine_identity_hash)`

Where `machine_identity_hash` is a stable hash of CPU model + RAM tier + drive serial(s) so preflight re-runs when hardware changes (laptop docked vs not, etc.).

Stored at `%LOCALAPPDATA%\superdeduper\preflight\<machine_id>\<folder_hash>.json`.

TTL: 30 days, configurable.

## Engine call

The modal kicks off `superdeduper diagnose <folder> --format json` as a background process. Streams stdout, parses the final report. Engine prints structured JSON (`schema: "superdeduper.diagnose.v1"`).

Timeout: 60 seconds. If exceeded, fall through to a "skip preflight this time" path with a small "preflight timed out" toast.

## Modal layout (mockup spec)

The modal is roughly the proportions of a credit-report page from a major bureau:

```
┌───────────────────────────────────────────────────────────────────┐
│  superdeduper preflight             ✕                             │
│                                                                   │
│       Scan-readiness grade:    ╭───╮                              │
│                                │ B+│       ← single big score     │
│                                ╰───╯                              │
│                                                                   │
│       Hardware capability    Disk read       Hash compute         │
│           [bar chart]         [bar chart]      [bar chart]        │
│                                                                   │
│  Estimated scan time for this folder: ~ 12-18 seconds             │
│                                                                   │
│  ─────────────────────────────────────────────────────────────    │
│                                                                   │
│  ▸ HIGH IMPACT (>20% improvement)                                 │
│    ⚠ Windows Defender Real-Time Protection is enabled.            │
│      Defender scans every file we open, adding 50–200ms per       │
│      open. Disabling RTP for this scan will speed it up ~2–3x.    │
│      Only do this if you trust the corpus you're scanning.        │
│      [ Learn more ]  [ Disable for this scan (admin) ]            │
│                                                                   │
│    ⚠ Windows Search Indexer is indexing this folder.              │
│      The indexer races us for file handles. Pause it for          │
│      this folder while scanning.                                  │
│      [ Pause indexer for this folder (admin) ]                    │
│                                                                   │
│  ▸ MEDIUM IMPACT (5–20%)                                          │
│    ℹ Increase IO threads to 96 (from default 48).                 │
│      You have CPU headroom — oversubscribe the IO pool.           │
│      [ Apply suggestion ]                                         │
│                                                                   │
│  ▸ INFORMATIONAL                                                  │
│    ℹ Your machine is in the 78th percentile for this workload.    │
│      (Based on 1,247 other users with similar hardware.)          │
│      [ Why ? ]                                                    │
│                                                                   │
│  ─────────────────────────────────────────────────────────────    │
│                                                                   │
│  ☐ Submit anonymous benchmark data to help improve sd's defaults  │
│    [Learn what gets sent]                                         │
│                                                                   │
│  ☐ Don't show preflight for this folder again                     │
│                                                                   │
│            [ Cancel scan ]      [ Start scan → ]                  │
└───────────────────────────────────────────────────────────────────┘
```

### Score-card design notes

* **The headline grade** (A+ through F) is the single number the user
  remembers. Compute from a weighted combination of per-axis subscores.
  Specific grading curve is in the recommendations engine.
* **Per-axis bars** show absolute numbers (MB/s, files/sec) with
  small comparator text like "(70th percentile)" when telemetry data
  is available locally.
* **Estimated scan time** uses the diagnose probe results × the
  inventory size (rough scan of folder for file count + total bytes
  in <5s) to project. ±50% accuracy is fine for setting expectations.

### Color palette

* Score grades: A green, B yellow-green, C amber, D orange, F red.
* Impact icons: ⚠ red for HIGH, ℹ blue for MEDIUM/INFO.
* Background: clean white / off-white. Sparingly use the brand
  accent for the score circle border.

### Typography

* Score card: prominent serif (Charter? PT Serif?) — credit-report feel.
* Body: clean sans-serif (Inter? System UI?).

## Recommendations engine

Located at: `src/diagnose.rs::build_recommendations` (engine-side, ships with the binary).

**Input:** the populated `DiagnoseReport` struct.

**Output:** `Vec<Recommendation>` with impact / title / detail / optional action.

**Logic** (current, will evolve):

| condition | impact | recommendation |
|---|---|---|
| `defender.rtp_enabled == true` | High | "Defender RTP enabled, slows scans 2–3x. Disable only if you trust corpus." |
| `profile == SlowCpuFastDisk` | Medium | "Hash compute may bottleneck large files. Default river5 is faster than blake3 here." |
| `profile == FastCpuFastNvme` | Info | "You're disk-bound, not hash-bound. Hash algo doesn't matter on your hardware." |
| `tier1.files_per_sec_per_thread < 500` | Medium | "Small-file open throughput is low. Check AV beyond Defender / storage stack." |
| (future) Windows Search indexer active on path | High | "Pause indexer for this folder" + action button |
| (future) tier3 < disk seq read ceiling × 0.5 | Medium | "Tier 3 IO scheduling improvement available — update sd" |

The engine produces structured records; the GUI doesn't need to know
the rules. New recommendation types ship in engine releases.

## Actions

A subset of recommendations have an "Action" button. Each action is
implemented engine-side as a command-line invocation, surfaced via:

```rust
pub struct RecommendationAction {
    pub kind: ActionKind,             // RtpDisable / IndexerPause / IoThreadsBump
    pub command: String,              // shell-runnable, what the GUI executes
    pub requires_admin: bool,         // true means UAC prompt
    pub reverse_on_scan_end: bool,    // true means we restore state after scan
}
```

**Actions that change system state** (RTP toggle, indexer pause) MUST:

* Show a confirmation modal before execution: "This will disable
  Defender RTP for the duration of this scan. RTP will be re-enabled
  automatically when the scan finishes (success or failure). Continue?"
* Register a state-restore hook that runs at scan end, including on
  panic / crash / process kill.
* Log to the diagnostics file: "Disabled RTP at T+0s, restored at
  T+12s, post-scan RTP state verified = enabled."

**Actions that change app state only** (IO thread bump): just apply
to the current scan; reset for the next one.

## Submission opt-in (Block T)

The "Submit anonymous benchmark data" checkbox at the bottom is
opt-in only. Default: unchecked.

When checked, after scan completes, the GUI sends:

* Hashed machine identity (irreversible).
* Probe results from this preflight.
* Anonymized workload signature (file count buckets + size buckets,
  no paths or names).
* Wall-clock + reclaimable + dup-group count.
* sd version + hash algo used.

Endpoint: `https://api.superdeduper.com/v1/preflight-submit`

What we *don't* send: paths, filenames, file contents, hashes (would
be reversible), system hostname, user account.

Privacy review needed before shipping. Default off in EU.

## Telemetry feedback loop

Backend aggregates submissions into per-hardware-class "expected
performance" profiles. Future sd versions ship with these profiles
bundled so the preflight can show comparator text:

* "Your machine is in the 78th percentile for this workload."
* "Median scan time for this workload on your hardware class is 14s."

This is how preflight becomes more valuable over time without
requiring per-user telemetry.

## Performance budget

* Preflight should complete in ≤ 5 seconds for the median user.
* If `--skip-io` lets it finish in ≤ 1 second on cached folders, do
  that. The IO probe is the long pole; only run it on first preflight
  for a folder, then cache.

## Error handling

* Engine returns non-zero or malformed JSON → fall back to "scan
  without preflight" with a small toast. Don't block the user.
* Permission errors writing to scratch dir → try system temp, then
  give up gracefully.
* No telemetry submission attempts if user is offline.

## Settings

In the Settings dialog, add a "Preflight" section:

* `[x] Run preflight before scanning (recommended)`
* `[x] Allow preflight to submit anonymous benchmark data`
* `[ ] Always show preflight (even for cached folders)`
* `Cache TTL: [30 days] [dropdown]`
* `Reset preflight cache for all folders [button]`
