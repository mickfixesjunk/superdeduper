# scripts/bench/Invoke-NoBufferPreRead.ps1
#
# FILE_FLAG_NO_BUFFERING pre-read pass over a corpus.
#
# WHAT
# Reads every file under -Path (or each individual path in
# -FilePaths) end-to-end with FILE_FLAG_NO_BUFFERING set, bypassing
# the Windows file system cache for the duration of THIS pass.
# Reports wall time + bytes read + effective MB/s + per-file errors.
#
# WHY
# RAMMap.exe -Ew -Es -Em -E0 evicts the FS cache (standby + modified
# + cached + ZeroPageList) but cannot reach lower layers: the USB
# host-controller buffer, the NTFS modified-write-cache pages that
# commit lazily back to disk, and the USB-HDD on-drive DRAM cache.
# A NO_BUFFERING read pass exercises the physical-disk path for
# every byte, gives a floor on cold-read cost, and lets the next
# timed trial settle against a known substrate.
#
# When the harness reports trial wall ~= NO_BUFFERING wall, the
# trial is cache-bound. When trial wall << NO_BUFFERING wall, the
# trial is cache-bound. When trial wall >> NO_BUFFERING wall, the
# trial is doing more work than cold-read (hash compute + grouping).
#
# WHEN TO CALL (canonical sequence for a true-cold HDD bench trial)
#   1. RAMMap.exe -Ew -Es -Em -E0   (FS-cache evict)
#   2. Start-Sleep -Milliseconds 500 (settle)
#   3. Invoke-NoBufferPreRead.ps1 -Path $Corpus
#   4. Run the actual bench trial.
#
# Run-SdHddBench.ps1 wires this in when -PreReadCache is passed.
#
# REQUIREMENTS
#   - Windows; PowerShell 5.1 or 7+ both supported.
#   - No external tools (uses .NET FileStream with FileOptions bit
#     0x20000000 = FILE_FLAG_NO_BUFFERING).
#   - Read access to every file under -Path. Inaccessible files are
#     counted in the per-file error list and skipped; the pass still
#     reports a clean wall + total bytes for the files that succeeded.
#
# IMPLEMENTATION NOTES
#   - Buffer size: 4 MB. >= 85 KB byte[] allocations land on the .NET
#     Large Object Heap which is page-aligned, satisfying the buffer
#     alignment FILE_FLAG_NO_BUFFERING requires (sector-aligned).
#   - Per-file read loop reads 4 MB chunks until the remaining size
#     is less than the volume sector size, then stops. The unread
#     tail (< sector size) is intentionally NOT served — the point
#     of this pass is to exercise the disk path, not produce
#     byte-perfect content. Trailing bytes don't change cache
#     occupancy meaningfully on the scale of a 10 GB corpus.
#   - Sector size: probed once per volume via Get-WmiObject. Falls
#     back to 4096 (NTFS default) if probe fails.

