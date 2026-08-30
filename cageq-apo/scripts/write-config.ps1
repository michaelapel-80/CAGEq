<#
.SYNOPSIS
    Write a persistent APO correction for one endpoint.

.DESCRIPTION
    Bring-up and debugging aid: produces the file the APO loads at LockForProcess, so a
    correction can be applied without CAGEq itself. The real app will write the same format.

    Bands are "<TYPE> <Fc> <gain dB> <Q>", type one of PK / LSC / HSC / BP, e.g.
        -Bands 'PK 120 12 1.0','HSC 8000 -3 0.7'

.PARAMETER Elevated
    Also lock down the config directory's ACL (needs admin). The APO parses these files
    inside audiodg, which runs as a SERVICE account, from a directory an ordinary user can
    write — so if the directory is writable by everyone, one user can silently alter another
    user's EQ, and no amount of parser hardening changes that. Pass this once at setup.

.EXAMPLE
    .\write-config.ps1 -EndpointId {6cafe423-...} -PreampDb -12 -Bands 'PK 120 12 1.0'
#>
param(
    [Parameter(Mandatory = $true)][string]$EndpointId,
    [double]$PreampDb = 0.0,
    [string[]]$Bands = @(),
    [switch]$Elevated
)
$ErrorActionPreference = 'Stop'

# Same normalisation as register.ps1 — PowerShell strips the braces off an unquoted {…}.
$bare = ($EndpointId -replace '[{}]', '').Trim()
if ($bare -notmatch '^[0-9a-fA-F]{8}-([0-9a-fA-F]{4}-){3}[0-9a-fA-F]{12}$') {
    throw "Not a GUID: '$EndpointId'"
}
$EndpointId = '{' + $bare + '}'

$dir = Join-Path $env:ProgramData 'CAGEq\apo'
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Path $dir -Force | Out-Null }

if ($Elevated) {
    # Identities are given as SIDs, never as names: account names are LOCALIZED, so
    # 'BUILTIN\Administrators' and 'NT AUTHORITY\SYSTEM' do not resolve on a German (or any
    # non-English) Windows and AddAccessRule fails with "some or all identity references
    # could not be translated". SIDs are invariant.
    $SYSTEM = [System.Security.Principal.SecurityIdentifier]'S-1-5-18'
    $ADMINS = [System.Security.Principal.SecurityIdentifier]'S-1-5-32-544'
    $AUTHED = [System.Security.Principal.SecurityIdentifier]'S-1-5-11'   # Authenticated Users

    # Authenticated Users get Modify, not read-only: CAGEq runs UNELEVATED and has to write
    # this file. Read-only here would lock out the very app the directory exists for.
    #
    # It also matches the control channel's access policy deliberately. The channel already
    # lets authenticated users push arbitrary (validated) coefficients live, so restricting
    # the file more tightly would be a strong lock on one door and an open window beside it.
    # What this DOES buy is dropping ProgramData's inherited rules, so the directory is not
    # writable by anonymous or by whatever a parent ACL happens to permit.
    $acl = Get-Acl $dir
    $acl.SetAccessRuleProtection($true, $false)   # stop inheriting, keep nothing
    $acl.Access | ForEach-Object { [void]$acl.RemoveAccessRule($_) }
    foreach ($sid in $SYSTEM, $ADMINS) {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    }
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        $AUTHED, 'Modify', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    Set-Acl -Path $dir -AclObject $acl
    "Locked down $dir (SYSTEM/Administrators full, Authenticated Users modify, no inheritance)"
}

# Validate before writing: the APO refuses a malformed file wholesale, and finding that out
# from a silent passthrough on the VM is a slow way to learn about a typo.
$lines = @('cageq-apo 1', ('preamp {0:0.####}' -f $PreampDb))
foreach ($b in $Bands) {
    $p = $b -split '\s+'
    if ($p.Count -ne 4) { throw "Band needs 4 fields '<TYPE> <Fc> <gain> <Q>': '$b'" }
    if ($p[0] -notin 'PK', 'LSC', 'HSC', 'BP') { throw "Unknown filter type '$($p[0])' in '$b'" }
    $lines += ('band {0} {1:0.####} {2:0.####} {3:0.####}' -f $p[0], [double]$p[1], [double]$p[2], [double]$p[3])
}

$path = Join-Path $dir "$EndpointId.cfg"
Set-Content -Path $path -Value $lines -Encoding UTF8
"Wrote $path"
$lines | ForEach-Object { "    $_" }
"`nRestart audio (or replug/reselect the device) so the APO re-locks and reloads:"
"    Restart-Service audiosrv -Force        # elevated"
