#!/usr/bin/env bash
# scripts/iothread-sweep.sh — sweep `--io-threads` across a range and
# emit a comparison table. Linux-only as a complement to the
# PowerShell version for Windows.
#
# Usage:
#   scripts/iothread-sweep.sh <scan-path>
#   scripts/iothread-sweep.sh /tmp/scan-bulk
#
# Output: one timing block per thread count plus a trailing summary
# table. Use this to answer "does oversubscription still help?"
# without committing to IOCP work.

set -euo pipefail

TARGET="${1:?usage: $0 <scan-path>}"
BIN="${SUPERDUPE_BIN:-./target/release/superdupe}"
SWEEP="${SWEEP_LOG:-/tmp/iothread-sweep.log}"
THREADS_LIST="${SWEEP_THREADS:-1 4 8 16 24 48 96}"

if [[ ! -x "$BIN" ]]; then
    echo "$BIN not found — run 'cargo build --release --bin superdupe' first." >&2
    exit 1
fi

: > "$SWEEP"
echo "Warmup pass…" >&2
"$BIN" scan "$TARGET" --min-size 0 --hash-algo blake3 --no-cache \
    --io-threads 8 -o /tmp/sweep-warmup.txt 2>/dev/null || true

for N in $THREADS_LIST; do
    echo "===== io-threads=$N =====" | tee -a "$SWEEP"
    OUT=$("$BIN" scan "$TARGET" --min-size 0 --hash-algo blake3 --no-cache \
        --io-threads "$N" -o "/tmp/sweep-$N.txt" 2>&1)
    echo "$OUT" | grep -E "^---|stage|Tier|total" | tee -a "$SWEEP"
    echo "" | tee -a "$SWEEP"
done

echo
echo "=== Summary ==="
echo "Compare 'total wallclock' across runs to find the saturation point."
echo "If wallclock stops improving past N threads, that's your sweet spot."
echo "Full log saved to: $SWEEP"