[CmdletBinding(DefaultParameterSetName = 'ByPath')]
param(
    # Root directory whose files will be pre-read. Walked recursively.
    [Parameter(Mandatory = $true, ParameterSetName = 'ByPath')]
    [string] $Path,

    # Explicit list of files to pre-read (overrides -Path). Useful
    # when the bench harness already has the file list cached.
    [Parameter(Mandatory = $true, ParameterSetName = 'ByList')]
    [string[]] $FilePaths,

    # Filter the recursive enumeration (only meaningful with -Path).
    # Defaults to '*' (every file).
    [Parameter(ParameterSetName = 'ByPath')]
    [string] $Filter = '*',

    # Emit one progress line per N files. 0 disables progress.
    [int] $ProgressEveryNFiles = 250
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$FILE_FLAG_NO_BUFFERING = 0x20000000
$BufferSize = 4 * 1024 * 1024  # 4 MB; LOH-aligned

function Get-VolumeSectorSize {
    param([string] $Drive)
    try {
        $vol = Get-CimInstance -ClassName Win32_DiskPartition |
            Where-Object { $_.DeviceID -and (Get-CimAssociatedInstance -InputObject $_ -ResultClass Win32_LogicalDisk |
                Where-Object { $_.DeviceID -eq $Drive }) } |
            Select-Object -First 1
        if ($vol -and $vol.BlockSize) { return [int]$vol.BlockSize }
    } catch { }
    return 4096
}

function Invoke-NoBufferRead {
    param(
        [string] $FilePath,
        [int]    $SectorSize
    )

    $fi = [System.IO.FileInfo]::new($FilePath)
    $totalSize = $fi.Length
    if ($totalSize -le 0) {
        return [PSCustomObject]@{
            Path      = $FilePath
            Bytes     = 0
            Error     = $null
        }
    }

    # Round down to a multiple of sector size. The trailing < sector
    # size bytes are not read; this is intentional (see header docs).
    $alignedSize = [math]::Floor($totalSize / $SectorSize) * $SectorSize
    if ($alignedSize -le 0) {
        return [PSCustomObject]@{
            Path  = $FilePath
            Bytes = 0
            Error = $null
        }
    }

    $opts = [System.IO.FileOptions]([int]$FILE_FLAG_NO_BUFFERING)
    $fs = $null
    try {
        $fs = [System.IO.FileStream]::new(
            $FilePath,
            [System.IO.FileMode]::Open,
            [System.IO.FileAccess]::Read,
            [System.IO.FileShare]::ReadWrite,
            $BufferSize,
            $opts
        )
        $buffer = [byte[]]::new($BufferSize)
        $bytesReadTotal = 0L
        while ($bytesReadTotal -lt $alignedSize) {
            $remaining = $alignedSize - $bytesReadTotal
            $request = [math]::Min([long]$BufferSize, [long]$remaining)
            $got = $fs.Read($buffer, 0, [int]$request)
            if ($got -le 0) { break }
            $bytesReadTotal += $got
        }
        return [PSCustomObject]@{
            Path  = $FilePath
            Bytes = $bytesReadTotal
            Error = $null
        }
    }
    catch {
        return [PSCustomObject]@{
            Path  = $FilePath
            Bytes = 0
            Error = $_.Exception.Message
        }
    }
    finally {
        if ($fs) { $fs.Dispose() }
    }
}

# Resolve file list.
$files = $null
if ($PSCmdlet.ParameterSetName -eq 'ByPath') {
    if (-not (Test-Path -LiteralPath $Path)) {
        throw "Path not found: $Path"
    }
    Write-Host "[no-buffer] enumerating '$Path' (filter='$Filter')..."
    $files = Get-ChildItem -LiteralPath $Path -Filter $Filter -Recurse -File -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty FullName
}
else {
    $files = $FilePaths
}

if (-not $files -or $files.Count -eq 0) {
    Write-Warning "[no-buffer] no files matched; nothing to read."
    return [PSCustomObject]@{
        Files     = 0
        Bytes     = 0
        WallMs    = 0
        MBps      = 0.0
        Errors    = @()
    }
}

# Probe sector size from the first file's volume root.
$firstFull = [System.IO.Path]::GetFullPath($files[0])
$rootDrive = [System.IO.Path]::GetPathRoot($firstFull).TrimEnd('\')
$sectorSize = Get-VolumeSectorSize -Drive $rootDrive
Write-Host "[no-buffer] sector size on $rootDrive = $sectorSize bytes; buffer = $BufferSize bytes"
Write-Host "[no-buffer] pre-reading $($files.Count) files with FILE_FLAG_NO_BUFFERING..."

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$totalBytes = 0L
$errors = [System.Collections.Generic.List[PSCustomObject]]::new()
$i = 0
foreach ($f in $files) {
    $i++
    $result = Invoke-NoBufferRead -FilePath $f -SectorSize $sectorSize
    if ($result.Error) {
        $errors.Add($result)
    }
    else {
        $totalBytes += $result.Bytes
    }
    if ($ProgressEveryNFiles -gt 0 -and ($i % $ProgressEveryNFiles -eq 0)) {
        $elapsed = $sw.Elapsed.TotalSeconds
        $mbps = if ($elapsed -gt 0) { ($totalBytes / 1MB) / $elapsed } else { 0 }
        Write-Host ("[no-buffer] {0}/{1} files; {2:N1} MiB read; {3:N1} MB/s" -f $i, $files.Count, ($totalBytes / 1MB), $mbps)
    }
}
$sw.Stop()

$wallMs = [int]$sw.Elapsed.TotalMilliseconds
$mbps = if ($sw.Elapsed.TotalSeconds -gt 0) { ($totalBytes / 1MB) / $sw.Elapsed.TotalSeconds } else { 0 }

Write-Host ("[no-buffer] DONE -- {0} files; {1:N1} MiB; {2} ms; {3:N1} MB/s; {4} errors" -f $files.Count, ($totalBytes / 1MB), $wallMs, $mbps, $errors.Count)

if ($errors.Count -gt 0 -and $errors.Count -le 10) {
    foreach ($e in $errors) {
        Write-Warning ("[no-buffer] error: {0} -- {1}" -f $e.Path, $e.Error)
    }
}
elseif ($errors.Count -gt 10) {
    Write-Warning ("[no-buffer] {0} files errored; first 3:" -f $errors.Count)
    foreach ($e in ($errors | Select-Object -First 3)) {
        Write-Warning ("  {0} -- {1}" -f $e.Path, $e.Error)
    }
}

return [PSCustomObject]@{
    Files  = $files.Count
    Bytes  = $totalBytes
    WallMs = $wallMs
    MBps   = [math]::Round($mbps, 2)
    Errors = $errors.ToArray()
}
