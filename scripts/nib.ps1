# Run nib from the current source tree (Windows). Rebuilds only what changed (incremental `fast`
# profile), then runs it with your arguments, from your current directory.
#
#   C:\src\nibble\scripts\nib.ps1 some_file.py
#   nib .                        (through scripts\nib.cmd, once scripts\ is on PATH)
#
# Same as scripts/nib on macOS/Linux. nib.exe finds nibd.exe next to itself.

. (Join-Path $PSScriptRoot 'nib-env.ps1')

cargo build --quiet --profile fast -p nib -p nibd --manifest-path (Join-Path $NibRoot 'Cargo.toml')
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

# Remote (ssh/WSL) daemons are uploaded by content hash; a stale one is refused by the handshake.
# Only checked when some were built (scripts\build.ps1 daemons); local use doesn't need them.
$daemons = Get-ChildItem (Join-Path $NibRoot 'dist\daemons\*\nibd') -ErrorAction SilentlyContinue
if ($daemons) {
    $oldest = ($daemons | Sort-Object LastWriteTime | Select-Object -First 1).LastWriteTime
    $newer = Get-ChildItem (Join-Path $NibRoot 'crates\nibd'), (Join-Path $NibRoot 'crates\nib-proto') -Recurse -File |
        Where-Object LastWriteTime -gt $oldest | Select-Object -First 1
    if ($newer -or (Get-Item (Join-Path $NibRoot 'Cargo.lock')).LastWriteTime -gt $oldest) {
        Write-Warning 'nib: nibd sources changed since the remote daemons were built; run scripts\build.ps1 daemons for ssh/WSL'
    }
}

if (-not $env:NIB_DAEMONS) { $env:NIB_DAEMONS = Join-Path $NibRoot 'dist\daemons' }
& (Join-Path $NibRoot 'target\fast\nib.exe') @args
exit $LASTEXITCODE
