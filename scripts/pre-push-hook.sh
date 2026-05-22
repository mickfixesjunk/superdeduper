#!/usr/bin/env bash
# Git pre-push hook that guards against the recurring "Cargo.lock
# resolved to path source" regression.
#
# Background: the project's local-development convenience patch in
# `.cargo/config.toml` (gitignored) redirects the river5 git dep to
# `../river5`. Any `cargo` invocation with that patch active
# rewrites `Cargo.lock` so river5's `source = …` line is removed
# (path source has no source line). CI runs without the patch and
# fails under `--locked` because the lock then needs to be
# re-resolved against the git URL — which `--locked` forbids.
#
# This hook detects the path-source state at push time and aborts
# with a clear remediation message. Run `scripts/precheck.sh` to
# refresh the lock against the git source, stage it, amend (or new
# commit), and push again.
#
# Install:
#     ln -sf ../../scripts/pre-push-hook.sh .git/hooks/pre-push
# Or run scripts/install-hooks.sh once after cloning.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
LOCK="$REPO_ROOT/Cargo.lock"

if [[ ! -f "$LOCK" ]]; then
    exit 0
fi

# Find the river5 package block in Cargo.lock. The block is a
# `[[package]]` table; look for the four lines following the
# `name = "river5"` marker. A git-source package always has a
# `source = "git+…"` line; a path-source package omits the source
# entirely.
block=$(awk '
    /^name = "river5"$/ { in_pkg = 1; next }
    in_pkg && /^name = / { in_pkg = 0 }
    in_pkg { print }
' "$LOCK")

if [[ -z "$block" ]]; then
    # No river5 entry at all — the dep is genuinely absent (someone
    # removed it intentionally). Allow the push.
    exit 0
fi

if echo "$block" | grep -q '^source = "git+'; then
    # Lock is git-sourced — CI-safe. Allow push.
    exit 0
fi

cat >&2 <<'EOF'
❌ Cargo.lock has river5 resolved to a *path source* (no `source =` line).
   CI runs without the local `.cargo/config.toml` patch and will fail
   on `--locked`.

   Fix:
       scripts/precheck.sh        # refreshes the lock against git
       git add Cargo.lock
       git commit --amend --no-edit   # OR new commit
       git push                       # retry

   The precheck script temporarily stashes the path patch, runs the
   same checks CI runs (fmt, clippy --locked, test --locked), and
   leaves Cargo.lock pointing at the git source. The hook will then
   accept the push.
EOF
exit 1
