#!/usr/bin/env bash
# iothread-scaling.sh — sweep --io-threads to characterize parallelism
# scaling on stage 4 hash IO. Helps localize whether the sd/cz wall is
# in the io-thread pool design or elsewhere.
#
# Usage: ./scripts/bench/iothread-scaling.sh <corpus_path>

set -euo pipefail

CORPUS="${1:?usage: iothread-scaling.sh <corpus_path>}"
SD="$(cd "$(dirname "$0")/../.." && pwd)/target/release/superdeduper"
RUNS=3

echo "## io-threads scaling sweep — $(basename "$CORPUS")"
echo "Run at $(date -u +%Y-%m-%dT%H:%M:%SZ) — corpus=$CORPUS (warm-cache regime)"
echo
echo "| io-threads | Trial | Wall ms | Stage4 ms | Tier1 CPU ms | Tier3 CPU ms |"
echo "|-----------:|------:|--------:|----------:|-------------:|-------------:|"

for n in 1 2 4 8 16 32 64; do
    for t in $(seq 1 $RUNS); do
        out=$("$SD" scan "$CORPUS" --min-size 1 --io-threads "$n" --quiet 2>&1)
        wall=$(printf '%s\n' "$out" | sed -nE 's/^total wallclock:\s+([0-9]+) ms.*/\1/p' | head -1)
        s4=$(printf '%s\n' "$out" | sed -nE 's/^stage 4 hashing:\s+([0-9]+) ms.*/\1/p' | head -1)
        # Tier CPU-summed pulled from "Tier N XXX: ... · NNNN ms CPU-summed"
        tier1=$(printf '%s\n' "$out" | sed -nE 's/^\s*Tier 1 head:.*·\s+([0-9]+) ms CPU-summed.*/\1/p' | head -1)
        tier3=$(printf '%s\n' "$out" | sed -nE 's/^\s*Tier 3 full:.*·\s+([0-9]+) ms CPU-summed.*/\1/p' | head -1)
        printf '| %d | %d | %s | %s | %s | %s |\n' "$n" "$t" "${wall:-?}" "${s4:-?}" "${tier1:-?}" "${tier3:-?}"
    done
done
echo
