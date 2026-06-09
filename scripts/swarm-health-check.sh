#!/usr/bin/env bash
# swarm-health-check.sh — sweep all agent panes for wedged states.
#
# Adapted from trade-boss-swarm reference (mickfixesjunk/trade-boss scripts/swarm-health-check.sh).
# Detection only — nudges are applied by the design (swarm-boss) agent based on the output.
#
# Differences from trade-boss reference:
#   - Single-host swarm (neo-wsl only). No SSH needed.
#   - Windows agents (sdd-testwin, benchmarker) live in the same tmux session via WSL interop.
#   - V1 scope: WEDGED detection only. IDLE_BAD detection deferred to V2 when priority
#     labels (p0-critical / p1-high) are added to the repo.
#   - 2 stood-down agents (czkawka, accountant) are skipped via STOOD_DOWN list.
#   - dumbo runs in a separate Claude Code session (not in this tmux) and is excluded.
#
# Signals (highest priority first):
#   1. API_ERROR_PATTERN matches the pane tail-12 → WEDGED_API_ERROR.
#   2. Claude Code session JSONL mtime (~/.claude/projects/<encoded-cwd>/<uuid>.jsonl).
#      Updated on every tool call + streamed token. Fresh = actively working;
#      stale = idle. This is the ground truth for "is the agent doing work,"
#      because the TUI animation isn't visible in `tmux capture-pane` and
#      verb-rotation words ("Crunched", "Cogitated", etc.) persist in the
#      buffer long after work stops. SWARM_JSONL_FRESH_S env var sets the
#      freshness threshold (default 120s — generous enough to absorb thinking
#      pauses, tight enough to catch real stalls).
#   3. Fallback: legacy ACTIVE_PATTERN regex against pane tail (for hosts
#      whose JSONL we can't probe — Windows agents whose Claude data lives
#      off-host, or non-Claude CLIs like codex-review).
#
# Usage:
#   ./scripts/swarm-health-check.sh            # full report (every active pane)
#   ./scripts/swarm-health-check.sh --quiet    # only print WEDGED + UNREACHABLE lines (Monitor mode)
#   ./scripts/swarm-health-check.sh --verbose  # also dump pane tails for context
#   ./scripts/swarm-health-check.sh --loop     # repeat every $SWARM_HEALTH_INTERVAL_S (default 3600)
#
# Exit codes (non-loop mode):
#   0 — all agents healthy (IDLE doesn't count as wedged)
#   1 — one or more agents wedged (API error or unreachable)
#
# Output format (one line per relevant agent):
#   STATUS host:agent [reason]
# STATUS ∈ {OK, WEDGED_API_ERROR, UNREACHABLE, IDLE, STOOD_DOWN}
#
# In --quiet mode (Monitor-friendly), the script emits ONE summary line per sweep
# when nothing is wrong (so the Monitor stays alive without spamming), and a
# per-issue line for each WEDGED_API_ERROR / UNREACHABLE pane. IDLE panes are
# NOT surfaced in quiet mode — V1 keeps Monitor signal narrow to wedged-only,
# matching the original design. Run without --quiet to see IDLE.

set -uo pipefail

SESSION="giga-superdeduper"
INTERVAL_S="${SWARM_HEALTH_INTERVAL_S:-3600}"

# JSONL freshness threshold in seconds. mtimes within this window count as
# "actively working"; older mtimes are IDLE. 120s default is chosen to absorb
# normal thinking pauses + occasional tool-call gaps without false-positiving,
# while still catching real stalls within ~2 sweep intervals when used with
# the default 1h interval.
JSONL_FRESH_S="${SWARM_JSONL_FRESH_S:-120}"

# Root of Claude Code's per-project session dirs. Override for non-standard
# installs. Each agent's session dir is "$CLAUDE_PROJECTS_ROOT/<encoded-cwd>".
CLAUDE_PROJECTS_ROOT="${CLAUDE_PROJECTS_ROOT:-$HOME/.claude/projects}"

