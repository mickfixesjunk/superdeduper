#!/usr/bin/env bash
# #69 — CI smoke-diff feature flags between scripts/cross-build-drop.sh
# and .github/workflows/release.yml.
#
# Why this exists: v0.2.4's GH-release binaries shipped without
# `telemetry similar-images similar-audio` features because
# release.yml's `cargo build --features "gui audio"` had drifted
# from scripts/cross-build-drop.sh's
# `cargo zigbuild --features "gui telemetry similar-images similar-audio"`.
# The local cross-build was correct; the GH release wasn't. Mick
# downloaded + reported missing features (#60-#61 in the v0.2.4
# triage thread; final fix in commit 93ac7dc that made v0.2.5).
#
# This script extracts the feature-flag sets from both files +
# verifies they match on the (platform, bin) axes both files share.
# Run as a CI pre-merge step so a future drift surfaces at PR time
# rather than as a published-bad-binary report.
#
# Usage:
#   scripts/check-feature-flag-consistency.sh
#
# Exit codes:
#   0 — all matched tuples agree
#   1 — at least one tuple's features differ between the two files
#   2 — could not parse a tuple from one of the files (script bug
#       or files restructured beyond what this script understands)
#
# Tuples checked:
#   * (windows, cli)  — features for Win CLI build
#   * (windows, gui)  — features for Win GUI build (incl. `audio`)
#   * (linux, cli)    — features for Linux CLI build
#   * (linux, gui)    — features for Linux GUI build (no `audio`)
#
# macOS-only build in release.yml isn't checked because the local
# cross-build script has no macOS path. Future-engine can extend
# this script when scripts/cross-build-drop.sh grows a macOS arm.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

SCRIPT_FILE="scripts/cross-build-drop.sh"
WORKFLOW_FILE=".github/workflows/release.yml"

for f in "$SCRIPT_FILE" "$WORKFLOW_FILE"; do
  if [[ ! -f "$f" ]]; then
    echo "ERROR: cannot find $f — repo layout changed?" >&2
    exit 2
  fi
done

# Normalise a features string ("telemetry similar-images similar-audio")
# into a deterministic sorted form so order doesn't cause false
# positives.
normalise() {
  echo "$1" | tr ' ' '\n' | sort -u | tr '\n' ' ' | sed 's/ $//'
}

# Extract the feature-string between `--features "..."` for a given
# build-command line. Returns "" if the line has no --features.
extract_features() {
  echo "$1" | grep -oE '\-\-features +"[^"]*"' | sed -E 's/.*"([^"]*)".*/\1/'
}

# Pull (platform, bin) → features tuples from each file.
# Tuple keys: "win-cli", "win-gui", "linux-cli", "linux-gui".

declare -A script_feats
declare -A workflow_feats

# --- scripts/cross-build-drop.sh ---
#
# Layout (verified 2026-05-25):
#   cargo zigbuild ... --target x86_64-pc-windows-gnu --features "..." --bin superdeduper [...]
#   cargo zigbuild ... --target x86_64-pc-windows-gnu --features "..." --bin superdeduper-gui [...]
#   cargo zigbuild ... --target x86_64-unknown-linux-gnu --features "..." --bin superdeduper [...]
#   cargo zigbuild ... --target x86_64-unknown-linux-gnu --features "..." --bin superdeduper-gui [...]

# The `cargo zigbuild` lines end with `\` and the `--target` is on
# the next line. Use `grep -A 1` to pull each pair, then parse the
# two-line block (joined by " ").
while IFS= read -r block; do
  feats=$(extract_features "$block")
  [[ -z "$feats" ]] && continue
  feats=$(normalise "$feats")
  if echo "$block" | grep -q 'windows-gnu\|windows-msvc'; then
    if echo "$block" | grep -q 'bin superdeduper-gui'; then
      script_feats[win-gui]="$feats"
    elif echo "$block" | grep -q 'bin superdeduper'; then
      script_feats[win-cli]="$feats"
    fi
  elif echo "$block" | grep -q 'linux-musl\|linux-gnu'; then
    if echo "$block" | grep -q 'bin superdeduper-gui'; then
      script_feats[linux-gui]="$feats"
    elif echo "$block" | grep -q 'bin superdeduper'; then
      script_feats[linux-cli]="$feats"
    fi
  fi
done < <(grep -A 1 'cargo zigbuild' "$SCRIPT_FILE" | tr '\n' ' ' | sed 's/ -- /\n/g' | sed -e '$a\')

# --- .github/workflows/release.yml ---
#
# Three build jobs: Windows (matrix.target = x86_64-pc-windows-msvc),
# Linux (target hardcoded x86_64-unknown-linux-musl), macOS (skipped).
# Windows job uses ${{ matrix.target }}; Linux job hardcodes its
# target. We pattern-match on the literal target strings.

windows_section=$(awk '/^  build:/{flag=1} /^  build-linux:/{flag=0} flag' "$WORKFLOW_FILE")
linux_section=$(awk '/^  build-linux:/{flag=1} /^  build-macos:/{flag=0} flag' "$WORKFLOW_FILE")

extract_section_features() {
  local section="$1"
  local kind="$2"  # cli or gui
  local pattern
  if [[ "$kind" == "cli" ]]; then
    pattern='cargo build.*--bin superdeduper$'
  else
    pattern='cargo build.*--bin superdeduper-gui'
  fi
  local line
  line=$(echo "$section" | grep -E "$pattern" | head -1 || true)
  extract_features "$line"
}

if [[ -n "$windows_section" ]]; then
  workflow_feats[win-cli]=$(normalise "$(extract_section_features "$windows_section" cli)")
  workflow_feats[win-gui]=$(normalise "$(extract_section_features "$windows_section" gui)")
fi
if [[ -n "$linux_section" ]]; then
  workflow_feats[linux-cli]=$(normalise "$(extract_section_features "$linux_section" cli)")
  workflow_feats[linux-gui]=$(normalise "$(extract_section_features "$linux_section" gui)")
fi

# Compare.
declare -i mismatch=0
for key in win-cli win-gui linux-cli linux-gui; do
  s="${script_feats[$key]:-}"
  w="${workflow_feats[$key]:-}"
  if [[ -z "$s" ]]; then
    echo "ERROR: could not extract '$key' features from $SCRIPT_FILE" >&2
    exit 2
  fi
  if [[ -z "$w" ]]; then
    echo "ERROR: could not extract '$key' features from $WORKFLOW_FILE" >&2
    exit 2
  fi
  if [[ "$s" != "$w" ]]; then
    mismatch=1
    cat >&2 <<EOF
MISMATCH on $key:
  $SCRIPT_FILE: '$s'
  $WORKFLOW_FILE: '$w'
EOF
  else
    echo "OK $key: '$s'"
  fi
done

if (( mismatch == 1 )); then
  cat >&2 <<EOF

Feature-flag drift detected between local cross-build script + GH
release workflow. The v0.2.4 missing-features regression was this
exact pattern. Sync the feature strings before merging.

Update either:
  - $SCRIPT_FILE (local WSL cross-build to /mnt/c/...)
  - $WORKFLOW_FILE (GH Actions release pipeline)

so each (platform, bin) tuple has the same feature set.
EOF
  exit 1
fi

echo
echo "All feature-flag tuples match between $SCRIPT_FILE and $WORKFLOW_FILE."