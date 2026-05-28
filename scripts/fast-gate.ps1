# Thin engine-side dispatcher for the Windows per-build fast-subset gate.
#
# Mirror of scripts/fast-gate.sh for the Windows leg. Spec:
# testdesign/specs/fast-subset-gate.md. Invokes the sdd-testwin-authored
# Windows gate wrapper (preflight + hardware-rows) via SD_HARNESS_DIR.
# The WSL cross-build-drop.sh runs the LINUX gate locally; this .ps1 is
# for the Windows box (sdd-testwin) to gate the dropped Windows EXE.
#
# Contract (mirrors scripts/fast-gate.sh):
#   exit 0 — all gate rows GREEN (build clear to promote)
#   exit 1 — a gate row FAILED (build flagged; do NOT promote)
#   exit 2 — gate is PRESENT but could not run (broken) — do NOT ship ungated
#   exit 3 — gate is ABSENT (harness not staged here) — ship-with-warning,
#            distinct from broken (quality NIT). Until sdd-testwin authors
#            the Windows wrapper, this dispatcher exits 3 (absent).
#
# Env:
#   SD_BIN           — binary under test (REQUIRED; exit 2 if unset)
#   SD_HARNESS_DIR   — dir holding the Windows gate wrapper
#                      (default: sdd-testwin harness in the standard workdir)
#   SD_GATE_WRAPPER  — wrapper filename (default: cli-matrix-fast-gate.ps1)
$ErrorActionPreference = 'Continue'

$harness = if ($env:SD_HARNESS_DIR) { $env:SD_HARNESS_DIR } `
           else { Join-Path $env:USERPROFILE '.giga\configs\superdeduper\workdirs\sdd-testwin\harness' }
$wrapper = if ($env:SD_GATE_WRAPPER) { $env:SD_GATE_WRAPPER } else { 'cli-matrix-fast-gate.ps1' }

function Say($m) { Write-Host "[fast-gate.ps1] $m" }

if (-not $env:SD_BIN) {
    Say 'ERROR: SD_BIN not set — point it at the freshly-built binary. Cannot gate.'
    exit 2
}
# Harness ABSENT (not staged) -> exit 3 (ship-with-warning), distinct
# from a present-but-broken gate (exit 2).
if (-not (Test-Path $harness)) {
    Say "ABSENT: harness dir not found: $harness (set SD_HARNESS_DIR). Gate not staged here."
    exit 3
}
$wrapperPath = Join-Path $harness $wrapper
if (-not (Test-Path $wrapperPath)) {
    Say "ABSENT: gate wrapper not present: $wrapperPath (sdd-testwin authors it). Gate not staged here."
    exit 3
}

$env:SD_HARNESS_DIR = $harness
Say "dispatching to $wrapperPath (SD_BIN=$($env:SD_BIN))"
& $wrapperPath
exit $LASTEXITCODE