# Workdir root for the swarm (matches the giga-harness layout). The encoded
# CCD form Claude Code uses is the absolute path with `/` → `-` and a leading
# `-` to anchor.
WORKDIR_ROOT="${SWARM_WORKDIR_ROOT:-/home/neomatrix/.giga/configs/superdeduper/workdirs}"

# Windows we intentionally exclude from the health sweep (bridges, system
# panes, etc.). Anything else in the tmux session is treated as an agent
# pane and classified.
EXCLUDED_WINDOWS=("codex-review-bridge" "codex-review-cli" "design")

# Agents intentionally stood down (per Mick directives). Skipped without flagging.
# Reactivate via Mick's explicit request + remove from this list.
STOOD_DOWN=("czkawka" "accountant")

# Dynamic discovery: AGENTS array is populated from `tmux list-windows` at
# every sweep so reorders / adds / removes are picked up automatically.
# Replaces the prior hardcoded window-index → agent-name map that drifted
# out of sync with the actual tmux layout (e.g. 2026-06-08: river5 moved
# from window 4 → 5, web from 6 → 8; the hardcoded map kept flagging
# windows 4 + 6 as UNREACHABLE and mis-targeted nudges from windows 12,
# 15, 16, 9 to whoever happened to land there).
declare -A AGENTS=()

is_excluded() {
  local name="$1"
  for ex in "${EXCLUDED_WINDOWS[@]}"; do
    if [[ "$name" == "$ex" ]]; then return 0; fi
  done
  return 1
}

discover_agents() {
  AGENTS=()
  local line idx name
  while IFS= read -r line; do
    idx="${line%%:*}"
    name="${line#*:}"
    # Strip trailing tmux "active" marker (`*`) or "last" marker (`-`).
    name="${name%[\*\-]}"
    if is_excluded "$name"; then continue; fi
    AGENTS[$idx]="$name"
  done < <(tmux list-windows -t "${SESSION}" -F '#I:#W' 2>/dev/null)
}

QUIET=0
VERBOSE=0
LOOP=0
for arg in "$@"; do
  case "$arg" in
    --quiet)   QUIET=1 ;;
    --verbose) VERBOSE=1 ;;
    --loop)    LOOP=1 ;;
    *) echo "unknown flag: $arg" >&2; exit 2 ;;
  esac
done

# Patterns that indicate the agent is stuck in a recoverable failure.
# Order matters — most specific first.
API_ERROR_PATTERN='API Error|Server is temporarily limiting|Internal server error|connection refused|connection error|503 Service|429 Too Many|Overloaded'

# Legacy fallback pattern for "agent is actively working" — used only when
# JSONL probing fails (no session dir / Windows agent / non-Claude CLI).
# Includes Claude Code's verb-rotation patterns ("Cogitated", "Cooked", "Baked", etc.)
# that show with active monitor counts ("· N monitor still running").
# Known false-positive risk: these words persist in the pane buffer after
# work stops, so this is the WEAKER signal. JSONL mtime is the primary.
ACTIVE_PATTERN='shipping|building|writing|drafting|amending|reviewing|Smoke testing|Working|Editing|Crunching|Baking|Cooking|Cogitated|Sautéed|Brewed|in progress|streaming|↓ [0-9]+ tokens'

is_stood_down() {
  local agent="$1"
  for sd in "${STOOD_DOWN[@]}"; do
    if [[ "$agent" == "$sd" ]]; then return 0; fi
  done
  return 1
}

# Capture last 40 lines from a window's primary pane.
capture_window() {
  local window="$1"
  tmux capture-pane -t "${SESSION}:${window}.0" -p 2>/dev/null
}

