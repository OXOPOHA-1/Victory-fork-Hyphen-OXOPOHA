# Runs the release build with switches chosen before the game starts.
#
# With no switch but -GameExe it runs with the code's defaults. The whole log
# (stderr.txt), the rest of the output (stdout.txt), what was set
# (settings.txt) and `victory report` of the log (report.txt) are kept in a
# new folder under -LogDir, outside the source tree. -Console shows the log in
# this window instead and keeps nothing. The caller's environment is put back
# when the script ends, also after an error or Ctrl+C.
param(
    [Parameter(Mandatory = $true)][string]$GameExe,
    [string]$Exe = "$PSScriptRoot\target\release\victory.exe",
    # 1 = keep the host texture/tile shell by address (the default).
    # 2 = also keep detiled bytes + hash by address (needs more work).
    # 0 = no reuse.
    [ValidateSet('0','1','2')][string]$Reuse = '1',
    # Internal render size: 1920x1080 by default.
    [ValidateSet('2k','1080p','720p')][string]$Res = '1080p',
    # VICTORY_INPLACE: title memory the card reads where it lies. Not set
    # unless given; the code's default is 2 (windows and title buffers in
    # place). 1 = windows only, 0 = off (copies as before).
    [ValidateSet('0','1','2')][string]$InPlace,
    # VICTORY_MESH_AMPLIFY: indirect mesh draws launched through the layer's
    # amplification stage. Not set unless given; the code's default is on.
    # 0 = a clamp pass in front of every indirect mesh draw.
    [ValidateSet('0','1')][string]$MeshAmplify,
    # VICTORY_FRAME_LOG=1: frame times every five seconds (off by default in a release build).
    [switch]$FrameLog,
    # VICTORY_RAW_SAVE=<dir>: raw blobs of the pipeline methods not implemented (outside the source tree).
    [string]$RawSave,
    # Where run folders go; must be outside the source tree.
    [string]$LogDir = (Join-Path $env:LOCALAPPDATA 'victory\runs'),
    # Show the log in this window and keep no files.
    [switch]$Console
)

$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot

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

if (-not (Test-Path -LiteralPath $Exe -PathType Leaf)) { throw "Build first: cargo build --release -p victory" }
if (-not (Test-Path -LiteralPath $GameExe -PathType Leaf)) { throw "Game not found: $GameExe" }
$Exe = (Resolve-Path -LiteralPath $Exe).ProviderPath
$GameExe = (Resolve-Path -LiteralPath $GameExe).ProviderPath
$gameDir = [System.IO.Path]::GetDirectoryName($GameExe)
if ($RawSave) {
    $RawSave = Get-FullPath $RawSave
    if (Test-InsideRepo $RawSave) { throw "RawSave is inside the source tree ($RawSave); it must stay outside it" }
}
$runDir = $null
if (-not $Console) {
    if (-not $LogDir) { throw 'LogDir is empty: give -LogDir' }
    $LogDir = Get-FullPath $LogDir
    if (Test-InsideRepo $LogDir) { throw "LogDir is inside the source tree ($LogDir); run folders must stay outside it (docs/asset-policy.md)" }
}

# The variables this script sets. Those after VICTORY_RES are the code's
# defaults, set here so the run carries them explicitly; none of them has
# been measured in a game run yet.
$vars = [ordered]@{
    VICTORY_REUSE            = $Reuse
    VICTORY_RES              = $Res
    VICTORY_HEAP_PASS_MARKED = '1'
    VICTORY_LOOKUP_CACHE     = '1'
    VICTORY_LIST_REUSE       = '1'
    VICTORY_IDLE_SCALE       = '4'
    VICTORY_FREE_THREAD      = '1'
    VICTORY_TEXTURE_BACK     = '1'
    VICTORY_PARALLEL         = '1'                  # fills and texture detiles on worker threads
}
# Pipeline compilation: one frame's wait (not half a second) and more workers.
# Override if a title needs a longer wait: $env:VICTORY_PIPELINE_WAIT_MS='100'
if ($InPlace) { $vars['VICTORY_INPLACE'] = $InPlace }
if ($MeshAmplify) { $vars['VICTORY_MESH_AMPLIFY'] = $MeshAmplify }
if ($FrameLog) { $vars['VICTORY_FRAME_LOG'] = '1' }
if ($RawSave) { $vars['VICTORY_RAW_SAVE'] = $RawSave }

