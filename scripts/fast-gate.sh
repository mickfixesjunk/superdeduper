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
# Contract:
#   exit 0 — all gate rows GREEN (build clear to promote)
#   exit 1 — a gate row FAILED (build flagged; do NOT promote the drop)
#   exit 2 — gate is PRESENT but could not run (broken: SD_BIN misconfig
#            or the wrapper itself reported can't-run) — build must NOT
#            ship ungated on this
#   exit 3 — gate is ABSENT (harness not staged on this box) — distinct
#            from 2 so the build can ship-with-warning rather than block
#            when the test harness simply isn't installed here
#            (quality NIT: don't conflate "not installed" with "broke").
# Codes 0/1/2 mirror the executor wrapper's contract; exit 3 is this
# engine-internal dispatcher's addition for the absent-vs-broken split.
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
# Harness ABSENT (not staged on this box) -> exit 3 (ship-with-warning),
# NOT exit 2 (which means a present-but-broken gate).
if [[ ! -d "$HARNESS_DIR" ]]; then
  say "ABSENT: harness dir not found: $HARNESS_DIR (set SD_HARNESS_DIR). Gate not staged here."
  exit 3
fi
if [[ ! -f "$HARNESS_DIR/$WRAPPER" ]]; then
  say "ABSENT: gate wrapper not present: $HARNESS_DIR/$WRAPPER. Gate not staged here."
  exit 3
fi

export SD_BIN SD_HARNESS_DIR="$HARNESS_DIR"
say "dispatching to $HARNESS_DIR/$WRAPPER (SD_BIN=$SD_BIN)"
# The wrapper's exit code IS this script's exit code (last command), so
# its 0/1/2 propagate to the build-hook caller unchanged. A wrapper exit
# 2 (present-but-can't-run) stays 2 here = broken, distinct from the
# exit-3 absent case handled above.
bash "$HARNESS_DIR/$WRAPPER"
