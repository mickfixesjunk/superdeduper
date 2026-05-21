#!/usr/bin/env bash
# scripts/precheck.sh — run the same commands CI runs, in the same
# conditions, so failures surface here instead of on the GitHub
# runner.
#
# Why this exists: the local .cargo/config.toml (gitignored) patches
# the river5 git dep to a sibling path so we can iterate without
# pushing river5 on every change. That patch makes `cargo … --locked`
# fail because Cargo wants to update Cargo.lock to switch sources.
# CI doesn't have the patch, so it doesn't trip the same issue. This
# script:
#
#   1. Stashes .cargo/config.toml (if present) → CI-equivalent env
#   2. Runs fmt --check, clippy with -D warnings + --locked, and
#      tests with --locked
#   3. Restores the patch so subsequent local builds keep using the
#      sibling river5 working copy
#
# Usage: just run `scripts/precheck.sh` before `git push`. Exit code
# is non-zero if any check failed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

CONFIG=".cargo/config.toml"
STASH="$CONFIG.precheck-stash"

# Move the local patch out of the way for the duration of the run.
# We do this even if the script fails halfway so the patch is always
# put back; that's what the trap is for.
stash_patch() {
    if [[ -f "$CONFIG" ]]; then
        mv "$CONFIG" "$STASH"
    fi
}
restore_patch() {
    if [[ -f "$STASH" ]]; then
        mv "$STASH" "$CONFIG"
    fi
}
trap restore_patch EXIT

stash_patch

echo "=== cargo fmt --all -- --check ==="
cargo fmt --all -- --check

echo "=== cargo clippy --workspace --all-targets --locked -- -D warnings ==="
cargo clippy --workspace --all-targets --locked -- -D warnings

echo "=== cargo test --workspace --locked --features gui ==="
cargo test --workspace --locked --features gui

echo
echo "✅ All CI checks passed locally. Safe to push."
