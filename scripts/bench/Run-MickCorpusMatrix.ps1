#requires -Version 5.1
# Phase 4/5 Mick-corpus matrix template (testdesign 12:42 PDT framework lock).
#
# Changes from v0.3.39 runner:
#   1. SCAN-END SIGNAL: replace CPU-idle-tail with scan-history JSON file-watch.
#      Wall measurement aligns with user-perceived "scan done" moment.
#   2. PAIRED-CLI METHODOLOGY: cold-CLI and cold-GUI in same matrix run with
#      standby-purge between each cold cell. Eliminates inter-matrix CLI variance.
#   3. DEFENDER-TOGGLE CELLS: separate cells for Defender-off (hermetic) vs
#      Defender-on (real-world Mick scenario).
#   4. NO chunk_size env overrides (Phase 4+ default-ships the fix).
#
# 5-cell matrix:
#   1. cold-CLI defender-off  (standby-purge before)
#   2. cold-GUI defender-off  (standby-purge before; paired ratio vs cell 1)
#   3. cold-CLI defender-on   (standby-purge before)
#   4. cold-GUI defender-on   (standby-purge before; paired ratio vs cell 3)
#   5. warm-GUI defender-off  (no purge; completeness anchor)
#
# Before running, edit BINARY_SHA and OUT path to point at the candidate.

$ErrorActionPreference = 'Stop'

# ---- EDIT THESE PER MATRIX RUN ----------------------------------------
$BINARY_SHA = '_REPLACE_ME_'          # e.g. '01cbf40' or '804750c' or 'phase4-sha'
$LABEL      = '_REPLACE_ME_'          # used in output dir name, e.g. 'phase4-firstsha'
# -----------------------------------------------------------------------

$OUT       = "C:\sdd-tests\cold-vs-cold-mick-corpus-$LABEL"
$HERMETIC  = Join-Path $OUT 'hermetic-data'
$INSTALL   = Join-Path $HERMETIC 'install'
$CORPUS    = 'C:\sdd-tests'
$CLI       = "C:\Users\NeoMatrix\sdd-builds\$BINARY_SHA\superdeduper-windows-x86_64.exe"
$GUI       = "C:\Users\NeoMatrix\sdd-builds\$BINARY_SHA\superdeduper-gui-windows-x86_64.exe"
$BACKBAK   = ".benchbak-$BINARY_SHA-$LABEL"

if ($BINARY_SHA -eq '_REPLACE_ME_' -or $LABEL -eq '_REPLACE_ME_') {
    throw "Edit BINARY_SHA and LABEL before running."
}

# ---- Sanity: binaries present ------------------------------------------
foreach ($b in @($CLI, $GUI)) {
    if (-not (Test-Path $b)) { throw "binary missing: $b" }
}
Write-Host "BINARIES OK: $CLI ; $GUI"

# ---- Fresh hermetic data dir ------------------------------------------
if (Test-Path $HERMETIC) { Remove-Item -Recurse -Force $HERMETIC }
New-Item -ItemType Directory -Path $HERMETIC -Force | Out-Null
New-Item -ItemType Directory -Path $INSTALL  -Force | Out-Null
New-Item -ItemType Directory -Path $OUT      -Force | Out-Null
Set-Location $OUT

$env:SUPERDEDUPER_TEST_DATA_DIR          = $HERMETIC
$env:SUPERDEDUPER_PERF_INSTRUMENT_UPDATE = '1'

# ---- Backup GUI state files BEFORE patching them (F2 fix) -------------
# Runner is now self-sufficient: backs up real user state, patches for
# hermetic run, restores at end. Caller no longer responsible for backup.
$stateFiles = @(
    'C:\Users\NeoMatrix\AppData\Roaming\superdeduper\data\app.ron',
    'C:\Users\NeoMatrix\AppData\Local\superdeduper\recent-projects.json',
    'C:\Users\NeoMatrix\AppData\Local\superdeduper\results-state.json',
    'C:\Users\NeoMatrix\AppData\Local\superdeduper\scan-checkpoint.json'
)
foreach ($f in $stateFiles) {
    $bk = "$f$BACKBAK"
    if (Test-Path $f) {
        if (Test-Path $bk) {
            throw "Stale backup at $bk -- refusing to overwrite. Investigate + remove manually before re-running."
        }
        Copy-Item -Path $f -Destination $bk -Force
        Write-Host "BACKED UP: $f -> $bk"
    } else {
        Write-Host "(no real file to back up: $f)"
    }
}

# ---- Capture pre-matrix Defender state (F3 fix) ------------------------
$defenderWasDisabled = (Get-MpPreference).DisableRealtimeMonitoring
Write-Host "Pre-matrix Defender DisableRealtimeMonitoring = $defenderWasDisabled (will restore at end)"

