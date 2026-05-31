<#
.SYNOPSIS
    HDD-profile bench harness for sd's scan-only path.

.DESCRIPTION
    Sweeps `--io-threads` across configurable values, runs sd's `scan` subcommand
    against a target folder, captures wall-clock + stage breakdown + peak working
    set + (optional) PhysicalDisk perf counters per trial, writes results to a
    markdown table + CSV.

    Companion methodology: docs/perf/hdd-profile-bench-methodology.md.

    Cold-cache regime is the design intent. Pass -DropCacheBetweenTrials and
    have RAMMap.exe on $PATH (or in C:\Tools\) to enforce. Without it, the
    harness warns + falls back to warm-cache numbers.

    Precedent: scripts/iothread-sweep.ps1 is the simple (warm-cache, single-trial)
    version this harness extends. That script is fine for a quick poke; this one
    is for the full HDD-profile sweep that design routed for sdd-testwin.

.PARAMETER SdExe
    Path to the sd executable. Default: ./target/release/superdeduper.exe.

.PARAMETER MatrixSubsetFolder
    Single folder to run the io-threads matrix against. Use a smaller subset
    (e.g. SAMSUNG-T5-1TB) to bound matrix runtime. Required unless
    -FullCorpusValidate is set.

.PARAMETER FullCorpusValidate
    Switch to run a SINGLE-CONFIG validation against the full 4-folder corpus
    (DROPBOX + SAMSUNG-T5-1TB + SEAGATE-2TB + WD-BLACK-4TB) at the
    -IoThreads value. Ignores -IoThreadsSweep; pass -IoThreads for the value.

.PARAMETER FullCorpusRoots
    Paths to use for the full-corpus validation when -FullCorpusValidate is set.
    Default: E:\DROPBOX, E:\SAMSUNG-T5-1TB, E:\SEAGATE-2TB, E:\WD-BLACK-4TB.

.PARAMETER IoThreadsSweep
    Array of --io-threads values to sweep. Default: 1, 2, 4, 8, 16, 32, 96.
    96 = engine default on a 32-CPU box (threads x 3).

.PARAMETER IoThreads
    Single --io-threads value, used when -FullCorpusValidate is set.

.PARAMETER Trials
    Trials per config. Default: 3. Variance shown in the markdown table.

.PARAMETER OutDir
    Output directory. Will be created if it doesn't exist.

.PARAMETER DropCacheBetweenTrials
    Switch. If set, calls RAMMap.exe -Ew -Es between trials to flush the
    Windows OS page cache + standby list. Requires RAMMap.exe on $PATH or in
    C:\Tools\. Required for true cold-cache numbers.

.PARAMETER CaptureDiskCounters
    Switch. If set, samples PhysicalDisk perf counters at 1 Hz during each
    trial. Requires the script to run elevated.

