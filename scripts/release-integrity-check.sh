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
#   0 — every closed-issue branch's work is present in the release (no strands)
#   1 — TRUE strands and/or auto-classify-inconclusive branches found (resolve)
#   2 — bad release ref
#
# Why git cherry (not merge-base --is-ancestor): cherry compares by
# patch-id, so squash/cherry-pick/re-implemented merges correctly count
# as "present." is-ancestor over-reports every non-fast-forward merge.
#
# Patch-id alone can't tell a re-implementation from a true strand, so this
# gate ALSO runs dev-health's distinctive-identifier method (the one that
# caught the #56/#126 + #134/#137 clusters loose-grep / patch-id missed): for
# each closed-issue branch with unmerged commits, extract its added
# definitions (fn/struct/enum/const) and `git grep` them in the release tree.
# All present -> CLEARED (re-implementation); all absent -> STRANDED (block);
# partial / no-marker -> needs-human-verify (block, conservatively). This
# auto-clears the recurring false-positives and AUTO-BLOCKS a cut that's
# missing closed-and-verified work, instead of relying on a human to eyeball
# each candidate.
#
# Requires: git, gh (for issue-state lookup; degrades to "unknown" if
# gh is unavailable).
#
# Usage: scripts/release-integrity-check.sh [release-ref]
#   release-ref defaults to HEAD — the local, about-to-be-tagged state.
#   This is deliberate: a pre-cut gate must judge what you're ABOUT to
#   tag (including local merges not yet pushed), not the stale remote.
#   Pass origin/release/v0.2.16 explicitly to audit the pushed remote.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

RELEASE_REF="${1:-HEAD}"

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

# dev-health distinctive-identifier classifier. Given a closed-issue branch
# with unmerged commits, decide whether its work is GENUINELY absent from the
# release ref (true strand -> BLOCK) or present via a re-implementation that
# git-cherry's patch-id couldn't match (safe -> CLEAR). This automates the
# manual `git grep <artifact>` check that caught the #56/#126 + #134/#137
# clusters loose-grep / patch-id alone missed.
#
# Method: extract DISTINCTIVE added markers (fn/struct/enum/const/static
# DEFINITIONS) from the branch's unmerged commits, then `git grep` each in the
# release ref's tree. Conservative: auto-CLEAR only when EVERY marker is
# present (confident re-impl); STRANDED when none are present (confident
# strand); AMBIGUOUS (partial, or no extractable markers e.g. comment-only)
# stays a human-gate flag. Echoes: CLEARED | STRANDED | AMBIGUOUS, then a
# detail line on stderr.
classify_stranding() {
  local branch="$1" ref="$2"
  local commits markers total=0 absent=0 absent_list=""
  commits="$(git cherry "$ref" "$branch" 2>/dev/null | awk '/^\+/{print $2}')"
  [[ -z "$commits" ]] && { echo "AMBIGUOUS"; return; }
  # Added definition identifiers across the unmerged commits' diffs.
  markers="$(
    for c in $commits; do git show --no-color "$c" 2>/dev/null; done \
      | grep -E '^\+' \
      | grep -oE '(fn|struct|enum|trait) [A-Za-z_][A-Za-z0-9_]+|(const|static) [A-Z_][A-Z0-9_]+' \
      | awk '{print $2}' | sort -u
  )"
  [[ -z "$markers" ]] && { echo "AMBIGUOUS"; return; }
  while IFS= read -r m; do
    [[ -z "$m" ]] && continue
    total=$((total + 1))
    if ! git grep -qwI "$m" "$ref" -- '*.rs' 2>/dev/null; then
      absent=$((absent + 1))
      absent_list="${absent_list} $m"
    fi
  done <<< "$markers"
  if [[ "$absent" -eq 0 ]]; then
    echo "CLEARED"
  elif [[ "$absent" -eq "$total" ]]; then
    echo "STRANDED"
  else
    echo "AMBIGUOUS"
    echo "    partial: ${absent}/${total} markers absent in ${ref}:${absent_list}" >&2
  fi
}

stranded=0      # confident true-strands (definitions absent) -> BLOCK
ambiguous=0     # partial / no-marker -> human gate -> BLOCK (conservative)
cleared=0       # confident re-implementations -> safe
reviewed=0      # non-closed-issue branches -> low signal

echo "==> Release-integrity gate vs ${RELEASE_REF}"
echo "    (closed-issue branches with unmerged commits, auto-classified by"
echo "     distinctive-identifier presence — dev-health method)"
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
    verdict="$(classify_stranding "$branch" "$RELEASE_REF")"
    case "$verdict" in
      CLEARED)
        cleared=$((cleared + 1))
        echo "  [cleared]   $short — #$num CLOSED, $unmerged unmerged, but its added identifiers are PRESENT in ${RELEASE_REF} (re-implementation)"
        ;;
      STRANDED)
        stranded=$((stranded + 1))
        echo "  [STRANDED]  $short — #$num CLOSED, $unmerged unmerged, and its added identifiers are ABSENT from ${RELEASE_REF} (TRUE strand — merge before cutting)"
        git cherry -v "$RELEASE_REF" "$branch" 2>/dev/null | grep '^+' | sed 's/^/                /'
        ;;
      *)
        ambiguous=$((ambiguous + 1))
        echo "  [verify]    $short — #$num CLOSED, $unmerged unmerged; auto-classify inconclusive (partial / no-marker) — human check required"
        git cherry -v "$RELEASE_REF" "$branch" 2>/dev/null | grep '^+' | sed 's/^/                /'
        ;;
    esac
  else
    reviewed=$((reviewed + 1))
  fi
done

echo
echo "==> ${stranded} TRUE-strand · ${ambiguous} needs-verify · ${cleared} cleared(re-impl) closed-issue branch(es)"
echo "    ${reviewed} other branch(es) with unmerged commits (open/experimental — lower signal)"

if [[ "$stranded" -gt 0 || "$ambiguous" -gt 0 ]]; then
  cat >&2 <<EOF

[STRANDED] branches are TRUE strands (added identifiers absent from
${RELEASE_REF}) — merge them before tagging, or the release ships missing
closed-and-verified work (the #56/#126 + #134/#137 class).
[verify] branches are auto-classify-inconclusive (partial overlap, or a
comment/doc-only change with no extractable identifier) — settle each with
\`git grep <distinctive-snippet> ${RELEASE_REF}\` and merge-or-accept.

Gate FAILED: resolve [STRANDED] (and clear [verify]) before tagging a release.
EOF
  exit 1
fi

echo "Gate PASSED: every closed-issue branch's work is present in ${RELEASE_REF} (no true strands)."
