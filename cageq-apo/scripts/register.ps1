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

# 2) Back the effect slots up BEFORE touching them.
#
# Recorded as a plain slot=value list, NOT via `reg export`/`reg import`: importing needs to
# open the key for full access and MMDevices is TrustedInstaller-owned, so the import fails
# ("Fehler beim Zugriff auf die Registrierung") even though setting individual values through
# the provider works fine. Restoring the same way we wrote is symmetric and uses only APIs
# proven to work on this key. A .reg export is still taken alongside, purely as a
# human-readable artifact for manual recovery.
$p = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'            # effect CLSID, per slot
$pm = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'           # supported processing modes, per slot
$enh = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5'        # PKEY_AudioEndpoint_Disable_SysFx
$modeDefault = '{C18E2F7E-933D-4965-B7D1-1EEF228D2AF3}'  # AUDIO_SIGNALPROCESSINGMODE_DEFAULT
$slots = '1', '2', '5', '6', '7'
$modeSlots = '5', '6', '7'   # LFX/GFX are legacy and predate processing modes
$backup = Join-Path $PSScriptRoot "fx-backup-$EndpointId.txt"

$existing = Get-ItemProperty $fx
$lines = @()
foreach ($s in $slots) {
    $v = $existing."$p,$s"
    if ($null -eq $v) { $v = '<absent>' }
    $lines += "fx$s=$v"
}
foreach ($s in $modeSlots) {
    $v = $existing."$pm,$s"
    if ($null -eq $v) { $v = '<absent>' } else { $v = ($v -join ';') }
    $lines += "pm$s=$v"
}
$v = $existing.$enh
if ($null -eq $v) { $v = '<absent>' }
$lines += "enh=$v"

Set-Content -Path $backup -Value $lines -Encoding UTF8
"Backed up effect slots -> $backup"
$lines | ForEach-Object { "    $_" }

& reg export "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$EndpointId\FxProperties" `
    (Join-Path $PSScriptRoot "fx-backup-$EndpointId.reg") /y 2>&1 | Out-Null

# 3) Attach. Effect-slot property GUID; index picks the slot.
$idx = @{ LFX = '1'; GFX = '2'; SFX = '5'; MFX = '6'; EFX = '7' }[$Slot]

# Clear the other CLSID slots so a previous attempt can't linger and confuse the result.
$slots | Where-Object { $_ -ne $idx } | ForEach-Object {
    Remove-ItemProperty -Path $fx -Name "$p,$_" -ErrorAction SilentlyContinue
}
New-ItemProperty -Path $fx -Name "$p,$idx" -Value $clsid -PropertyType String -Force | Out-Null
"Set  $p,$idx = $clsid   (slot $Slot)"

# Declare which processing modes this slot's APO supports.
#
# THIS IS REQUIRED, and its absence is why the DLL previously loaded only after Equalizer
# APO had been installed once: modern Windows will not load an APO in a slot that does not
# declare its processing modes, and EqAPO's installer writes this value `if (!exists)` —
# so our CLSID was silently inheriting EqAPO's. On a clean machine there is nothing to
# inherit and the APO is simply skipped, with no error anywhere.
if ($modeSlots -contains $idx) {
    $pmName = "$pm,$idx"
    if ($null -eq (Get-ItemProperty $fx).$pmName) {
        New-ItemProperty -Path $fx -Name $pmName -Value @($modeDefault) -PropertyType MultiString -Force | Out-Null
        "Set  $pmName = $modeDefault   (processing modes)"
    } else {
        "Kept $pmName (already declared)"
    }
}

# Force-enable enhancements: with PKEY_AudioEndpoint_Disable_SysFx set, Windows bypasses
# the endpoint's whole effect chain, so no APO runs at all. EqAPO deletes this for the same
# reason; the original value is recorded in the backup above and restored on unregister.
if ($null -ne (Get-ItemProperty $fx).$enh) {
    Remove-ItemProperty -Path $fx -Name $enh -ErrorAction SilentlyContinue
    "Cleared $enh (enhancements were disabled for this endpoint)"
}

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