# Encode an absolute path into Claude Code's project-dir form:
#   /home/neomatrix/.giga/configs/superdeduper/workdirs/superdeduper
#   → -home-neomatrix--giga-configs-superdeduper-workdirs-superdeduper
# Replicates the encoding Claude Code uses to derive CLAUDE_PROJECTS_ROOT
# entries from the cwd at session start. The leading `-` anchors; every
# `/` becomes `-`; existing `.` stays as `-` BUT only for hidden-dir
# segments (`.giga` → `-giga` with a second `-` prefix from the parent
# slash). Empirically: matches what's on disk for this swarm.
encode_cwd_to_project_dir() {
  local path="$1"
  # Strip leading slash, then convert each / to - and each . at a segment
  # start to -. The reference implementation in Claude Code does this via
  # path-segment iteration; the regex below produces the same output for
  # all paths we care about (absolute, no traversal markers).
  local encoded="${path//\//-}"
  encoded="${encoded//./-}"
  echo "$encoded"
}

# Map an agent name to its Claude Code session dir. Returns empty if the
# dir doesn't exist (Windows agent, non-Claude CLI, fresh agent that hasn't
# started a session yet, etc.).
agent_session_dir() {
  local agent="$1"
  local cwd="${WORKDIR_ROOT}/${agent}"
  local encoded
  encoded=$(encode_cwd_to_project_dir "$cwd")
  local dir="${CLAUDE_PROJECTS_ROOT}/${encoded}"
  if [[ -d "$dir" ]]; then
    echo "$dir"
  fi
}

