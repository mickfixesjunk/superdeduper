#!/usr/bin/env bash
# One-time installer for the project's git hooks.
#
# Currently installs:
#   pre-push  → scripts/pre-push-hook.sh
#       guards against the Cargo.lock "path-source" regression
#       that breaks CI under `--locked`.
#
# Symlinks (not copies) so future edits to the script propagate
# without re-running this installer.
#
# Idempotent: re-running just refreshes the symlinks. Safe to run
# after every clone.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOKS_DIR="$REPO_ROOT/.git/hooks"

if [[ ! -d "$HOOKS_DIR" ]]; then
    echo "error: $HOOKS_DIR not found — is this a git checkout?" >&2
    exit 1
fi

install_one() {
    local name="$1"
    local target="$REPO_ROOT/scripts/${name}-hook.sh"
    local link="$HOOKS_DIR/$name"
    if [[ ! -f "$target" ]]; then
        echo "skip $name (no $target)"
        return
    fi
    chmod +x "$target"
    # ln -sf with a relative path so the symlink works regardless
    # of where REPO_ROOT lives on the filesystem.
    ln -sf "../../scripts/${name}-hook.sh" "$link"
    echo "installed $name → $target"
}

install_one pre-push

echo
echo "Done. Push will now refuse to upload commits whose Cargo.lock"
echo "would fail CI's --locked check."
