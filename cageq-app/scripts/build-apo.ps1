<#
.SYNOPSIS
    Maintainer step: stage CAGEq's APO and its setup helper for bundling.

.DESCRIPTION
    Builds CAGEqApo.dll (C++ shim + Rust core) and cageq-apo-setup.exe, then copies both into
    src-tauri\apo\, which tauri.conf.json ships as a resource. Same shape as build-sidecar.ps1:
    a deliberate maintainer step rather than something `tauri build` triggers, because the DLL
    needs the MSVC toolchain.

    Both files must land in the SAME directory: the helper looks for the DLL beside itself,
    which is what lets `register` install it without being told a path.

    Run before `npm run tauri build`.
#>
param(
    [switch]$SkipDll
)
$ErrorActionPreference = 'Stop'

$repo   = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$apoDir = Join-Path $repo 'cageq-apo'
$stage  = Join-Path $repo 'cageq-app\src-tauri\apo'

# Native tools write progress to stderr, and with ErrorActionPreference = Stop PowerShell turns
# that into a terminating error even when the tool succeeded. The exit code is the real verdict,
# so these calls run with it relaxed and are checked explicitly.
function Invoke-Tool {
    param([string]$File, [string[]]$Arguments, [string]$What)
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $File @Arguments
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prev
    }
    if ($code -ne 0) { throw "$What failed (exit $code)" }
}

# 1) The APO itself. build.bat drives cl.exe + cargo and emits the DLL.
if (-not $SkipDll) {
    Write-Host '[apo] building CAGEqApo.dll'
    Invoke-Tool -File 'cmd.exe' -What 'cageq-apo build' -Arguments @(
        '/c', (Join-Path $apoDir 'build.bat'))
}
$dll = Join-Path $apoDir 'build\CAGEqApo.dll'
if (-not (Test-Path $dll)) { throw "missing $dll (run without -SkipDll)" }

# 2) The elevated setup helper. Static CRT, matching the shim's /MT: it runs on machines that
#    may have no VC++ redistributable, and a helper that cannot start looks exactly like setup
#    being broken.
Write-Host '[apo] building cageq-apo-setup.exe'
$cargo = (Get-Command cargo -ErrorAction SilentlyContinue).Source
if (-not $cargo) { $cargo = Join-Path $env:USERPROFILE '.cargo\bin\cargo.exe' }
if (-not (Test-Path $cargo)) { throw "cargo not found (looked for $cargo)" }

Push-Location $repo
try {
    Invoke-Tool -File $cargo -What 'cageq-apo-setup build' -Arguments @(
        'rustc', '--release', '-p', 'cageq-apo-backend', '--bin', 'cageq-apo-setup',
        '--', '-C', 'target-feature=+crt-static')
} finally {
    Pop-Location
}
$helper = Join-Path $repo 'target\release\cageq-apo-setup.exe'
if (-not (Test-Path $helper)) { throw "missing $helper" }

# 3) Stage them together.
New-Item -ItemType Directory -Path $stage -Force | Out-Null
Copy-Item $dll    (Join-Path $stage 'CAGEqApo.dll')        -Force
Copy-Item $helper (Join-Path $stage 'cageq-apo-setup.exe') -Force

Write-Host "[apo] staged -> $stage"
Get-ChildItem $stage | ForEach-Object { '    {0}  ({1:N0} bytes)' -f $_.Name, $_.Length }
Write-Host ''
Write-Host 'Now run:  npm run tauri build'