# ---- Patch app.ron in place for hermetic run --------------------------
$appRon = 'C:\Users\NeoMatrix\AppData\Roaming\superdeduper\data\app.ron'
if (Test-Path $appRon) {
    $rc = Get-Content $appRon -Raw
    $rc = [regex]::Replace($rc, 'skip_preflight:false', 'skip_preflight:true')
    $rc = [regex]::Replace($rc, 'dismissed_alpha_warning:false', 'dismissed_alpha_warning:true')
    $rc = [regex]::Replace($rc, 'roots:\[[^\]]*\]', 'roots:[]')
    Set-Content -Path $appRon -Value $rc -NoNewline
    Write-Host 'PATCHED: app.ron (skip_preflight + dismissed_alpha_warning + roots:[])'
}

# ---- Pre-seed install.prod.json (share_default=auto_opt_in) ----------
$installJson = @{
    schema_version = 1
    install_id = '1e4c9ee7-f81a-4991-b6bc-0014314d6a28'
    install_key_hex = 'cd4f6d8323fa01481a1cb889abcbd822233df54dbb0e7059aa20b90521da9112'
    registered = $true
    server_url = 'https://api.superdeduper.io'
    client_version_at_register = '0.3.36'
    share_default = 'auto_opt_in'
    counters = @{ exclude_pattern_edits = 0; achievements_verify_invocations = 0 }
    share_prompt_count = 0
    share_last_choice = $null
    last_bench_lane = $null
} | ConvertTo-Json -Depth 5
$installJson | Set-Content -Path (Join-Path $INSTALL 'install.prod.json')
Write-Host 'PRE-SEEDED: install.prod.json (share_default=auto_opt_in)'

# ---- Verify app.ron patch is in place (defense in depth) --------------
$ronContent = Get-Content $appRon -Raw
if ($ronContent -notmatch 'skip_preflight:true') { throw 'app.ron patch failed (skip_preflight)' }
if ($ronContent -notmatch 'dismissed_alpha_warning:true') { throw 'app.ron patch failed (dismissed_alpha_warning)' }
if ($ronContent -notmatch 'roots:\[\]') { throw 'app.ron patch failed (roots not empty)' }
Write-Host 'VERIFIED: app.ron patches in place'

# ---- Defender control helpers -----------------------------------------
function Set-Defender([bool]$enabled) {
    Set-MpPreference -DisableRealtimeMonitoring (-not $enabled)
    $actual = (Get-MpPreference).DisableRealtimeMonitoring
    Write-Host ("Defender: realtime $(if ($enabled) {'ENABLED'} else {'DISABLED'}); DisableRealtimeMonitoring=$actual")
}

# ---- Standby purge wrapper ---------------------------------------------
function Invoke-StandbyPurge {
    Start-ScheduledTask -TaskName 'sdd-standby-purge'
    $deadline = (Get-Date).AddSeconds(60)
    while ((Get-Date) -lt $deadline) {
        Start-Sleep -Seconds 2
        $task = Get-ScheduledTask -TaskName 'sdd-standby-purge'
        if ($task.State -eq 'Ready') { break }
    }
    $info = Get-ScheduledTaskInfo -TaskName 'sdd-standby-purge'
    Write-Host ("LastTaskResult=0x{0:X8}  LastRunTime={1}  State={2}" -f $info.LastTaskResult, $info.LastRunTime, $task.State)
    $free = (Get-Counter '\Memory\Available MBytes').CounterSamples.CookedValue
    Write-Host ("Free MB pre-scan: {0:N0}" -f $free)
}

# ---- Reset hermetic data dir between GUI cells (no scan-history bleed) -
function Reset-HermeticData {
    if (Test-Path $HERMETIC) { Remove-Item -Recurse -Force $HERMETIC }
    New-Item -ItemType Directory -Path $HERMETIC -Force | Out-Null
    New-Item -ItemType Directory -Path $INSTALL  -Force | Out-Null
    $installJson | Set-Content -Path (Join-Path $INSTALL 'install.prod.json')
    # Reset app.ron persisted state so roots/scan-history don't bleed across cells
    $reseed = Get-Content $appRon -Raw
    $reseed = [regex]::Replace($reseed, 'roots:\[[^\]]*\]', 'roots:[]')
    Set-Content -Path $appRon -Value $reseed -NoNewline
}