.EXAMPLE
    # Matrix sweep on the SAMSUNG-T5-1TB subset, cold cache, 3 trials per config.
    ./scripts/bench/Run-SdHddBench.ps1 `
        -SdExe ./target/release/superdeduper.exe `
        -MatrixSubsetFolder 'E:\SAMSUNG-T5-1TB' `
        -OutDir ./bench-out/hdd-2026-05-31 `
        -DropCacheBetweenTrials `
        -Trials 3

.EXAMPLE
    # Full-corpus validation at the optimal io-threads value the matrix surfaced.
    ./scripts/bench/Run-SdHddBench.ps1 `
        -SdExe ./target/release/superdeduper.exe `
        -FullCorpusValidate `
        -IoThreads 16 `
        -OutDir ./bench-out/hdd-2026-05-31-validate `
        -DropCacheBetweenTrials `
        -Trials 3

.NOTES
    Author:  superdeduper-overflow, 2026-05-31
    Owner :  sdd-testwin (executor); overflow + benchmarker (analysis).
    Spec  :  docs/perf/hdd-profile-bench-methodology.md
#>
[CmdletBinding(DefaultParameterSetName = 'Matrix')]
param(
    [string]$SdExe = "$(Join-Path (Get-Location) 'target\release\superdeduper.exe')",

    [Parameter(Mandatory = $true, ParameterSetName = 'Matrix')]
    [string]$MatrixSubsetFolder,

    [Parameter(Mandatory = $true, ParameterSetName = 'Validate')]
    [switch]$FullCorpusValidate,

    [Parameter(ParameterSetName = 'Validate')]
    [string[]]$FullCorpusRoots = @(
        'E:\DROPBOX',
        'E:\SAMSUNG-T5-1TB',
        'E:\SEAGATE-2TB',
        'E:\WD-BLACK-4TB'
    ),

    [Parameter(ParameterSetName = 'Matrix')]
    [int[]]$IoThreadsSweep = @(1, 2, 4, 8, 16, 32, 96),

    [Parameter(ParameterSetName = 'Validate')]
    [int]$IoThreads = 16,

    [int]$Trials = 3,

    [Parameter(Mandatory = $true)]
    [string]$OutDir,

    [switch]$DropCacheBetweenTrials,

    [switch]$CaptureDiskCounters
)

$ErrorActionPreference = 'Stop'

# ---------------------------------------------------------------- preflight

if (-not (Test-Path $SdExe)) {
    throw "sd executable not found at $SdExe. Build with 'cargo build --release --features telemetry' or pass -SdExe <path>."
}

# Resolve RAMMap if cold-cache requested.
$RamMap = $null
if ($DropCacheBetweenTrials) {
    $RamMap = (Get-Command 'RAMMap.exe' -ErrorAction SilentlyContinue).Source
    if (-not $RamMap -and (Test-Path 'C:\Tools\RAMMap.exe')) {
        $RamMap = 'C:\Tools\RAMMap.exe'
    }
    if (-not $RamMap) {
        Write-Warning "DropCacheBetweenTrials requested but RAMMap.exe not found on PATH or in C:\Tools\."
        Write-Warning "Download: https://learn.microsoft.com/en-us/sysinternals/downloads/rammap"
        Write-Warning "Falling back to WARM-cache measurements. Results will not reflect cold-HDD behaviour."
    }
    else {
        Write-Host "RAMMap.exe located at $RamMap" -ForegroundColor DarkGray
    }
}

# Elevation check for perf counters.
if ($CaptureDiskCounters) {
    $current = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($current)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        Write-Warning "CaptureDiskCounters requested but PowerShell is not elevated."
        Write-Warning "Perf-counter capture may fail. Re-run from an Administrator shell."
    }
}

# Create out dir.
$OutDir = New-Item -ItemType Directory -Force -Path $OutDir | Select-Object -ExpandProperty FullName

# Mode selection.
if ($FullCorpusValidate) {
    $Mode = 'Validate'
    $TrialTargets = $FullCorpusRoots
    $IoSweep = @($IoThreads)
    Write-Host "Mode: FULL-CORPUS VALIDATE @ --io-threads $IoThreads" -ForegroundColor Cyan
    Write-Host "Roots:" -ForegroundColor DarkGray
    $FullCorpusRoots | ForEach-Object { Write-Host "  $_" -ForegroundColor DarkGray }
}
else {
    $Mode = 'Matrix'
    $TrialTargets = @($MatrixSubsetFolder)
    $IoSweep = $IoThreadsSweep
    Write-Host "Mode: MATRIX SWEEP" -ForegroundColor Cyan
    Write-Host "Subset folder: $MatrixSubsetFolder" -ForegroundColor DarkGray
    Write-Host "io-threads values: $($IoSweep -join ', ')" -ForegroundColor DarkGray
}
Write-Host "Trials per config: $Trials" -ForegroundColor DarkGray
Write-Host "Cold cache: $DropCacheBetweenTrials  Perf counters: $CaptureDiskCounters" -ForegroundColor DarkGray
Write-Host "Output: $OutDir" -ForegroundColor DarkGray

# Validate that targets exist.
foreach ($t in $TrialTargets) {
    if (-not (Test-Path $t)) {
        throw "Target path not found: $t"
    }
}

# ---------------------------------------------------------------- helpers

function Drop-Cache {
    if (-not $script:RamMap) { return }
    # -Ew = empty working sets; -Es = empty standby list. Both required for
    # true cold cache. Sysinternals returns 0 on success.
    & $script:RamMap -Ew 2>&1 | Out-Null
    & $script:RamMap -Es 2>&1 | Out-Null
    Start-Sleep -Milliseconds 250
}

function Parse-Timing {
    # Parse the human-readable stderr timing block sd emits at end of every scan.
    # Returns a hashtable with the extracted fields; null fields where parsing
    # didn't match.
    param([string]$Stderr)
    $out = @{
        Inventory_ms    = $null
        InventoryFiles  = $null
        Grouping_ms     = $null
        Layout_ms       = $null
        Hashing_ms      = $null
        BytesRead       = $null
        TotalWall_ms    = $null
        Tier0Files      = $null
        Tier1Files      = $null
        Tier2Files      = $null
        Tier3Files      = $null
    }
    # stage 1 inventory:    XXX ms (NNN files)
    if ($Stderr -match 'stage 1 inventory:\s+(\d+)\s+ms\s+\((\d+)\s+files\)') {
        $out.Inventory_ms = [int]$matches[1]
        $out.InventoryFiles = [int]$matches[2]
    }
    if ($Stderr -match 'stage 2 grouping:\s+(\d+)\s+ms') {
        $out.Grouping_ms = [int]$matches[1]
    }
    if ($Stderr -match 'stage 3 layout:\s+(\d+)\s+ms') {
        $out.Layout_ms = [int]$matches[1]
    }
    if ($Stderr -match 'stage 4 hashing:\s+(\d+)\s+ms.*bytes_read=([\d.]+\s*[KMGT]?i?B)') {
        $out.Hashing_ms = [int]$matches[1]
        $out.BytesRead = $matches[2]
    }
    if ($Stderr -match 'total wallclock:\s+(\d+)\s+ms') {
        $out.TotalWall_ms = [int]$matches[1]
    }
    # Tier breakdown: "  Tier 0 fmt :    NNN files ·    ..."
    foreach ($i in 0..3) {
        $pat = "Tier $i [a-z ]+:\s+(\d+) files"
        if ($Stderr -match $pat) {
            $out["Tier$($i)Files"] = [int]$matches[1]
        }
    }
    return $out
}

function Start-PerfSampler {
    # Returns a job handle that samples PhysicalDisk counters at 1 Hz to a CSV.
    # Caller stops the job + reads the CSV.
    param(
        [string]$CsvPath,
        [int]$MaxSeconds = 7200  # 2-hour safety cap
    )
    if (-not $script:CaptureDiskCounters) { return $null }
    $script = {
        param($Csv, $Max)
        $counters = @(
            '\PhysicalDisk(*)\Avg. Disk Queue Length',
            '\PhysicalDisk(*)\Disk Reads/sec',
            '\PhysicalDisk(*)\Avg. Disk sec/Read',
            '\PhysicalDisk(*)\Disk Read Bytes/sec'
        )
        $samples = New-Object System.Collections.Generic.List[PSCustomObject]
        $deadline = (Get-Date).AddSeconds($Max)
        while ((Get-Date) -lt $deadline) {
            try {
                $snap = Get-Counter -Counter $counters -SampleInterval 1 -MaxSamples 1
                foreach ($sample in $snap.CounterSamples) {
                    [void]$samples.Add([PSCustomObject]@{
                        Timestamp   = $sample.Timestamp.ToString('o')
                        Path        = $sample.Path
                        Value       = $sample.CookedValue
                    })
                }
            }
            catch {
                # transient -- keep sampling
            }
        }
        $samples | Export-Csv -Path $Csv -NoTypeInformation
    }
    Start-Job -ScriptBlock $script -ArgumentList $CsvPath, $MaxSeconds
}

function Stop-PerfSampler {
    param($Job)
    if (-not $Job) { return }
    Stop-Job -Job $Job -ErrorAction SilentlyContinue
    Remove-Job -Job $Job -Force -ErrorAction SilentlyContinue
}

# ---------------------------------------------------------------- main loop

$results = New-Object System.Collections.Generic.List[PSCustomObject]
$totalConfigs = $IoSweep.Count * $Trials
$current = 0

foreach ($iot in $IoSweep) {
    for ($t = 1; $t -le $Trials; $t++) {
        $current++
        $configLabel = "io$iot-t$t"
        Write-Host "`n[$current/$totalConfigs] $configLabel (io-threads=$iot, trial $t/$Trials)" -ForegroundColor Yellow

        Drop-Cache

        $stderrLog = Join-Path $OutDir "trial-$configLabel.stderr.log"
        $stdoutTmp = Join-Path $OutDir "trial-$configLabel.stdout.tmp"
        $perfCsv   = Join-Path $OutDir "trial-$configLabel.perf.csv"

        $perfJob = Start-PerfSampler -CsvPath $perfCsv

        # Build the sd invocation.
        # SUPERDEDUPER_CHANNEL=local keeps the run airgapped from prod state.
        $env:SUPERDEDUPER_CHANNEL = 'local'
        $sdArgs = @('scan') + $TrialTargets + @('--no-cache', '--io-threads', "$iot", '--format', 'json')

        # Measure wall + peak working set.
        $proc = Start-Process -FilePath $SdExe -ArgumentList $sdArgs `
                              -NoNewWindow -PassThru `
                              -RedirectStandardOutput $stdoutTmp `
                              -RedirectStandardError $stderrLog
        $peakWS_MB = 0
        while (-not $proc.HasExited) {
            try {
                $procInfo = Get-Process -Id $proc.Id -ErrorAction Stop
                $thisWS_MB = [math]::Round($procInfo.WorkingSet64 / 1MB, 1)
                if ($thisWS_MB -gt $peakWS_MB) { $peakWS_MB = $thisWS_MB }
            }
            catch {
                # process exited between checks; fall through
            }
            Start-Sleep -Milliseconds 200
        }
        $proc.WaitForExit()
        $exitCode = $proc.ExitCode
        $wallSec = $proc.ExitTime.Subtract($proc.StartTime).TotalSeconds

        Stop-PerfSampler -Job $perfJob

        # Parse the stage breakdown.
        $stderr = Get-Content $stderrLog -Raw -ErrorAction SilentlyContinue
        $parsed = Parse-Timing -Stderr $stderr

        # Validate: did the scan actually find anything?
        $stdoutLen = (Get-Item $stdoutTmp -ErrorAction SilentlyContinue).Length
        Remove-Item $stdoutTmp -ErrorAction SilentlyContinue

        $row = [PSCustomObject]@{
            Config         = $configLabel
            IoThreads      = $iot
            Trial          = $t
            ExitCode       = $exitCode
            WallSec        = [math]::Round($wallSec, 3)
            Inventory_ms   = $parsed.Inventory_ms
            InventoryFiles = $parsed.InventoryFiles
            Grouping_ms    = $parsed.Grouping_ms
            Layout_ms      = $parsed.Layout_ms
            Hashing_ms     = $parsed.Hashing_ms
            BytesRead      = $parsed.BytesRead
            TotalWall_ms   = $parsed.TotalWall_ms
            Tier3Files     = $parsed.Tier3Files
            PeakWS_MB      = $peakWS_MB
            StdoutBytes    = $stdoutLen
        }
        $results.Add($row)

        $hashStr = if ($null -eq $parsed.Hashing_ms) { '-' } else { "$($parsed.Hashing_ms)" }
        Write-Host ("  wall: {0,7:N2}s  hash: {1,7} ms  peakWS: {2,7:N1} MB  exit: {3}" -f
            $wallSec,
            $hashStr,
            $peakWS_MB,
            $exitCode) -ForegroundColor DarkGray
    }
}

# ---------------------------------------------------------------- report

# CSV.
$csvPath = Join-Path $OutDir 'results.csv'
$results | Export-Csv -Path $csvPath -NoTypeInformation
Write-Host "`nCSV: $csvPath" -ForegroundColor Green

