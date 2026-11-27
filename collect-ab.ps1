# Collects two frame logs under an A/B protocol, then compares them.
#
# The rule of this harness: the only difference between the two runs is the
# variables in VariantA and VariantB. Both runs use the same game executable,
# the same cache snapshot and the same data snapshot, copied into separate
# directories so one run cannot warm the other's cache. The game must be
# started from the same place in the same scene; a script cannot know where
# that is, so the operator does it before each run.
#
# Each run gets its own folder under -OutDir (outside the source tree): the
# whole log (stderr.txt), the rest of the output (stdout.txt), the frame,
# first-bind and frame-presented lines (frames.txt), what was set
# (settings.txt) and `victory report` of the log (report.txt). The comparison
# is written to compare.txt. The script ends with the exit code of `victory ab`.

param(
    [Parameter(Mandatory = $true)][string]$GameExe,
    [string]$Exe = "$PSScriptRoot\target\release\victory.exe",
    # A new folder per call, outside the source tree.
    [string]$OutDir = (Join-Path $env:LOCALAPPDATA ('victory\ab-runs\' + (Get-Date -Format 'yyyy-MM-dd_HH-mm-ss'))),
    # Seconds of gameplay to capture per run. Kelvaroq's protocol uses 300.
    [int]$RunSeconds = 300,
    [int]$MinDraws = 3000,
    # Optional snapshots copied into each run's own cache/data directory.
    [string]$CacheSnapshot,
    [string]$DataSnapshot,
    # Optional directory for raw missing-pipeline blobs (one subdir per run).
    [string]$RawSave,
    # Variables that differ between the two runs, e.g. @{ VICTORY_REUSE = '2' }.
    [hashtable]$VariantA = @{},
    [hashtable]$VariantB = @{}
)

$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot

# A path as PowerShell resolves it (relative to the current location, not to
# the process directory that .NET would use), whether it exists or not.
function Get-FullPath([string]$path) {
    return $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($path)
}

# True when $full lies in this source tree or in any folder holding the
# tree's .victory-root marker (the layer refuses such folders too).
function Test-InsideRepo([string]$full) {
    $root = [System.IO.Path]::GetFullPath($repo).TrimEnd('\') + '\'
    if (($full.TrimEnd('\') + '\').StartsWith($root, [System.StringComparison]::OrdinalIgnoreCase)) { return $true }
    $dir = New-Object System.IO.DirectoryInfo($full)
    while ($null -ne $dir) {
        if (Test-Path -LiteralPath (Join-Path $dir.FullName '.victory-root')) { return $true }
        $dir = $dir.Parent
    }
    return $false
}

# The VICTORY_* names the Rust source reads, or $null without the source.
function Get-KnownSwitches {
    $dirs = @((Join-Path $repo 'crates'), (Join-Path $repo 'apps')) | Where-Object { Test-Path -LiteralPath $_ -PathType Container }
    if (-not $dirs) { return $null }
    $known = @{}
    $files = Get-ChildItem -LiteralPath $dirs -Recurse -Filter '*.rs' -File
    foreach ($m in (Select-String -LiteralPath $files.FullName -Pattern '"(VICTORY_[A-Z0-9_]+)"' -AllMatches)) {
        foreach ($match in $m.Matches) { $known[$match.Groups[1].Value] = $true }
    }
    return $known
}

function Get-SourceCommit {
    $ErrorActionPreference = 'Continue'
    if ($null -eq (Get-Command git -ErrorAction SilentlyContinue)) { return 'unknown (no git)' }
    $head = & git -C $repo rev-parse --short=12 HEAD 2>$null
    if ($LASTEXITCODE -ne 0) { return 'unknown (not a git checkout)' }
    $changed = @(& git -C $repo status --porcelain 2>$null).Count
    return "$head, $changed files changed against it"
}

# GPU name and driver as Windows reports them. Whether Resizable BAR is on is
# not visible from here: the layer's log says whether D3D12 GPU upload heaps
# are supported (they need it), shown as gpu_upload_heaps in report.txt.
function Get-GpuLines {
    try {
        return @(Get-CimInstance -ClassName Win32_VideoController | ForEach-Object { "GPU: $($_.Name), driver $($_.DriverVersion)" })
    }
    catch {
        return @('GPU: unknown (Win32_VideoController could not be read)')
    }
}

function Write-Utf8Lines([string]$file, [string[]]$lines) {
    [System.IO.File]::WriteAllLines($file, $lines, (New-Object System.Text.UTF8Encoding($false)))
}

# Names that this script sets for both runs; a variant may not change them.
$reserved = @('VICTORY_FRAME_LOG', 'VICTORY_GAME_DIR', 'VICTORY_CACHE_DIR', 'VICTORY_DATA_DIR', 'VICTORY_RAW_SAVE')

function Test-Variant([string]$label, [hashtable]$vars, $known) {
    foreach ($key in $vars.Keys) {
        if ("$key" -notmatch '^VICTORY_[A-Z0-9_]+$') { throw "${label}: only VICTORY_* variables belong in a variant: '$key'" }
        if ($reserved -contains $key) { throw "${label}: $key is set by this script for both runs" }
        if (($null -ne $known) -and (-not $known.ContainsKey($key))) {
            throw "${label}: the Rust code does not read $key (misspelt?); the two runs would be the same"
        }
    }
}

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "Build first: cargo build --release -p victory" }
if (-not (Test-Path -LiteralPath $GameExe -PathType Leaf)) { throw "Game not found: $GameExe" }
if ($RunSeconds -lt 10) { throw 'RunSeconds must be at least 10' }
if ($CacheSnapshot -and -not (Test-Path -LiteralPath $CacheSnapshot -PathType Container)) { throw "Cache snapshot not found: $CacheSnapshot" }
if ($DataSnapshot -and -not (Test-Path -LiteralPath $DataSnapshot -PathType Container)) { throw "Data snapshot not found: $DataSnapshot" }

$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath
$GameExe = (Resolve-Path -LiteralPath $GameExe).ProviderPath
$gameDir = [System.IO.Path]::GetDirectoryName($GameExe)
if ($CacheSnapshot) { $CacheSnapshot = (Resolve-Path -LiteralPath $CacheSnapshot).ProviderPath }
if ($DataSnapshot) { $DataSnapshot = (Resolve-Path -LiteralPath $DataSnapshot).ProviderPath }
if (-not $OutDir) { throw 'OutDir is empty: give -OutDir' }
$root = Get-FullPath $OutDir
if (Test-InsideRepo $root) { throw "OutDir is inside the source tree ($root); run folders must stay outside it (docs/asset-policy.md)" }
if ($RawSave) {
    $RawSave = Get-FullPath $RawSave
    if (Test-InsideRepo $RawSave) { throw "RawSave is inside the source tree ($RawSave); it must stay outside it" }
}

$known = Get-KnownSwitches
Test-Variant 'VariantA' $VariantA $known
Test-Variant 'VariantB' $VariantB $known

New-Item -ItemType Directory -Force -Path $root | Out-Null
$commit = Get-SourceCommit
$gpu = Get-GpuLines
foreach ($line in $gpu) { Write-Host $line }

# Save the caller's environment. The script restores it when it finishes,
# also after an error or Ctrl+C.
$saved = @{}
foreach ($item in [System.Environment]::GetEnvironmentVariables().GetEnumerator()) {
    if ($item.Key -like 'VICTORY_*') { $saved[$item.Key] = [string]$item.Value }
}

function Set-EnvVar([string]$name, [string]$value) {
    # Literal names and values; an empty value removes the variable.
    [System.Environment]::SetEnvironmentVariable($name, $value, 'Process')
}

function Clear-VictoryEnv {
    foreach ($key in @([System.Environment]::GetEnvironmentVariables().Keys)) {
        if ($key -like 'VICTORY_*') { Set-EnvVar $key $null }
    }
}

function Copy-Snapshot([string]$source, [string]$destination) {
    # Never reuse what an earlier call left here: each run starts from the snapshot.
    if (Test-Path -LiteralPath $destination) { Remove-Item -LiteralPath $destination -Recurse -Force }
    if (-not $source) { New-Item -ItemType Directory -Force -Path $destination | Out-Null; return }
    Copy-Item -LiteralPath $source -Destination $destination -Recurse -Force
}

function Invoke-Variant([string]$name, [hashtable]$vars) {
    $dir = Join-Path $root $name
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $cache = Join-Path $dir 'cache'
    $data = Join-Path $dir 'data'
    Copy-Snapshot $CacheSnapshot $cache
    Copy-Snapshot $DataSnapshot $data

    Clear-VictoryEnv
    # Common settings: identical in both runs. They are the code's defaults
    # and are set here so both runs carry them explicitly.
    Set-EnvVar 'VICTORY_REUSE' '1'
    Set-EnvVar 'VICTORY_RES' '1080p'
    Set-EnvVar 'VICTORY_TEXTURE_BACK' '1'
    Set-EnvVar 'VICTORY_PARALLEL' '1'
    # The variant's own variables come after, so they win.
    foreach ($key in $vars.Keys) { Set-EnvVar $key ([string]$vars[$key]) }
    # Frame log is mandatory here; directories are this run's own.
    Set-EnvVar 'VICTORY_FRAME_LOG' '1'
    Set-EnvVar 'VICTORY_GAME_DIR' $gameDir
    Set-EnvVar 'VICTORY_CACHE_DIR' $cache
    Set-EnvVar 'VICTORY_DATA_DIR' $data
    if ($RawSave) {
        $raw = Join-Path $RawSave $name
        New-Item -ItemType Directory -Force -Path $raw | Out-Null
        Set-EnvVar 'VICTORY_RAW_SAVE' $raw
    }

    $settings = @("run: $name", "source: $commit") + $gpu + @('variables set (nothing else VICTORY_* was set):')
    foreach ($item in ([System.Environment]::GetEnvironmentVariables().GetEnumerator() | Where-Object { $_.Key -like 'VICTORY_*' } | Sort-Object Key)) {
        $settings += "  $($item.Key)=$($item.Value)"
    }
    Write-Utf8Lines (Join-Path $dir 'settings.txt') $settings

    $stdout = Join-Path $dir 'stdout.txt'
    $stderr = Join-Path $dir 'stderr.txt'
    Write-Host "Starting $name for $RunSeconds seconds; output: $dir"
    $process = Start-Process -FilePath $Exe `
        -ArgumentList @('run', ('"' + $GameExe + '"'), '--execute') `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
        -PassThru -WorkingDirectory $dir
    # Keeps the process handle, so the exit code can be read after it ends.
    $null = $process.Handle
    $stopped = $false
    try {
        # Short waits, so Ctrl+C is not held up until the time is over.
        $deadline = [DateTime]::UtcNow.AddSeconds($RunSeconds)
        while (-not $process.WaitForExit(500)) {
            if ([DateTime]::UtcNow -ge $deadline) {
                Write-Host "${name}: still running, stopping the run"
                Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
                $stopped = $true
                $null = $process.WaitForExit(10000)
                break
            }
        }
    }
    finally {
        if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
    }
    $code = $null
    if (-not $stopped) { $code = $process.ExitCode }
    $process.Dispose()
    if ($stopped) { $end = "stopped by the script after $RunSeconds s" } else { $end = "ended by itself with exit code $code" }
    [System.IO.File]::AppendAllText((Join-Path $dir 'settings.txt'), "end: $end`r`n", (New-Object System.Text.UTF8Encoding($false)))
    if ((-not $stopped) -and ($code -ne 0)) {
        throw "${name}: victory ended with exit code $code before the time was up; see $stderr"
    }
    if (-not $stopped) { Write-Warning "${name}: the run ended before $RunSeconds s; its log is shorter than planned" }

    # Frame lines carry timings and per-frame draws, first-bind lines carry the
    # shader hashes used as anchors, and frame-presented lines carry guest
    # workload. `victory ab` reads the whole log; frames.txt is a short copy.
    $frames = @(Get-Content -LiteralPath $stderr -Encoding UTF8 -ErrorAction SilentlyContinue |
        Where-Object { $_ -match '(victory_gpu::framelog: |first bind |frame presented)' })
    Write-Utf8Lines (Join-Path $dir 'frames.txt') ([string[]]$frames)
    $readings = @($frames | Where-Object { $_ -match 'framelog: frame times:' }).Count
    Write-Host "${name}: $readings five-second frame readings collected"
    if ($readings -eq 0) { Write-Warning "${name}: no frame readings in $stderr (did the title reach its first frames?)" }

    & $Exe report $stderr --out $dir --scrub $gameDir | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "${name}: victory report failed (exit code $LASTEXITCODE)" }
    return $stderr
}

$abCode = 1
try {
    $logA = Invoke-Variant 'A' $VariantA
    $logB = Invoke-Variant 'B' $VariantB
    Write-Host ''
    $compare = @(& $Exe ab $logA $logB --min-draws $MinDraws)
    $abCode = $LASTEXITCODE
    foreach ($line in $compare) { Write-Host $line }
    Write-Utf8Lines (Join-Path $root 'compare.txt') ([string[]]$compare)
    Write-Host ''
    Write-Host "Runs and compare.txt: $root"
    Write-Host "To align by named anchors: & '$Exe' ab '$logA' '$logB' --min-draws $MinDraws --anchor <mesh-or-vertex hash>:<pixel hash>"
}
finally {
    Clear-VictoryEnv
    foreach ($key in $saved.Keys) { Set-EnvVar $key $saved[$key] }
}
exit $abCode
