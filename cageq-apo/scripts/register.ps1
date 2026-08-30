#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Register CAGEq's APO COM server and attach it to ONE render endpoint.

.DESCRIPTION
    ⚠ RUN ON THE SNAPSHOTTED VM ONLY, on a SCRATCH endpoint.

    Attaching a broken or mis-configured APO can silence an endpoint, and the APO runs
    inside audiodg.exe — the process that owns audio for the whole machine. Take a
    snapshot first; `unregister.ps1` restores the endpoint from the backup this makes,
    but a snapshot is the thing that always works.

    With no -EndpointId this only lists endpoints so you can pick one.

.PARAMETER Slot
    Which effect slot to attach to. EFX (,7) is the endpoint/post-mix effect — the slot
    Equalizer APO uses, and the one the earlier spike found actually loads a manually
    registered APO here (SFX ,5 and LFX ,1 did not).
#>
param(
    [string]$EndpointId,
    [ValidateSet('EFX', 'MFX', 'SFX', 'GFX', 'LFX')] [string]$Slot = 'EFX'
)
$ErrorActionPreference = 'Stop'

# MUST match CLSID_CageqApo in shim/cageq_apo.cpp.
$clsid = '{530052E1-2CD4-400A-AC2B-0D19273AD5B7}'
$dll = Join-Path (Split-Path $PSScriptRoot -Parent) 'build\CAGEqApo.dll'
if (-not (Test-Path $dll)) { throw "Build first (build.bat) — $dll not found" }

$root = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'

# Normalise the endpoint id to the braced form the registry actually uses.
#
# Necessary because PowerShell parses an UNQUOTED `{6cafe...}` argument as a ScriptBlock and
# stringifies it WITHOUT the braces — so `-EndpointId {6cafe...}`, copy-pasted straight from
# the listing above, silently arrives brace-less and matches no key. Accepting both forms is
# better than demanding quotes for an id this script itself printed with braces.
if ($EndpointId) {
    $bare = ($EndpointId -replace '[{}]', '').Trim()
    if ($bare -notmatch '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$') {
        throw "Not a GUID: '$EndpointId'. Expected something like {6cafe423-cde5-4ec1-a1e2-e3fcec778349}"
    }
    $EndpointId = '{' + $bare + '}'
}

if (-not $EndpointId) {
    "`n--- Render endpoints ---"
    "Pick a SCRATCH one with fx=True, then: register.ps1 -EndpointId <GUID>`n"
    Get-ChildItem $root | ForEach-Object {
        $props = Get-ItemProperty "$($_.PSPath)\Properties" -ErrorAction SilentlyContinue
        $name = $props.'{a45c254e-df1c-4efd-8020-67d146a850e0},2'
        $state = (Get-ItemProperty $_.PSPath -ErrorAction SilentlyContinue).DeviceState
        $hasFx = Test-Path "$($_.PSPath)\FxProperties"
        "  {0}  fx={1}  state={2}  {3}" -f $_.PSChildName, $hasFx, $state, $name
    }
    return
}

$fx = "$root\$EndpointId\FxProperties"
if (-not (Test-Path $fx)) { throw "No FxProperties under $EndpointId (pick one listed with fx=True)" }

# The actual load gate, verified on the VM 2026-08-30: without this key an APO must pass
# Windows' APO signature check, which a self-signed certificate does NOT satisfy (tested:
# unsigned and self-signed both fail to load). With it set, unsigned loads fine.
# Equalizer APO's own installer sets exactly this (Setup/Setup.nsi:279) and its docs say so
# outright — so CAGEq's APO is in the same deployment position as the incumbent, not a worse
# one. Warned about rather than set here: it is a machine-wide change that belongs to an
# installer, not to a test script.
$audioKey = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio'
$dg = (Get-ItemProperty $audioKey -Name DisableProtectedAudioDG -ErrorAction SilentlyContinue).DisableProtectedAudioDG
if ($dg -ne 1) {
    Write-Warning @"
DisableProtectedAudioDG is $(if ($null -eq $dg) { 'NOT SET' } else { $dg }) — the APO will almost certainly NOT load.
Windows' APO signature check rejects unsigned AND self-signed DLLs; this key disables it.
To proceed on this VM:
  New-ItemProperty -Path '$audioKey' -Name DisableProtectedAudioDG -PropertyType DWord -Value 1 -Force
  Restart-Service audiosrv -Force
(Equalizer APO's installer sets the same value. Note the documented trade-off: apps
requiring a secure audio path may change behaviour or refuse to output audio.)
"@
}

# 1) COM server: DllRegisterServer -> RegisterAPO + HKLM\SOFTWARE\Classes\CLSID\{..}\InprocServer32.
#
# Start-Process -Wait, not `& regsvr32`: regsvr32 is a GUI-subsystem binary, so PowerShell
# does not wait for it and $LASTEXITCODE would be whatever the previous command left behind
# — i.e. a silent false "registered" that costs a debugging cycle on the VM.
$rc = Start-Process regsvr32 -ArgumentList '/s', "`"$dll`"" -Wait -PassThru
if ($rc.ExitCode -ne 0) {
    throw "regsvr32 failed (exit $($rc.ExitCode)). Check: is the DLL x64, are you elevated, and does DllRegisterServer's RegisterAPO call succeed?"
}
"COM server registered: $clsid"
"  -> $dll"

# 2) Back the endpoint's effect chain up BEFORE touching it, so unregister can restore it
#    byte-for-byte rather than guessing what was there.
$backup = Join-Path $PSScriptRoot "fx-backup-$EndpointId.reg"
& reg export "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$EndpointId\FxProperties" "$backup" /y | Out-Null
"Backed up FxProperties -> $backup"

# 3) Attach. Effect-slot property GUID; index picks the slot.
$p = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'
$idx = @{ LFX = '1'; GFX = '2'; SFX = '5'; MFX = '6'; EFX = '7' }[$Slot]

# Clear the other CLSID slots so a previous attempt can't linger and confuse the result.
'1', '2', '5', '6', '7' | Where-Object { $_ -ne $idx } | ForEach-Object {
    Remove-ItemProperty -Path $fx -Name "$p,$_" -ErrorAction SilentlyContinue
}
New-ItemProperty -Path $fx -Name "$p,$idx" -Value $clsid -PropertyType String -Force | Out-Null
"Set  $p,$idx = $clsid   (slot $Slot)"

Restart-Service audiosrv -Force
@"

audiosrv restarted. To check whether it loaded:
  1. Play audio on that endpoint (audiodg only loads APOs once a stream is running).
  2. Process Explorer -> audiodg.exe -> lower pane (Ctrl+D, DLL view) -> look for CAGEqApo.dll.
     Loaded + audio still audible = stage B passes (it loads AND passes audio through).
  3. Silence, or the endpoint stops working -> unregister.ps1 -EndpointId $EndpointId

If it does NOT load: check DisableProtectedAudioDG=1 first (see the warning above) — that
is the gate, NOT signing. Then try -Slot LFX. Then Event Viewer -> Windows Logs -> System.
"@
