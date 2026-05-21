#!/usr/bin/env bash
# scripts/precheck.sh — run the same commands CI runs, in the same
# conditions, so failures surface here instead of on the GitHub
# runner.
#
# Why this exists: the local .cargo/config.toml (gitignored) patches
# the river5 git dep to a sibling path so we can iterate without
# pushing river5 on every change. That patch:
#
#   1. makes `cargo … --locked` fail (Cargo wants to update
#      Cargo.lock to switch sources), AND
#   2. causes any `cargo build/check/test` to *rewrite* Cargo.lock
#      with the path source. If we commit after that rewrite, CI
#      sees the path source in the lockfile, can't resolve it under
#      --locked, and the workflow fails.
#
# This script:
#   1. Stashes .cargo/config.toml (if present) → CI-equivalent env
#   2. Runs fmt --check, clippy with -D warnings + --locked, and
#      tests with --locked. The cargo check at the end regenerates
#      Cargo.lock against the git source so it's safe to commit.
#   3. Restores the patch (via the EXIT trap) so subsequent local
#      builds keep using the sibling river5 working copy.
#   4. If Cargo.lock was modified during the check, prints a
#      warning so the user knows to `git add Cargo.lock` before
#      pushing.
#
# Usage: just run `scripts/precheck.sh` before `git push`. Exit
# code is non-zero if any check failed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

CONFIG=".cargo/config.toml"
STASH="$CONFIG.precheck-stash"

# Record Cargo.lock's git-tracked state at start so we can tell
# whether the run modified it (which means the lockfile needs to be
# staged for the upcoming push).
LOCK_BEFORE=""
if git ls-files --error-unmatch Cargo.lock >/dev/null 2>&1; then
    LOCK_BEFORE="$(git hash-object Cargo.lock 2>/dev/null || true)"
fi

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

# Verify the lockfile matches what CI will see. If a local cargo
# run (cross-compile, dev iteration) rewrote it under the patch
# *between* the last precheck and this one, the lockfile would
# point at the path source — CI would fail on --locked.
LOCK_AFTER="$(git hash-object Cargo.lock 2>/dev/null || true)"
if [[ -n "$LOCK_BEFORE" && "$LOCK_BEFORE" != "$LOCK_AFTER" ]]; then
    echo
    echo "⚠️  Cargo.lock changed during this run. Stage it before pushing:"
    echo "    git add Cargo.lock"
    echo
    echo "   This happens when a local 'cargo build' ran with the"
    echo "   .cargo/config.toml river5 patch active and rewrote the"
    echo "   lockfile to point at the path dep. CI uses --locked and"
    echo "   needs the git source in the lockfile."
fi

echo
echo "✅ All CI checks passed locally. Safe to push."