# Markdown summary.
$mdPath = Join-Path $OutDir 'results.md'
$md = New-Object System.Text.StringBuilder
[void]$md.AppendLine('# HDD-profile bench results')
[void]$md.AppendLine()
[void]$md.AppendLine("- **Mode:** $Mode")
[void]$md.AppendLine("- **Trials/config:** $Trials")
$rammapStatus = if ($null -eq $RamMap) { 'not available' } else { $RamMap }
[void]$md.AppendLine("- **Cold cache (RAMMap):** $DropCacheBetweenTrials  (RAMMap: $rammapStatus)")
[void]$md.AppendLine("- **Perf counters captured:** $CaptureDiskCounters")
[void]$md.AppendLine("- **Targets:**")
foreach ($t in $TrialTargets) { [void]$md.AppendLine("  - `$t`") }
[void]$md.AppendLine()

# Per-config aggregate (mean wall + min/max).
[void]$md.AppendLine('## Aggregate by io-threads')
[void]$md.AppendLine()
[void]$md.AppendLine('| io-threads | trials | wall mean (s) | wall min (s) | wall max (s) | hash mean (ms) | peak WS mean (MB) |')
[void]$md.AppendLine('|-----------:|-------:|--------------:|-------------:|-------------:|---------------:|------------------:|')
foreach ($iot in $IoSweep) {
    $group = $results | Where-Object { $_.IoThreads -eq $iot }
    if ($group.Count -eq 0) { continue }
    $walls = $group | ForEach-Object { $_.WallSec }
    $hashs = $group | Where-Object { $null -ne $_.Hashing_ms } | ForEach-Object { $_.Hashing_ms }
    $ws    = $group | ForEach-Object { $_.PeakWS_MB }
    $wm = [math]::Round(($walls | Measure-Object -Average).Average, 3)
    $wn = [math]::Round(($walls | Measure-Object -Minimum).Minimum, 3)
    $wx = [math]::Round(($walls | Measure-Object -Maximum).Maximum, 3)
    $hm = if ($hashs.Count -gt 0) { [math]::Round(($hashs | Measure-Object -Average).Average, 0) } else { '-' }
    $wsm = [math]::Round(($ws | Measure-Object -Average).Average, 1)
    [void]$md.AppendLine("| $iot | $($group.Count) | $wm | $wn | $wx | $hm | $wsm |")
}
[void]$md.AppendLine()