# Find the newest JSONL in an agent's session dir. Empty if none found.
latest_agent_jsonl() {
  local agent="$1"
  local dir
  dir=$(agent_session_dir "$agent")
  [[ -z "$dir" ]] && return 0
  # `ls -t` sorts by mtime desc; head -1 picks the freshest.
  ls -t "$dir"/*.jsonl 2>/dev/null | head -1
}

# Return mtime age in seconds for the given file, or empty on failure.
file_age_seconds() {
  local path="$1"
  [[ -f "$path" ]] || return 0
  local mtime now
  mtime=$(stat -c %Y "$path" 2>/dev/null) || return 0
  now=$(date +%s)
  echo $(( now - mtime ))
}

# Classify a single pane's state. Echoes one line: STATUS reason.
classify_pane() {
  local agent="$1"
  local content="$2"

  if is_stood_down "$agent"; then
    echo "STOOD_DOWN intentionally-parked"
    return
  fi

  if [[ -z "$content" ]]; then
    echo "UNREACHABLE pane-capture-returned-empty"
    return
  fi

  # Look at last 12 lines for state cues. Trade-boss reference uses tail-40, but
  # our swarm has API-error discussion in broadcasts that leaves the text in pane
  # scrollback long after the actual error was recovered. Empirically (2026-06-06
  # sweep), tail-40 produced 4 false positives where agents had historical API
  # errors in scrollback but had since recovered. tail-12 reflects current state.
  # If an agent is genuinely wedged, the error stays near the bottom because no
  # further output displaces it. If they've recovered, post-recovery activity
  # (Monitor events, "Crunched for Xs", channel posts) pushes the error past the
  # tail-12 window within seconds.
  local tail12
  tail12=$(printf '%s\n' "$content" | tail -12)

  # 1. API error sweep — highest priority.
  if printf '%s\n' "$tail12" | grep -qE "$API_ERROR_PATTERN"; then
    local sample
    sample=$(printf '%s\n' "$tail12" | grep -oE "$API_ERROR_PATTERN" | head -1)
    echo "WEDGED_API_ERROR matched=\"${sample}\""
    return
  fi

  # 2. JSONL mtime check — primary "is the agent actually working" signal.
  # Claude Code appends to the session JSONL on every tool call + streamed
  # token, so a fresh mtime is the ground truth for "active." Pane text
  # patterns false-positive after work stops because verb-rotation words
  # persist in the buffer (the TUI animation that you SEE as "active" isn't
  # captured by tmux capture-pane).
  local jsonl age
  jsonl=$(latest_agent_jsonl "$agent")
  if [[ -n "$jsonl" ]]; then
    age=$(file_age_seconds "$jsonl")
    if [[ -n "$age" ]]; then
      if (( age < JSONL_FRESH_S )); then
        echo "OK active-work-detected jsonl-age=${age}s"
        return
      else
        echo "IDLE jsonl-age=${age}s no-recent-claude-activity"
        return
      fi
    fi
  fi

  # 3. Fallback: legacy pane-text active-pattern check. Used when JSONL is
  # absent (Windows agents whose Claude data lives off-host; non-Claude CLIs
  # like codex-review; agents that haven't started a session yet). Known to
  # false-positive on stale buffers — kept only because no JSONL means no
  # better signal available.
  if printf '%s\n' "$tail12" | grep -qE "$ACTIVE_PATTERN"; then
    echo "OK active-work-detected pattern-fallback-no-jsonl"
    return
  fi

  # 4. Nothing matched and no JSONL available — pane is in a neutral state.
  # V1 reports OK; V2 will add IDLE_BAD detection once p0/p1 label scheme
  # exists on the repo.
  echo "OK no-issue-detected no-jsonl"
}

# --- main sweep ---

sweep_once() {
  local unhealthy_count=0
  local heading_emitted=0
  local ts
  ts=$(date -Iseconds)

  emit_pane_line() {
    local result="$1"
    local agent="$2"
    local window="$3"
    if [[ "$QUIET" == "1" ]]; then
      case "$result" in
        WEDGED_API_ERROR*|UNREACHABLE*)
          if [[ "$heading_emitted" == "0" ]]; then
            echo "SWARM_HEALTH ${ts} action-needed:"
            heading_emitted=1
          fi
          echo "  ${result} ${agent} (window ${window})"
          ;;
      esac
    else
      printf '%-22s %s\n' "${result}" "${agent} (window ${window})"
    fi
  }

  # Refresh the window→agent map every sweep so layout changes are
  # picked up without an editor round-trip.
  discover_agents

  if [[ "$QUIET" != "1" ]]; then
    echo "swarm-health-check ${ts} — sweeping ${#AGENTS[@]} panes in tmux session '${SESSION}'"
    echo "  jsonl-freshness threshold: ${JSONL_FRESH_S}s"
    echo "---"
  fi

  # Sweep in window-index order for consistent output.
  for window in $(echo "${!AGENTS[@]}" | tr ' ' '\n' | sort -n); do
    local agent="${AGENTS[$window]}"
    local content
    content=$(capture_window "$window")
    local result
    result=$(classify_pane "$agent" "$content")
    emit_pane_line "$result" "$agent" "$window"
    case "$result" in
      WEDGED_API_ERROR*) unhealthy_count=$((unhealthy_count+1)) ;;
      UNREACHABLE*) unhealthy_count=$((unhealthy_count+1)) ;;
    esac
    if [[ "$VERBOSE" == "1" ]]; then
      echo "  ---- tail-15 ----"
      printf '%s\n' "$content" | tail -15 | sed 's/^/  /'
      echo "  ----"
    fi
  done

  if [[ "$QUIET" != "1" ]]; then
    echo "---"
    echo "summary: wedged=${unhealthy_count}"
  elif [[ "$heading_emitted" == "0" ]]; then
    # Quiet mode + nothing wrong: emit a single low-noise heartbeat so the
    # Monitor sees activity and the operator gets one notification per sweep.
    # If even this is too noisy, comment out the next line — but then a
    # dead Monitor process is indistinguishable from a healthy swarm.
    echo "SWARM_HEALTH ${ts} all-clear (wedged=0)"
  fi

  return $(( unhealthy_count > 0 ? 1 : 0 ))
}

if [[ "$LOOP" == "1" ]]; then
  while true; do
    sweep_once || true   # don't exit on non-zero sweep status
    sleep "$INTERVAL_S"
  done
else
  sweep_once
  exit $?
fi