if ($Reuse -eq '2') {
    Write-Warning 'VICTORY_REUSE=2 needs more work: detiled bytes are kept by guest address. Use only to test.'
}

# Variables already set in this window are kept (an override such as
# VICTORY_PIPELINE_WAIT_MS); they are listed so the run is not mistaken for
# the defaults.
$saved = @{}
foreach ($item in [System.Environment]::GetEnvironmentVariables().GetEnumerator()) {
    if ($item.Key -like 'VICTORY_*') { $saved[$item.Key] = [string]$item.Value }
}
$inherited = @($saved.Keys | Where-Object { -not $vars.Contains($_) } | Sort-Object | ForEach-Object { "  $_=$($saved[$_])" })
if ($inherited.Count -gt 0) {
    Write-Warning 'Also set in this window, and passed to the game:'
    foreach ($line in $inherited) { Write-Host $line }
}

$gpu = Get-GpuLines
foreach ($line in $gpu) { Write-Host $line }

$code = 1
try {
    foreach ($key in $vars.Keys) { [System.Environment]::SetEnvironmentVariable($key, [string]$vars[$key], 'Process') }
    if ($RawSave) { New-Item -ItemType Directory -Force -Path $RawSave | Out-Null }

    if ($Console) {
        & $Exe run $GameExe --execute
        $code = $LASTEXITCODE
    }
    else {
        $runDir = Join-Path $LogDir ((Get-Date -Format 'yyyy-MM-dd_HH-mm-ss') + "_reuse$Reuse" + "_$Res")
        New-Item -ItemType Directory -Force -Path $runDir | Out-Null
        $stdout = Join-Path $runDir 'stdout.txt'
        $stderr = Join-Path $runDir 'stderr.txt'
        $settings = @("source: $(Get-SourceCommit)") + $gpu + @('VICTORY_* variables passed to the game:')
        foreach ($item in ([System.Environment]::GetEnvironmentVariables().GetEnumerator() | Where-Object { $_.Key -like 'VICTORY_*' } | Sort-Object Key)) {
            $settings += "  $($item.Key)=$($item.Value)"
        }
        Write-Utf8Lines (Join-Path $runDir 'settings.txt') $settings
        Write-Host "Run folder: $runDir"
        Write-Host "The log goes to stderr.txt there (Get-Content -Wait shows it live). Close the game to end the run."

        $process = Start-Process -FilePath $Exe `
            -ArgumentList @('run', ('"' + $GameExe + '"'), '--execute') `
            -RedirectStandardOutput $stdout -RedirectStandardError $stderr `
            -WorkingDirectory $runDir -NoNewWindow -PassThru
        # Keeps the process handle, so the exit code can be read after it ends.
        $null = $process.Handle
        try {
            # Short waits, so Ctrl+C ends the script (and the game) at once.
            while (-not $process.WaitForExit(500)) { }
        }
        finally {
            if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
        }
        $code = $process.ExitCode
        $process.Dispose()
        [System.IO.File]::AppendAllText((Join-Path $runDir 'settings.txt'), "end: exit code $code`r`n", (New-Object System.Text.UTF8Encoding($false)))
    }
}
finally {
    foreach ($key in @([System.Environment]::GetEnvironmentVariables().Keys)) {
        if ($key -like 'VICTORY_*') { [System.Environment]::SetEnvironmentVariable($key, $null, 'Process') }
    }
    foreach ($key in $saved.Keys) { [System.Environment]::SetEnvironmentVariable($key, $saved[$key], 'Process') }
}

if ($runDir) {
    Write-Host ''
    Write-Host "The game ended with exit code $code."
    & $Exe report $stderr --out $runDir --scrub $gameDir | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Warning "victory report failed (exit code $LASTEXITCODE); the log is still in $stderr" }
    Write-Host "Run folder: $runDir"
    Write-Host 'report.txt has user paths replaced; stderr.txt is the whole log (it holds paths of this PC).'
}
exit $code
