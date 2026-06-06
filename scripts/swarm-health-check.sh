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
# Usage:
#   ./scripts/swarm-health-check.sh            # full report (every active pane)
#   ./scripts/swarm-health-check.sh --quiet    # only print WEDGED + UNREACHABLE lines (Monitor mode)
#   ./scripts/swarm-health-check.sh --verbose  # also dump pane tails for context
#   ./scripts/swarm-health-check.sh --loop     # repeat every $SWARM_HEALTH_INTERVAL_S (default 3600)
#
# Exit codes (non-loop mode):
#   0 — all agents healthy
#   1 — one or more agents wedged (API error or unreachable)
#
# Output format (one line per relevant agent):
#   STATUS host:agent [reason]
# STATUS ∈ {OK, WEDGED_API_ERROR, UNREACHABLE, STOOD_DOWN}
#
# In --quiet mode (Monitor-friendly), the script emits ONE summary line per sweep
# when nothing is wrong (so the Monitor stays alive without spamming), and a
# per-issue line for each WEDGED_API_ERROR / UNREACHABLE pane.

set -uo pipefail

SESSION="giga-superdeduper"
INTERVAL_S="${SWARM_HEALTH_INTERVAL_S:-3600}"

# Window index : agent name. Matches the layout from `tmux list-windows -t giga-superdeduper`.
# Update if the swarm topology changes (use `giga add-agent` + re-run `giga launch` to grow).
declare -A AGENTS=(
  [0]="superdeduper"
  [1]="design"
  [2]="testdesign"
  [3]="testrunner"
  [4]="river5"
  [5]="czkawka"
  [6]="web"
  [7]="quality"
  [8]="research"
  [9]="achievements"
  [10]="accountant"
  [11]="dev-health"
  [12]="sdd-testwin"
  [13]="infosec"
  [14]="giga"
  [15]="superdeduper-overflow"
  [16]="benchmarker"
)

# Agents intentionally stood down (per Mick directives). Skipped without flagging.
# Reactivate via Mick's explicit request + remove from this list.
STOOD_DOWN=("czkawka" "accountant")

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

# Patterns for "agent is actively working" — these win over any neutral-state detection.
# Includes Claude Code's verb-rotation patterns ("Cogitated", "Cooked", "Baked", etc.)
# that show with active monitor counts ("· N monitor still running").
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

  # 2. Active-work check — if the agent is mid-task, classify OK.
  if printf '%s\n' "$tail12" | grep -qE "$ACTIVE_PATTERN"; then
    echo "OK active-work-detected"
    return
  fi

  # 3. Nothing matched — pane is in a neutral state. V1 reports OK; V2 will add
  # IDLE_BAD detection once p0/p1 label scheme exists on the repo.
  echo "OK no-issue-detected"
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

  if [[ "$QUIET" != "1" ]]; then
    echo "swarm-health-check ${ts} — sweeping ${#AGENTS[@]} panes in tmux session '${SESSION}'"
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