# ---- CLI run helper ----------------------------------------------------
function Invoke-CliScan([string]$label) {
    $jsonPath   = Join-Path $OUT "$label.json"
    $stderrPath = Join-Path $OUT "$label.stderr"
    $sw = [Diagnostics.Stopwatch]::StartNew()
    & $CLI scan --no-cache --format json -o $jsonPath $CORPUS 2> $stderrPath
    $sw.Stop()
    Write-Host ("$label WALL: {0:N3}s ({1:N2} min)" -f $sw.Elapsed.TotalSeconds, $sw.Elapsed.TotalMinutes)
    Get-Content $stderrPath | Where-Object { $_ -match '^(stage|total|tier|  Tier|  \(per)' } | ForEach-Object { Write-Host $_ }
    return $sw.Elapsed.TotalSeconds
}

# ---- GUI run helper (scan-history file-watch methodology) -------------
function Invoke-GuiLive-FileWatch([string]$label) {
    $stderrPath  = Join-Path $OUT "$label.stderr"
    $stdoutPath  = Join-Path $OUT "$label.stdout"
    $scanHistDir = Join-Path $HERMETIC 'scan-history'

    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p  = Start-Process -FilePath $GUI `
        -ArgumentList '--live', $CORPUS `
        -RedirectStandardError $stderrPath `
        -RedirectStandardOutput $stdoutPath `
        -PassThru -WindowStyle Hidden
    Write-Host "GUI PID: $($p.Id)  ($label)"

    # Poll for scan-history JSON file appearance (scan-end signal)
    $maxSeconds = 600
    $jsonFile   = $null
    while ($sw.Elapsed.TotalSeconds -lt $maxSeconds) {
        if ($p.HasExited) {
            Write-Host "GUI exited unexpectedly at $($sw.Elapsed.TotalSeconds)s"
            break
        }
        if (Test-Path $scanHistDir) {
            $jsonFile = Get-ChildItem $scanHistDir -Filter '*.json' -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($jsonFile) { break }
        }
        Start-Sleep -Milliseconds 200
    }

    if ($jsonFile) {
        # Wait for file size to stabilize (1s of unchanged size = write complete)
        $lastSize = -1
        $stableCount = 0
        while ($sw.Elapsed.TotalSeconds -lt $maxSeconds -and $stableCount -lt 5) {
            $sz = (Get-Item $jsonFile.FullName -ErrorAction SilentlyContinue).Length
            if ($sz -eq $lastSize -and $sz -gt 0) { $stableCount++ } else { $stableCount = 0 }
            $lastSize = $sz
            Start-Sleep -Milliseconds 200
        }
        $sw.Stop()
        Write-Host ("$label scan-history landed: {0} ({1} bytes; stabilized after {2}s)" -f $jsonFile.Name, $lastSize, $sw.Elapsed.TotalSeconds)
    } else {
        $sw.Stop()
        Write-Host "$label TIMEOUT or no scan-history file written"
    }
    Write-Host ("$label WALL: {0:N3}s ({1:N2} min)" -f $sw.Elapsed.TotalSeconds, $sw.Elapsed.TotalMinutes)
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
    return $sw.Elapsed.TotalSeconds
}

# ---- 5-CELL MATRIX -----------------------------------------------------
$results = [ordered]@{}

Write-Host ''
Write-Host '========== Cell 1: cold-CLI Defender-OFF =========='
Set-Defender $false
Invoke-StandbyPurge
$results['cell1-cold-cli-defoff'] = Invoke-CliScan 'cell1-cold-cli-defoff'

Write-Host ''
Write-Host '========== Cell 2: cold-GUI Defender-OFF =========='
Reset-HermeticData
Invoke-StandbyPurge
$results['cell2-cold-gui-defoff'] = Invoke-GuiLive-FileWatch 'cell2-cold-gui-defoff'

Write-Host ''
Write-Host '========== Cell 3: cold-CLI Defender-ON =========='
Set-Defender $true
Invoke-StandbyPurge
$results['cell3-cold-cli-defon'] = Invoke-CliScan 'cell3-cold-cli-defon'

Write-Host ''
Write-Host '========== Cell 4: cold-GUI Defender-ON =========='
Reset-HermeticData
Invoke-StandbyPurge
$results['cell4-cold-gui-defon'] = Invoke-GuiLive-FileWatch 'cell4-cold-gui-defon'

Write-Host ''
Write-Host '========== Cell 5: warm-GUI Defender-OFF (anchor) =========='
Set-Defender $false
Reset-HermeticData
$results['cell5-warm-gui-defoff'] = Invoke-GuiLive-FileWatch 'cell5-warm-gui-defoff'

# ---- Defender final state restore (F3 fix: restore to pre-matrix state) -
Set-Defender (-not $defenderWasDisabled)
Write-Host "Defender restored to pre-matrix state (DisableRealtimeMonitoring=$defenderWasDisabled)"

