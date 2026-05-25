#!/usr/bin/env bash
# Cross-build the four release binaries (Linux CLI + GUI, Windows
# CLI + GUI via cargo-zigbuild) and drop them to BOTH:
#
#   1. /mnt/c/Users/NeoMatrix/sdd-builds/<sha>/     (archive-by-sha)
#   2. /mnt/c/Users/NeoMatrix/projects/mickfixesjunk/  (latest, overwrite)
#
# Locked convention per design 2026-05-24T21:45Z. Both destinations
# every build, no exceptions.
#
# Usage: scripts/cross-build-drop.sh
#   Assumes `cargo`, `cargo-zigbuild`, and a Windows GNU target are
#   already installed. Writes everything from the current working
#   tree (HEAD's sha is recorded in the archive dir name).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

SHA="$(git rev-parse --short=7 HEAD)"
BRANCH="$(git rev-parse --abbrev-ref HEAD)"
ARCHIVE_DIR="/mnt/c/Users/NeoMatrix/sdd-builds/${SHA}"
LATEST_DIR="/mnt/c/Users/NeoMatrix/projects/mickfixesjunk"

if [[ -e "$ARCHIVE_DIR" ]]; then
  echo "warning: ${ARCHIVE_DIR} already exists; not overwriting the archive copy" >&2
  echo "         (the latest-path drop will still happen; archive untouched)" >&2
fi

# Diff vs main when building from a non-main branch — surfaces the
# #37 regression class (a stale feature branch overwriting the
# latest-path binary, missing features that already landed on main).
# Fetch quietly so the count is accurate even if main has moved
# upstream since the last local sync.
if [[ "$BRANCH" != "main" && "$BRANCH" != "HEAD" ]]; then
  git fetch origin main --quiet 2>/dev/null || true
  # Pick the freshest main reference available — `origin/main` if the
  # fetch succeeded, else local `main` if it exists, else skip the
  # diff (shallow clone, no upstream, etc.).
  MAIN_REF=""
  if git rev-parse --verify origin/main >/dev/null 2>&1; then
    MAIN_REF="origin/main"
  elif git rev-parse --verify main >/dev/null 2>&1; then
    MAIN_REF="main"
  fi
  if [[ -n "$MAIN_REF" ]]; then
    AHEAD="$(git rev-list --count "$MAIN_REF..HEAD")"
    BEHIND="$(git rev-list --count "HEAD..$MAIN_REF")"
    if [[ "$BEHIND" -gt 0 ]]; then
      cat >&2 <<EOF
==================================================================
WARNING: building from branch '${BRANCH}'
         ${AHEAD} commit(s) ahead of ${MAIN_REF}
         ${BEHIND} commit(s) BEHIND ${MAIN_REF}

This drop is about to overwrite the latest-path binary at
${LATEST_DIR} with a build that's missing ${BEHIND} commits worth of
work from main. If those commits include user-visible features
(branding, GUI surfaces, etc.) this will look like a regression.

Resolve by rebasing the branch onto main BEFORE running this script:
    git checkout ${BRANCH}
    git rebase main
    scripts/cross-build-drop.sh

Continuing in 5 seconds — Ctrl-C to abort.
==================================================================
EOF
      sleep 5
    elif [[ "$AHEAD" -gt 0 ]]; then
      echo "==> branch '${BRANCH}' is ${AHEAD} commit(s) ahead of ${MAIN_REF}, no commits behind" >&2
    fi
  fi
fi

echo "==> cross-build for ${SHA} (branch: ${BRANCH})"
echo "    archive: ${ARCHIVE_DIR}"
echo "    latest:  ${LATEST_DIR}"

echo "==> Linux CLI (telemetry; musl static — no glibc dependency)"
cargo zigbuild --release --features "telemetry similar-images similar-audio" --bin superdeduper \
  --target x86_64-unknown-linux-musl

echo "==> Linux GUI (gui + telemetry; musl static — Ubuntu 20.04+ compat)"
cargo zigbuild --release --features "gui telemetry similar-images similar-audio" --bin superdeduper-gui \
  --target x86_64-unknown-linux-musl

echo "==> Windows CLI (telemetry; cross-compile via cargo-zigbuild)"
cargo zigbuild --release --features "telemetry similar-images similar-audio" --bin superdeduper \
  --target x86_64-pc-windows-gnu

echo "==> Windows GUI (gui + telemetry + audio; cross-compile via cargo-zigbuild)"
cargo zigbuild --release --features "gui telemetry audio similar-images similar-audio" --bin superdeduper-gui \
  --target x86_64-pc-windows-gnu

# Stage into the archive dir (never overwrite an existing per-sha
# folder). Stage into the latest dir (always overwrite).
mkdir -p "$ARCHIVE_DIR" "$LATEST_DIR"

declare -a BINARIES=(
  "target/x86_64-unknown-linux-musl/release/superdeduper           superdeduper-linux-x86_64"
  "target/x86_64-unknown-linux-musl/release/superdeduper-gui       superdeduper-gui-linux-x86_64"
  "target/x86_64-pc-windows-gnu/release/superdeduper.exe           superdeduper-windows-x86_64.exe"
  "target/x86_64-pc-windows-gnu/release/superdeduper-gui.exe       superdeduper-gui-windows-x86_64.exe"
)

for entry in "${BINARIES[@]}"; do
  src="$(echo "$entry" | awk '{print $1}')"
  dst="$(echo "$entry" | awk '{print $2}')"
  cp -v "$src" "$ARCHIVE_DIR/$dst"
  cp -v "$src" "$LATEST_DIR/$dst"
done

# SHA256SUMS — same content in both destinations.
(cd "$ARCHIVE_DIR" && sha256sum \
  superdeduper-linux-x86_64 \
  superdeduper-gui-linux-x86_64 \
  superdeduper-windows-x86_64.exe \
  superdeduper-gui-windows-x86_64.exe > SHA256SUMS)
cp -v "$ARCHIVE_DIR/SHA256SUMS" "$LATEST_DIR/SHA256SUMS"

echo "==> drop complete"
echo "archive: ${ARCHIVE_DIR}"
ls -la "$ARCHIVE_DIR"
echo "latest:  ${LATEST_DIR}"
ls -la "$LATEST_DIR" | grep -E "superdeduper|SHA256"
