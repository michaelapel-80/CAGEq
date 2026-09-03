<#
.SYNOPSIS
    Warn (or fail) when the APO's source is newer than the DLL/helper staged for bundling.

.DESCRIPTION
    build-apo.ps1 stages fresh binaries into src-tauri\apo\ for tauri.conf.json to bundle, but
    nothing rebuilds them automatically - see that script's own doc for why (the DLL needs the
    MSVC toolchain, so `tauri build` cannot just do it itself). Forgetting to re-run it after an
    APO change ships a stale DLL that silently fails its own update check: `dll_current`
    compares the staged ("shipped") copy against the installed one, and if neither was ever
    refreshed they still match - just both wrong.

    This is a filesystem mtime check, not a rebuild - cheap, and needs no MSVC toolchain, so it
    can run on every dev/build invocation without slowing either down.

.PARAMETER Block
    Exit 1 (not just warn) when stale - for beforeBuildCommand, where shipping a stale DLL is a
    real, packaged mistake. Omit for beforeDevCommand, where blocking every frontend-only dev
    cycle over an unrelated stale APO build would be its own annoyance.
#>
param(
    [switch]$Block
)

$repo  = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$stage = Join-Path $repo 'cageq-app\src-tauri\apo'

# Everything build-apo.ps1 actually compiles to produce the two staged files - see its own
# `cargo rustc -p cageq-apo` / `-p cageq-apo-backend --bin cageq-apo-setup` calls. cageq-backend
# is included because cageq-apo-backend depends on it directly (the Capabilities/EqBackend
# trait), so a change there feeds the staged setup helper just as much as a change in either
# crate's own src\.
$watch = @(
    (Join-Path $repo 'cageq-apo\src'),
    (Join-Path $repo 'cageq-apo\shim'),
    (Join-Path $repo 'cageq-apo\Cargo.toml'),
    (Join-Path $repo 'cageq-apo-backend\src'),
    (Join-Path $repo 'cageq-apo-backend\Cargo.toml'),
    (Join-Path $repo 'cageq-backend\src'),
    (Join-Path $repo 'cageq-backend\Cargo.toml')
)
$staged = @('CAGEqApo.dll', 'cageq-apo-setup.exe') | ForEach-Object { Join-Path $stage $_ }

$missing = $staged | Where-Object { -not (Test-Path $_) }
if ($missing) {
    Write-Warning "[apo-staleness] never staged: $($missing -join ', ') -- run cageq-app\scripts\build-apo.ps1"
    if ($Block) { exit 1 }
    exit 0
}

$stagedOldest = ($staged | Get-Item | Measure-Object -Property LastWriteTimeUtc -Minimum).Minimum
$sourceNewest = $watch |
    Where-Object { Test-Path $_ } |
    ForEach-Object { Get-ChildItem $_ -Recurse -File -ErrorAction SilentlyContinue } |
    Measure-Object -Property LastWriteTimeUtc -Maximum |
    Select-Object -ExpandProperty Maximum

if ($sourceNewest -and $sourceNewest -gt $stagedOldest) {
    Write-Warning ("[apo-staleness] cageq-apo / cageq-apo-backend / cageq-backend source is " +
        "newer than the staged DLL/setup helper in src-tauri\apo -- run " +
        "cageq-app\scripts\build-apo.ps1 before shipping, or the in-app update check " +
        "(dll_current) will not notice the new build either, since it compares the stale " +
        "staged copy against itself.")
    if ($Block) { exit 1 }
}
exit 0
