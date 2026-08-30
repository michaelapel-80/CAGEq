#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Detach CAGEq's APO from an endpoint and unregister its COM server.

.DESCRIPTION
    The undo for register.ps1. Restores the endpoint's FxProperties from the backup that
    script made — byte-for-byte, rather than guessing which slots were originally set.

    Run this before reverting a snapshot too: it is the same procedure a real uninstall
    will have to perform, so exercising it is part of what stage B is checking. A stuck
    APO lives in audiodg and means broken audio machine-wide, so the detach path has to
    be as reliable as the attach path.
#>
param(
    [Parameter(Mandatory = $true)][string]$EndpointId
)
$ErrorActionPreference = 'Stop'

# Same normalisation as register.ps1 — PowerShell strips the braces off an unquoted {…}
# argument (it parses it as a ScriptBlock), and the two scripts must agree, since the
# backup filename is keyed on this value.
$bare = ($EndpointId -replace '[{}]', '').Trim()
if ($bare -notmatch '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$') {
    throw "Not a GUID: '$EndpointId'. Expected something like {6cafe423-cde5-4ec1-a1e2-e3fcec778349}"
}
$EndpointId = '{' + $bare + '}'

$clsid = '{530052E1-2CD4-400A-AC2B-0D19273AD5B7}'
$dll = Join-Path (Split-Path $PSScriptRoot -Parent) 'build\CAGEqApo.dll'
$fx = "HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render\$EndpointId\FxProperties"

# 1) Detach first, then unregister. This order matters: leaving a CLSID in an endpoint's
#    effect chain whose COM server no longer resolves is exactly the state that breaks an
#    endpoint, so the reference goes away before the thing it points at does.
$p = '{d04e05a6-594b-4fb6-a80d-01af5eed7d1d}'      # effect CLSID, per slot
$pm = '{d3993a3f-99c2-4402-b5ec-a92a0367664b}'     # supported processing modes, per slot
$enh = '{1da5d803-d492-4edd-8c23-e0c0ffee7f0e},5'  # PKEY_AudioEndpoint_Disable_SysFx
$slots = '1', '2', '5', '6', '7'
$modeSlots = '5', '6', '7'
$backup = Join-Path $PSScriptRoot "fx-backup-$EndpointId.txt"

if (-not (Test-Path $fx)) {
    "No FxProperties under $EndpointId — nothing to detach."
}
elseif (Test-Path $backup) {
    # Restore value-by-value through the registry provider — deliberately NOT `reg import`,
    # which needs full access to a TrustedInstaller-owned key and fails there even though
    # per-value writes succeed. This mirrors exactly how register.ps1 wrote them.
    $saved = @{}
    foreach ($line in Get-Content $backup) {
        if ($line -match '^([A-Za-z]+\d*)=(.*)$') { $saved[$Matches[1]] = $Matches[2] }
    }

    # Effect CLSID per slot.
    foreach ($s in $slots) {
        $want = $saved["fx$s"]
        if ($null -eq $want) { continue }   # not recorded; leave it alone
        if ($want -eq '<absent>') {
            Remove-ItemProperty -Path $fx -Name "$p,$s" -ErrorAction SilentlyContinue
        } else {
            New-ItemProperty -Path $fx -Name "$p,$s" -Value $want -PropertyType String -Force | Out-Null
        }
    }

    # Processing modes per slot (REG_MULTI_SZ, stored ';'-joined in the backup). Restored
    # rather than left behind: register.ps1 may have created it on an endpoint that never
    # had one, and leaving a modes declaration for a slot with no APO is not the state we
    # found the machine in.
    foreach ($s in $modeSlots) {
        $want = $saved["pm$s"]
        if ($null -eq $want) { continue }
        if ($want -eq '<absent>') {
            Remove-ItemProperty -Path $fx -Name "$pm,$s" -ErrorAction SilentlyContinue
        } else {
            New-ItemProperty -Path $fx -Name "$pm,$s" -Value ($want -split ';') -PropertyType MultiString -Force | Out-Null
        }
    }

    # PKEY_AudioEndpoint_Disable_SysFx — put the user's "disable enhancements" choice back.
    $want = $saved['enh']
    if ($null -ne $want) {
        if ($want -eq '<absent>') {
            Remove-ItemProperty -Path $fx -Name $enh -ErrorAction SilentlyContinue
        } else {
            New-ItemProperty -Path $fx -Name $enh -Value ([int]$want) -PropertyType DWord -Force | Out-Null
        }
    }

    "Restored effect slots from $backup"
    Get-Content $backup | ForEach-Object { "    $_" }
}
else {
    # No backup (registered by hand, or it was deleted): clear our CLSID out of every slot
    # rather than leaving a dangling reference. Only ours — never touch a vendor's.
    $props = Get-ItemProperty $fx
    foreach ($s in $slots) {
        $name = "$p,$s"
        if ($props.$name -eq $clsid) {
            Remove-ItemProperty -Path $fx -Name $name -ErrorAction SilentlyContinue
            "Cleared $name (was ours)"
        }
    }
    "No backup found — cleared only CAGEq's own CLSID, left everything else alone."
}

# 2) COM server: DllUnregisterServer -> UnregisterAPO + remove the CLSID keys.
if (Test-Path $dll) {
    # -Wait/-PassThru for the same reason as register.ps1: regsvr32 is GUI-subsystem, so
    # `& regsvr32` would not be waited on and its exit code would be meaningless.
    $rc = Start-Process regsvr32 -ArgumentList '/s', '/u', "`"$dll`"" -Wait -PassThru
    if ($rc.ExitCode -ne 0) { "WARNING: regsvr32 /u exit $($rc.ExitCode) — check HKLM\SOFTWARE\Classes\CLSID\$clsid" }
    "COM server unregistered: $clsid"
} else {
    "DLL not found ($dll) — skipping regsvr32 /u. Remove HKLM\SOFTWARE\Classes\CLSID\$clsid by hand if it lingers."
}

Restart-Service audiosrv -Force
"audiosrv restarted. Play audio to confirm the endpoint works normally again."
