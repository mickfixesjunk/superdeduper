# scripts/iothread-sweep.ps1 — Windows version of the io-threads
# sweep. Run from the folder where superdupe.exe lives.
#
# Usage:
#   .\scripts\iothread-sweep.ps1 -Target "C:\Users\Audio\AppData"
#
# Output: one timing block per thread count, dropped into
# `iothread-sweep` on the user's Desktop. The decoder unwinds the
# PowerShell-flavoured UTF-16-with-BOM stderr capture into something
# pasteable.

param(
    [Parameter(Mandatory = $true)] [string]$Target,
    [string]$Bin = ".\superdupe.exe",
    [int[]]$Threads = @(1, 8, 16, 24, 48, 96),
    [string]$ResultsDir = "$env:USERPROFILE\Desktop\iothread-sweep"
)

$ErrorActionPreference = "Stop"
if (-not (Test-Path $Bin)) {
    throw "$Bin not found. Copy the latest superdupe.exe here, or pass -Bin <path>."
}
New-Item -ItemType Directory -Force -Path $ResultsDir | Out-Null

# Warmup pass so all measured runs see the OS file cache in the
# same state. Without this the first run shows artificially-slow
# numbers and skews the curve.
Write-Host "Warmup pass…" -ForegroundColor DarkGray
& $Bin scan $Target --min-size 0 --hash-algo blake3 --no-cache `
    --io-threads 8 -o "$env:TEMP\warmup.txt" *> $null

foreach ($n in $Threads) {
    Write-Host "=== io-threads=$n ===" -ForegroundColor Cyan
    $timingFile = "$ResultsDir\timing-$n.txt"
    & $Bin scan $Target --min-size 0 --hash-algo blake3 --no-cache `
        --io-threads $n `
        -o "$ResultsDir\out-$n.txt" `
        *> $timingFile
    # PowerShell *> writes UTF-16 with a BOM. Read it as Unicode,
    # then pull just the timing lines.
    [System.IO.File]::ReadAllText($timingFile, [System.Text.Encoding]::Unicode) `
        | Select-String -Pattern '^(---|stage|  Tier|total)' `
        | ForEach-Object { $_.Line }
    Write-Host ""
}

Write-Host "Done. Per-run timing files in $ResultsDir\" -ForegroundColor Green
Write-Host "Paste the console output above into the conversation for analysis." -ForegroundColor Yellow
