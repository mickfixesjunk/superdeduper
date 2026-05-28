#!/usr/bin/env bash
# Pre-cut release-integrity gate.
#
# Surfaces "stranded verified work": commits living on a fix/* or
# feat/* branch that never made it onto the release branch, where the
# branch's GitHub issue is already CLOSED. That combination —
# closed-issue + unmerged commits — is the recurring gap that shipped
# releases missing verified work (#88 Phase 1, #58, #108-extended,
# #60-hardening, #33 — all "closed-via-verification, merge-to-release
# silently skipped").
#
# Run this BEFORE cutting a release tag. Exit code:
#   0 — no closed-issue branches have unmerged commits (clear to cut)
#   1 — candidate stranded branches found (eyeball each before cutting)
#
# Why git cherry (not merge-base --is-ancestor): cherry compares by
# patch-id, so squash/cherry-pick/re-implemented merges correctly count
# as "present." is-ancestor over-reports every non-fast-forward merge.
# Even so, cherry can't tell a re-implementation from a true strand, so
# this is a HUMAN gate: it narrows ~60 branches to the handful worth a
# 30-second artifact check, it does not auto-decide.
#
# Requires: git, gh (for issue-state lookup; degrades to "unknown" if
# gh is unavailable).
#
# Usage: scripts/release-integrity-check.sh [release-ref]
#   release-ref defaults to origin/release/v0.2.16
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

RELEASE_REF="${1:-origin/release/v0.2.16}"

git fetch origin --quiet 2>/dev/null || true

if ! git rev-parse --verify "$RELEASE_REF" >/dev/null 2>&1; then
  echo "error: release ref '$RELEASE_REF' not found" >&2
  exit 2
fi

have_gh=0
if command -v gh >/dev/null 2>&1; then
  have_gh=1
fi

# Look up a GitHub issue's state, cached per-run. Echoes one of:
# OPEN | CLOSED | NONE (no such issue) | UNKNOWN (no gh / lookup failed).
declare -A ISSUE_STATE_CACHE
issue_state() {
  local num="$1"
  if [[ -z "$num" ]]; then echo "NONE"; return; fi
  if [[ -n "${ISSUE_STATE_CACHE[$num]:-}" ]]; then
    echo "${ISSUE_STATE_CACHE[$num]}"; return
  fi
  local state="UNKNOWN"
  if [[ "$have_gh" -eq 1 ]]; then
    state="$(gh issue view "$num" --json state -q .state 2>/dev/null || echo UNKNOWN)"
    [[ -z "$state" ]] && state="UNKNOWN"
  fi
  ISSUE_STATE_CACHE[$num]="$state"
  echo "$state"
}

stranded=0
reviewed=0

echo "==> Release-integrity gate vs ${RELEASE_REF}"
echo "    (closed-issue branches with unmerged commits = verify before cutting)"
echo

for branch in $(git branch -r --list 'origin/fix/*' 'origin/feat/*' | sed 's/^[ *]*//'); do
  # Count commits on the branch not present on release (patch-id).
  unmerged="$(git cherry "$RELEASE_REF" "$branch" 2>/dev/null | grep -c '^+')"
  [[ "$unmerged" -eq 0 ]] && continue

  # Extract a leading issue number from the branch name
  # (fix/58-..., feat/108-...). Branches with no number (experiments)
  # are reported at low signal.
  short="${branch#origin/}"
  num="$(echo "$short" | sed -n 's#^\(fix\|feat\)/\([0-9]\+\).*#\2#p')"
  state="$(issue_state "$num")"

  if [[ "$state" == "CLOSED" ]]; then
    stranded=$((stranded + 1))
    echo "  [STRANDED?] $short — issue #$num CLOSED, $unmerged commit(s) not on release"
    git cherry -v "$RELEASE_REF" "$branch" 2>/dev/null | grep '^+' | sed 's/^/             /'
  else
    reviewed=$((reviewed + 1))
  fi
done

echo
echo "==> $stranded closed-issue branch(es) with unmerged commits (verify each)"
echo "    $reviewed other branch(es) with unmerged commits (open/experimental — lower signal)"

if [[ "$stranded" -gt 0 ]]; then
  cat >&2 <<EOF

For each [STRANDED?] branch above, confirm whether its work is genuinely
absent from ${RELEASE_REF} (truly stranded — merge it before cutting) or
present via a re-implementation git cherry couldn't match (safe to ignore).
A quick \`git grep <artifact> ${RELEASE_REF}\` settles it.

Gate FAILED: resolve or consciously accept these before tagging a release.
EOF
  exit 1
fi

echo "Gate PASSED: no closed-issue branches have unmerged commits."
