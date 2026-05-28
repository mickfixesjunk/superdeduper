#!/usr/bin/env bash
# Thin engine-side dispatcher for the Linux per-build fast-subset gate.
#
# Spec: testdesign/specs/fast-subset-gate.md (design APPROVED 2026-05-28).
# Engine owns this thin wrapper + the build-hook placement
# (scripts/cross-build-drop.sh calls it post-drop). The actual gate
# logic + the constituent row scripts live in the executor harness
# (testrunner/harness/), located via SD_HARNESS_DIR — so the gate
# composition stays where testrunner maintains it, and only the
# build-trigger lives with the build.
#
# Contract (propagated verbatim from the harness wrapper):
#   exit 0 — all gate rows GREEN (build clear to promote)
#   exit 1 — a gate row FAILED (build flagged; do NOT promote the drop)
#   exit 2 — gate could not run (no SD_BIN / harness not staged on this box)
#
# Env:
#   SD_BIN           — binary under test (REQUIRED; exit 2 if unset)
#   SD_HARNESS_DIR   — dir holding the executor wrapper + row scripts
#                      (default: testrunner harness in the standard workdir)
#   SD_GATE_WRAPPER  — wrapper filename within SD_HARNESS_DIR
#                      (default: cli-matrix-fast-gate.sh)
#   FAST_GATE_CORPUS — test60 corpus root; passed through if set (the
#                      wrapper defaults + SKIPs the containment row when absent)
set -uo pipefail

DEFAULT_HARNESS="$HOME/.giga/configs/superdeduper/workdirs/testrunner/harness"
HARNESS_DIR="${SD_HARNESS_DIR:-$DEFAULT_HARNESS}"
WRAPPER="${SD_GATE_WRAPPER:-cli-matrix-fast-gate.sh}"

say() { echo "[fast-gate.sh] $*"; }

if [[ -z "${SD_BIN:-}" ]]; then
  say "ERROR: SD_BIN not set — point it at the freshly-built binary. Cannot gate."
  exit 2
fi
if [[ ! -d "$HARNESS_DIR" ]]; then
  say "WARN: harness dir not found: $HARNESS_DIR (set SD_HARNESS_DIR). Gate cannot run."
  exit 2
fi
if [[ ! -f "$HARNESS_DIR/$WRAPPER" ]]; then
  say "WARN: gate wrapper not present: $HARNESS_DIR/$WRAPPER. Gate cannot run."
  exit 2
fi

export SD_BIN SD_HARNESS_DIR="$HARNESS_DIR"
say "dispatching to $HARNESS_DIR/$WRAPPER (SD_BIN=$SD_BIN)"
# The wrapper's exit code IS this script's exit code (last command),
# so 0/1/2 propagate to the build-hook caller unchanged.
bash "$HARNESS_DIR/$WRAPPER"