# Per-trial detail.
[void]$md.AppendLine('## All trials')
[void]$md.AppendLine()
[void]$md.AppendLine('| config | exit | wall (s) | inventory (ms) | inv files | hashing (ms) | bytes read | tier-3 files | peak WS (MB) |')
[void]$md.AppendLine('|--------|----:|---------:|---------------:|----------:|-------------:|-----------:|-------------:|-------------:|')
foreach ($r in $results) {
    $bytes = if ($null -eq $r.BytesRead)      { '-' } else { $r.BytesRead }
    $inv   = if ($null -eq $r.Inventory_ms)   { '-' } else { $r.Inventory_ms }
    $invf  = if ($null -eq $r.InventoryFiles) { '-' } else { $r.InventoryFiles }
    $hash  = if ($null -eq $r.Hashing_ms)     { '-' } else { $r.Hashing_ms }
    $t3    = if ($null -eq $r.Tier3Files)     { '-' } else { $r.Tier3Files }
    [void]$md.AppendLine("| $($r.Config) | $($r.ExitCode) | $($r.WallSec) | $inv | $invf | $hash | $bytes | $t3 | $($r.PeakWS_MB) |")
}
[void]$md.AppendLine()
[void]$md.AppendLine('## Notes for analysis')
[void]$md.AppendLine()
[void]$md.AppendLine('- The wall (s) column is the OS-level process wall (Start-Process ExitTime - StartTime). The "total wallclock" inside sd''s stderr is engine-internal and usually within tens of ms of the OS-level wall; if they diverge substantially, investigate (background work, process-spawn overhead, etc).')
[void]$md.AppendLine('- The hashing (ms) column is sd''s stage-4 wallclock. On HDD, this is dominated by IO; if it doesn''t scale down as io-threads increases, the HDD head is the bottleneck (good signal: oversubscription doesn''t help here).')
[void]$md.AppendLine('- The peak WS (MB) column is the maximum WorkingSet64 the harness saw during sampling. If it climbs linearly with io-threads, each rayon worker carries its own buffers; bounded but worth noting.')
[void]$md.AppendLine('- Per-trial perf-counter CSVs (when -CaptureDiskCounters was set) live alongside this file as `trial-<config>.perf.csv`.')
[void]$md.AppendLine('- Per-trial raw sd stderr lives as `trial-<config>.stderr.log` for spot-checking the parsing.')
[void]$md.AppendLine()
[void]$md.AppendLine('Companion methodology: `docs/perf/hdd-profile-bench-methodology.md`.')

Set-Content -Path $mdPath -Value $md.ToString() -Encoding UTF8
Write-Host "Markdown: $mdPath" -ForegroundColor Green
Write-Host "`nDone." -ForegroundColor Green
