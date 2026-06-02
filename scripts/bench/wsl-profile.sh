#!/usr/bin/env bash
# wsl-profile.sh — WSL/Linux profiler harness for v0.3.29 work-stealing
# investigation. Reports sd-vs-cz wall + per-stage (sd) + peak RSS for
# 3 runs per (corpus, tool); writes one Markdown row block per run.
#
# Cache regime: WARM. Linux page cache holds most of the corpus on the
# 61 GB host; we have no sudo, so drop_caches isn't available. Numbers
# are warm-cache; relative sd/cz delta still informative.
#
# Usage: ./scripts/bench/wsl-profile.sh <corpus_path> [tag]

set -euo pipefail

CORPUS="${1:?usage: wsl-profile.sh <corpus_path> [tag]}"
TAG="${2:-$(basename "$CORPUS")}"
SD="$(cd "$(dirname "$0")/../.." && pwd)/target/release/superdeduper"
CZ="${CZKAWKA_BIN:-czkawka_cli}"
RUNS=3

if [[ ! -x "$SD" ]]; then
    echo "missing release binary: $SD (cargo build --release first)" >&2
    exit 2
fi

if ! command -v "$CZ" >/dev/null 2>&1; then
    echo "missing czkawka_cli on PATH" >&2
    exit 2
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }

# stage timings are emitted on stdout by sd's --print-timing path
# (always-on, no flag needed); peak RSS comes from /usr/bin/time -v
run_sd() {
    local out
    # SUPERDEDUPER_LOG=info captures the "stage 1: walk complete files=N"
    # tracing line so we can report total-files-walked alongside the
    # size-twin count from the human-readable timing block.
    # Strip ANSI escapes so the tracing colour codes don't break the
    # `files=N` regex. NOTE: `--quiet` would suppress the tracing
    # output we need, so we drop it; the human-readable timing block
    # is on stdout regardless of log level.
    out=$(SUPERDEDUPER_LOG=info /usr/bin/time -v "$SD" scan "$CORPUS" --min-size 1 2>&1 | sed -E 's/\x1b\[[0-9;]*m//g' || true)
    local wall stage1 stage4 rss twin total bytes
    wall=$(printf '%s\n' "$out" | sed -nE 's/^total wallclock:\s+([0-9]+) ms.*/\1/p' | head -1)
    stage1=$(printf '%s\n' "$out" | sed -nE 's/^stage 1 inventory:\s+([0-9]+) ms.*/\1/p' | head -1)
    stage4=$(printf '%s\n' "$out" | sed -nE 's/^stage 4 hashing:\s+([0-9]+) ms.*/\1/p' | head -1)
    rss=$(printf '%s\n' "$out" | sed -nE 's/.*Maximum resident set size \(kbytes\): ([0-9]+).*/\1/p' | head -1)
    twin=$(printf '%s\n' "$out" | sed -nE 's/^stage 1 inventory:.*\(([0-9]+) files\).*/\1/p' | head -1)
    total=$(printf '%s\n' "$out" | sed -nE 's/.*stage 1: walk complete.*files=([0-9]+).*/\1/p' | head -1)
    bytes=$(printf '%s\n' "$out" | sed -nE 's/^stage 4 hashing:.*bytes_read=([0-9.]+ [KMGT]?i?B).*/\1/p' | head -1)
    printf 'sd|%s|%s|%s|%s|%s|%s|%s\n' "${wall:-?}" "${stage1:-?}" "${stage4:-?}" "${rss:-?}" "${total:-?}" "${twin:-?}" "${bytes:-?}"
}

run_cz() {
    local out start end wall_ms rss
    # ns precision via %N; /1e6 → ms.
    start=$(date +%s%N)
    # cz default search-method is HASH (full-file hash dedup) — matches
    # sd's default exact-mode contract.
    out=$(/usr/bin/time -v "$CZ" dup -d "$CORPUS" 2>&1 || true)
    end=$(date +%s%N)
    wall_ms=$(( (end - start) / 1000000 ))
    rss=$(printf '%s\n' "$out" | sed -nE 's/.*Maximum resident set size \(kbytes\): ([0-9]+).*/\1/p' | head -1)
    printf 'cz|%s|-|-|%s|-|-|-\n' "${wall_ms:-?}" "${rss:-?}"
}

echo "## $TAG"
echo "Run at $(ts) — corpus=$CORPUS (warm-cache regime)"
echo
echo "| Tool | Trial | Wall ms | Stage1 (walk) ms | Stage4 (hash) ms | Peak RSS KiB | Total files | Size-twin files | Bytes |"
echo "|------|-------|---------|-----------------|------------------|--------------|-------------|-----------------|-------|"

for i in $(seq 1 $RUNS); do
    row=$(run_sd)
    IFS='|' read -r tool wall s1 s4 rss total twin bytes <<< "$row"
    printf '| %s | %d | %s | %s | %s | %s | %s | %s | %s |\n' "$tool" "$i" "$wall" "$s1" "$s4" "$rss" "$total" "$twin" "$bytes"
done
for i in $(seq 1 $RUNS); do
    row=$(run_cz)
    IFS='|' read -r tool wall s1 s4 rss total twin bytes <<< "$row"
    printf '| %s | %d | %s | %s | %s | %s | %s | %s | %s |\n' "$tool" "$i" "$wall" "$s1" "$s4" "$rss" "$total" "$twin" "$bytes"
done

echo
