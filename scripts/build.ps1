# Release builds on Windows, organized by target, into dist\ (same layout as scripts/build.sh):
#
#   dist\<triple>\nib.exe, nibd.exe, BUILD_INFO   full client for this machine
#   dist\<triple>\.skills\                        agent skills (copied from the repo's .skills\)
#   dist\daemons\<triple>\nibd                    static Linux daemons for ssh/WSL remotes
#
# Usage:
#   scripts\build.ps1                # client + Linux daemons (daemons skipped if targets missing)
#   scripts\build.ps1 client         # this machine's client only
#   scripts\build.ps1 daemons        # Linux daemons only
#
# One-time setup for the Linux daemons:
#   rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl

param([ValidateSet('all', 'client', 'daemons')][string]$What = 'all')

. (Join-Path $PSScriptRoot 'nib-env.ps1')
Set-Location $NibRoot

$hostTriple = ((rustc -vV) | Where-Object { $_ -like 'host:*' }) -replace '^host:\s*', ''
$daemonTargets = if ($env:NIB_DAEMON_TARGETS) { $env:NIB_DAEMON_TARGETS -split '\s+' } else { @('x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl') }

function Get-BuildInfo {
    # Best effort (no git, or not a checkout, in some CI layouts): never fail the build over it.
    $ErrorActionPreference = 'Continue'
    $commit = git rev-parse --short HEAD 2>$null
    if (-not $commit) { $commit = 'unknown' }
    git diff --quiet 2>$null
    $dirty = if ($LASTEXITCODE -ne 0) { ' (dirty)' } else { '' }
    "commit $commit$dirty`nbuilt  $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))`nhost   $hostTriple"
}

function Build-Client {
    Write-Host "==> client for $hostTriple"
    cargo build --release -p nib -p nibd
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $out = "dist\$hostTriple"
    New-Item -ItemType Directory -Force $out | Out-Null
    Copy-Item target\release\nib.exe, target\release\nibd.exe $out -Force
    if (Test-Path "$out\.skills") { Remove-Item -Recurse -Force "$out\.skills" }
    Copy-Item -Recurse .skills "$out\.skills"
    Get-BuildInfo | Set-Content "$out\BUILD_INFO"
    Get-ChildItem $out -File | Format-Table Name, Length -AutoSize
}

function Build-Daemons {
    $env:CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = 'rust-lld'
    $env:CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER = 'rust-lld'
    $installed = rustup target list --installed 2>$null
    foreach ($t in $daemonTargets) {
        Write-Host "==> nibd for $t"
        if ($installed -notcontains $t) {
            Write-Warning "    skipped: run 'rustup target add $t' once"
            continue
        }
        cargo build --release -p nibd --target $t
        if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
        New-Item -ItemType Directory -Force "dist\daemons\$t" | Out-Null
        Copy-Item "target\$t\release\nibd" "dist\daemons\$t\nibd" -Force
        Get-BuildInfo | Set-Content "dist\daemons\$t\BUILD_INFO"
    }
}

switch ($What) {
    'all' { Build-Client; Build-Daemons }
    'client' { Build-Client }
    'daemons' { Build-Daemons }
}
Write-Host "done: $((Get-ChildItem dist -Recurse -File -Filter 'nib*' | ForEach-Object { $_.FullName.Substring($NibRoot.Length + 1) }) -join ' ')"
