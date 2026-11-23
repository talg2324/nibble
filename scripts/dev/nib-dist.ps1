# Run the latest compiled release for this machine (dist\<host-triple>\nib.exe, from
# scripts\build\build.ps1). Never builds; tells you if the build is older than the source.
#
#   C:\src\nibble\scripts\dev\nib-dist.ps1 some_file.py

$ErrorActionPreference = 'Stop'
$NibRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

$hostTriple = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
$bin = Join-Path $NibRoot "dist\$hostTriple\nib.exe"
if (-not (Test-Path $bin)) {
    Write-Error "nib-dist: no build for $hostTriple - run scripts\build\build.ps1"
    exit 1
}

$info = Join-Path $NibRoot "dist\$hostTriple\BUILD_INFO"
$built = if (Test-Path $info) { ((Get-Content $info | Where-Object { $_ -like 'commit *' }) -split ' ')[1] }
$head = try { git -C $NibRoot rev-parse --short HEAD 2>$null } catch { $null }
if ($built -and $head -and $built -ne $head) {
    Write-Warning "nib-dist: build is from $built, source is at $head (scripts\build\build.ps1 to refresh)"
}

if (-not $env:NIB_DAEMONS) { $env:NIB_DAEMONS = Join-Path $NibRoot 'dist\daemons' }
& $bin @args
exit $LASTEXITCODE
