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
ARCHIVE_DIR="/mnt/c/Users/NeoMatrix/sdd-builds/${SHA}"
LATEST_DIR="/mnt/c/Users/NeoMatrix/projects/mickfixesjunk"

if [[ -e "$ARCHIVE_DIR" ]]; then
  echo "warning: ${ARCHIVE_DIR} already exists; not overwriting the archive copy" >&2
  echo "         (the latest-path drop will still happen; archive untouched)" >&2
fi

echo "==> cross-build for ${SHA}"
echo "    archive: ${ARCHIVE_DIR}"
echo "    latest:  ${LATEST_DIR}"

echo "==> Linux CLI (telemetry)"
cargo build --release --features "telemetry" --bin superdeduper

echo "==> Linux GUI (gui + telemetry; no audio — alsa-sys often missing on WSL)"
cargo build --release --features "gui telemetry" --bin superdeduper-gui

echo "==> Windows CLI (telemetry; cross-compile via cargo-zigbuild)"
cargo zigbuild --release --features "telemetry" --bin superdeduper \
  --target x86_64-pc-windows-gnu

echo "==> Windows GUI (gui + telemetry + audio; cross-compile via cargo-zigbuild)"
cargo zigbuild --release --features "gui telemetry audio" --bin superdeduper-gui \
  --target x86_64-pc-windows-gnu

# Stage into the archive dir (never overwrite an existing per-sha
# folder). Stage into the latest dir (always overwrite).
mkdir -p "$ARCHIVE_DIR" "$LATEST_DIR"

declare -a BINARIES=(
  "target/release/superdeduper                                superdeduper-linux-x86_64"
  "target/release/superdeduper-gui                            superdeduper-gui-linux-x86_64"
  "target/x86_64-pc-windows-gnu/release/superdeduper.exe      superdeduper-windows-x86_64.exe"
  "target/x86_64-pc-windows-gnu/release/superdeduper-gui.exe  superdeduper-gui-windows-x86_64.exe"
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
