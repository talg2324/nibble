# Shared setup for the Windows scripts (dot-sourced by nib.ps1, nib-dist.ps1, build.ps1):
# puts cargo and an MSVC toolchain on PATH when they aren't already, and sets $NibRoot.
#
# MSVC: if link.exe is not on PATH, the first of these is dot-sourced to provide it:
#   $env:NIB_MSVC_ENV                                  (any script that sets PATH/INCLUDE/LIB)
#   $env:USERPROFILE\Documents\portable-msvc\msvc-env.ps1   (portable MSVC, no admin needed)
# A regular Visual Studio / Build Tools install needs none of this: cargo finds it by itself.

$ErrorActionPreference = 'Stop'
$NibRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path (Join-Path $cargoBin 'cargo.exe')) {
        $env:PATH = "$cargoBin;$env:PATH"
    } else {
        throw 'nib: cargo not found (winget install Rustlang.Rustup)'
    }
}

$vsInstalled = Test-Path "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not $vsInstalled -and -not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
    # @(...): a single match must stay an array (Windows PowerShell 5.1 would unwrap it to a string).
    $candidates = @(@($env:NIB_MSVC_ENV, (Join-Path $env:USERPROFILE 'Documents\portable-msvc\msvc-env.ps1')) |
        Where-Object { $_ -and (Test-Path $_) })
    if ($candidates) {
        . $candidates[0]
    } else {
        throw 'nib: no MSVC toolchain found. Install Visual Studio Build Tools (C++ workload), or set NIB_MSVC_ENV to a script that puts cl.exe/link.exe on PATH.'
    }
}
