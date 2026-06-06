# Swarm Health Check

**Owner:** `design` (swarm boss — runs from the neo-wsl host)
**Cadence:** Hourly (driven by a persistent Monitor; event-driven, not polled)
**Implementation:** `scripts/swarm-health-check.sh`

Adapted from the trade-boss-swarm reference at `mickfixesjunk/trade-boss` (`scripts/swarm-health-check.sh` + `docs/swarm-health-check.md`). This swarm's superdeduper topology has differences worth knowing:

- **Single-host swarm.** All 17 agent panes (WSL + Windows agents both) live in the same `giga-superdeduper` tmux session on neo-wsl. No SSH needed for pane capture.
- **V1 scope.** WEDGED detection only. IDLE_BAD detection is deferred to V2 once the repo has a priority-label scheme (`p0-critical` / `p1-high`).
- **Stood-down agents** (`czkawka`, `accountant`) are explicitly skipped via a `STOOD_DOWN` list in the script. Reactivate via Mick directive + remove from the list.
- **dumbo** runs in a separate Claude Code session (cleanroom isolation per [[feedback_dumbo_cleanroom_isolation]] memory) and is NOT in this tmux session — excluded from the sweep.
- **Tail window**: tail-12 (vs trade-boss's tail-40). Empirically tuned for this swarm — broadcast posts mentioning "API error" patterns leave scrollback text that triggers false positives at tail-40. Tail-12 reflects current state since recovery output displaces the error within seconds.

## What this is

A periodic sweep of all active agent panes to detect:

1. **Wedged agents** — stuck in a recoverable failure (rate-limit API error, transient connection error, server overload).
2. (V2, deferred) Unnecessarily-idle agents — pane shows standby pattern AND open p0/p1 issues without `health-check:ignore-idle` label.

When the sweep finds a wedged pane, the design (swarm boss) agent applies a **judgment-based nudge** via `tmux send-keys`. Critically: nudges are applied **one at a time** with delays between, to avoid creating a request storm at the Anthropic API that would worsen any existing rate-limit.

## When to run

- **Hourly,** automatically — the persistent Monitor wraps `--loop --quiet`. Emits to stdout only when actionable (per-pane lines on wedged/unreachable) OR one all-clear heartbeat line per healthy sweep (so a dead Monitor is distinguishable from a healthy swarm).
- **On demand,** when something looks off: `./scripts/swarm-health-check.sh` for the full report.
- **With verbose tails,** to eyeball each pane: `./scripts/swarm-health-check.sh --verbose`.

## How it works

The script uses `tmux capture-pane -p` on the local session for each window's primary pane, takes the last 12 lines, and runs pattern matches in order of priority.

### Classification priorities

Each pane gets exactly ONE status, picked in this order:

1. **STOOD_DOWN** — agent is in the `STOOD_DOWN` list (`czkawka`, `accountant`). Skip without flagging.
2. **UNREACHABLE** — `tmux capture-pane` returned empty (session dead, pane closed). Treated as wedged.
3. **WEDGED_API_ERROR** — last 12 lines match `API Error`, `Server is temporarily limiting`, `Internal server error`, `connection refused`, `429 Too Many`, `Overloaded`, `503 Service`, `connection error`. **Always wins.**
4. **OK (active-work-detected)** — last 12 lines match active-work cues: `shipping`, `building`, `writing`, `drafting`, `Smoke testing`, `Editing`, `Crunching`, `Cogitated`, `Sautéed`, `Brewed`, `↓ N tokens`, etc. Prevents flagging a mid-task agent.
5. **OK (no-issue-detected)** — fallback. Pane is in a neutral state (standby with watcher running, empty prompt, etc.); design agent can eyeball if they want but no action needed.

### Status codes & exit

| Status | Exit-code bit | Action |
| --- | :---: | --- |
| OK / STOOD_DOWN | — | None |
| WEDGED_API_ERROR | 1 | Wait 60s (rate limit usually clears), then nudge |
| UNREACHABLE | 1 | Manual investigation (check tmux directly) |

Exit code is 0 for healthy, 1 if any wedged/unreachable found.

## Nudge procedure (design-agent judgment)

The script DETECTS — design agent (operator) DECIDES the nudge content. Nudge text depends on context (which channel posts they missed, what they were doing before they wedged, how stale the rate-limit is).

### Send a nudge

```
tmux send-keys -t giga-superdeduper:<window> C-u "<nudge-text>" Enter
```

Then a second `Enter` to ensure submission (Claude Code's prompt sometimes needs a second Enter to commit multi-line input).

### Suggested nudge text by case

**WEDGED_API_ERROR:**
> "Check your last action — if rate-limited, the limit has likely cleared. Read recent posts on your bilateral channels (use the `tail -100` pattern on `~/.giga/configs/superdeduper/inbox/*.md`). Continue from where you left off."

If the agent has specific routing context that landed during their wedge window (e.g., a Mick directive override), include that explicitly:
> "Mick directive at HH:MM PDT: <directive>. Read latest <channel> post. <what to do>."

**UNREACHABLE:**
Don't nudge via send-keys. Investigate manually:
```
tmux ls
tmux capture-pane -t giga-superdeduper:<window>.0 -p
```

## Serialization rule — one nudge at a time

If 2+ agents need nudges in the same sweep:

1. Apply the nudge to the **highest-priority pane first** (the agent whose work is on the ship-critical path).
2. Wait at least **30 seconds** before nudging the next agent.
3. Re-run the health check after each nudge cycle to confirm the nudge took effect.

Why: simultaneous nudges = N concurrent Claude API calls hitting Anthropic. If the wedged state was a rate-limit, parallel nudges create a request storm and make things worse. Stagger.

## False positives & exceptions

Known cases:

1. **Historical API Error in scrollback (RESOLVED by tail-12).** Trade-boss's tail-40 caught long-recovered errors as WEDGED. Our tail-12 only catches errors that are still near the bottom (i.e., genuinely current). If we ever see false positives recur, tighten further or add a "minutes since last activity" check.

2. **OK (no-issue-detected) when agent is mid-task.** Sometimes an agent is on a non-standard screen (compaction running, `/clear` running, `/resume` picker, edit prompt with multi-line content). The active-work pattern misses these. They're transient — don't nudge.

3. **Reviewer just finished a review.** Pane may show review verdict prose immediately followed by "Standing by" — looks neutral but they just finished. Don't nudge if the activity timestamp is < 5 min old.

4. **`giga sync` / `giga merger` daemon panes.** If the swarm ever goes multi-host, those Monitor outputs may include API-error-like text from peer-host status. Not currently an issue (single-host); flag if it becomes one.

## Monitor setup

Arm the persistent Monitor from the design (swarm-boss) host. Per the [[update-config]] skill if integrating with Claude Code's hook system, OR just run:

```
Monitor(
  description: "swarm health sweep (hourly)",
  persistent: true,
  command: "cd /home/neomatrix/projects/mickfixesjunk/superdeduper && ./scripts/swarm-health-check.sh --loop --quiet"
)
```

The Monitor runs from the design host (this swarm boss). Single Monitor covers the entire swarm — no peer-host monitors needed.

If the Monitor dies (process crash, host reboot), the all-clear heartbeats stop. Design notices on next swarm-meta check and re-arms.

To run a sweep on-demand:
```
cd /home/neomatrix/projects/mickfixesjunk/superdeduper && ./scripts/swarm-health-check.sh
```

To change the cadence (e.g., 30-min during high-activity windows):
```
SWARM_HEALTH_INTERVAL_S=1800 ./scripts/swarm-health-check.sh --loop --quiet
```

## V2: IDLE_BAD detection (deferred)

The trade-boss reference includes an IDLE_BAD class that flags agents with idle patterns AND open p0/p1 GH issues. This swarm doesn't have a priority-label scheme yet:

- Repo: `mickfixesjunk/superdeduper`
- Existing label convention: internal `A-<label>` items in commits/posts (per [[feedback_internal_task_labels_a_dash_not_hash]]); `#NN` for actual GH issues
- Missing: `p0-critical`, `p1-high`, `agent:<name>`, `health-check:ignore-idle` labels

**V2 implementation requires:**

1. Add `p0-critical` + `p1-high` priority labels to `mickfixesjunk/superdeduper`.
2. Add `agent:<slug>` labels for each agent in the swarm (`agent:superdeduper`, `agent:design`, etc.) so issues can be filtered by assignee-agent.
3. Add `health-check:ignore-idle` label for date-deferred work that shouldn't trigger IDLE_BAD.
4. Backfill existing issues with appropriate priority + agent labels (engine's queue items like `#185`, `#175`, `#155`, `#144` would get classified).
5. Uncomment the `has_high_pri_issues` + `high_pri_issue_count` functions in the script (port from trade-boss reference).
6. Add classification step 3 (IDLE_BAD) between current step 2 (active-work) and current step 5 (fallback OK).

**Don't ship V2 until the label scheme exists** — otherwise every neutral-state agent gets flagged hourly. The trade-boss spec is explicit: false positives erode trust faster than missed signals.

## Scheduled v0.3.41+ migration (per gigachanges.md)

Per `workdirs/design/gigachanges.md` Scheduled section, all `scripts/bench/*` matrix tooling is queued for lift-and-shift to a new `mickfixesjunk/superdeduper-matrix-tooling` repo as a v0.3.41+ infra cycle. This health-check script could either:

- (a) Stay in the engine repo `scripts/` (separate from `scripts/bench/`) — different purpose
- (b) Migrate alongside the matrix tooling to the new repo (swarm tooling = swarm tooling, regardless of purpose)

Lean (b) for consistency. Tracking via the existing gigachanges entry.

## Anti-patterns to avoid

(From the trade-boss reference; same rules apply here.)

- **Don't add a WARN or INFO status.** Two failure modes only (WEDGED, eventually IDLE_BAD).
- **Don't make the script send-keys nudges itself.** Nudge text needs judgment.
- **Don't run `git pull` mid-loop.** Pull once at Monitor startup; if the script changes, TaskStop the Monitor and re-arm.
- **Don't poll with cron.** Monitor is event-driven; cron sends a notification every hour even when healthy.
- **Don't fire nudges in parallel.** One at a time, 30s+ apart.

## Update history

- **2026-06-06**: Initial creation. Adapted from trade-boss-swarm reference. Triggered by the v0.3.40 ship-cycle rate-limit storm (testdesign hit WEDGED on API error; manual pane sweep recovered them). Tail window tuned from 40 → 12 after empirical false positives on 4 agents with historical API-error text in scrollback.