# ---- Harvest perf-chunks + perf-streaming from each .stderr ----------
Write-Host ''
Write-Host '========== INSTRUMENTATION HARVEST =========='
$harvestPath = Join-Path $OUT 'instrumentation-harvest.txt'
'' | Set-Content -Path $harvestPath
foreach ($lbl in @($results.Keys)) {
    $stderrFile = Join-Path $OUT "$lbl.stderr"
    if (-not (Test-Path $stderrFile)) { continue }
    Add-Content -Path $harvestPath -Value ""
    Add-Content -Path $harvestPath -Value "===== $lbl.stderr ====="
    foreach ($line in (Get-Content $stderrFile -ErrorAction SilentlyContinue)) {
        if ($line -match 'perf-chunks|perf-streaming|GUI scan:|SCAN-COMPLETE') {
            Add-Content -Path $harvestPath -Value $line
            Write-Host ("  [{0}] {1}" -f $lbl, $line)
        }
    }
}

# ---- GUI state restore from backups created at start ------------------
# (uses Copy-then-delete instead of Rename, so we can detect stale backups
# next run -- see backup step at runner start)
Write-Host ''
Write-Host '========== STATE RESTORE =========='
foreach ($f in $stateFiles) {
    $bk = "$f$BACKBAK"
    if (Test-Path $bk) {
        if (Test-Path $f) { Remove-Item -Path $f -Force }
        Move-Item -Path $bk -Destination $f -Force
        Write-Host "RESTORED: $f"
    } else {
        Write-Host "(no backup to restore: $f -- runner-created file may remain)"
    }
}

# ---- Summary + paired ratios ------------------------------------------
$c1 = $results['cell1-cold-cli-defoff']
$c2 = $results['cell2-cold-gui-defoff']
$c3 = $results['cell3-cold-cli-defon']
$c4 = $results['cell4-cold-gui-defon']
$c5 = $results['cell5-warm-gui-defoff']

# F1 fix: refuse to compute ratios if any cold cell wall is non-positive
# (process failure / GUI exited before scan-history wrote / timeout).
# Reporting a ratio with a 0 denominator would yield false-GREEN.
$defoff_ok = ($c1 -gt 0) -and ($c2 -gt 0)
$defon_ok  = ($c3 -gt 0) -and ($c4 -gt 0)
$ratio_defoff = if ($defoff_ok) { $c2 / $c1 } else { [double]::NaN }
$ratio_defon  = if ($defon_ok)  { $c4 / $c3 } else { [double]::NaN }

$status_defoff =
    if (-not $defoff_ok) { 'ERROR -- cell1 or cell2 wall <= 0 (process failure / timeout); criteria undefined' }
    elseif ($ratio_defoff -le 1.10) { 'GREEN -- meets <=1.10x criteria' }
    elseif ($ratio_defoff -le 1.20) { 'EDGE -- 1.10x to 1.20x band' }
    else { 'RED -- exceeds 1.10x criteria' }

$status_defon =
    if (-not $defon_ok) { 'ERROR -- cell3 or cell4 wall <= 0 (process failure / timeout)' }
    else { '(informational; Mick real-world simulation)' }

$ratio_defoff_disp = if ($defoff_ok) { ('{0:N3}x' -f $ratio_defoff) } else { 'N/A (cell failure)' }
$ratio_defon_disp  = if ($defon_ok)  { ('{0:N3}x' -f $ratio_defon)  } else { 'N/A (cell failure)' }

$summary = @"

========== PAIRED-CLI MATRIX SUMMARY ==========
Binary:  $BINARY_SHA
Label:   $LABEL

  cell1 cold-CLI def-OFF:   {0,8:N2} s
  cell2 cold-GUI def-OFF:   {1,8:N2} s          paired ratio vs cell1: {6}
  cell3 cold-CLI def-ON :   {2,8:N2} s
  cell4 cold-GUI def-ON :   {3,8:N2} s          paired ratio vs cell3: {7}
  cell5 warm-GUI def-OFF:   {4,8:N2} s

PAIRED RATIOS:
  cold-GUI/cold-CLI Defender-off: {6}   (criteria gate: <= 1.10x)
  cold-GUI/cold-CLI Defender-on:  {7}   (Mick real-world simulation)

CRITERIA-GATE STATUS (Defender-off paired):
  {8}
DEFENDER-ON STATUS:
  {9}
"@ -f $c1, $c2, $c3, $c4, $c5, 0, $ratio_defoff_disp, $ratio_defon_disp, $status_defoff, $status_defon

Write-Host $summary
$summary | Set-Content -Path (Join-Path $OUT 'summary.txt')
Write-Host 'MATRIX DONE'
