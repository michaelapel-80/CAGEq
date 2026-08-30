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
    # Administrators + SYSTEM full control, everyone else read-only. audiodg runs as
    # LocalService and only needs to read; whoever configures CAGEq needs write, which an
    # installer should grant to that specific account rather than to Users at large.
    $acl = Get-Acl $dir
    $acl.SetAccessRuleProtection($true, $false)   # drop inherited, possibly-permissive rules
    $acl.Access | ForEach-Object { [void]$acl.RemoveAccessRule($_) }
    foreach ($who in 'BUILTIN\Administrators', 'NT AUTHORITY\SYSTEM') {
        $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
            $who, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    }
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
        'BUILTIN\Users', 'ReadAndExecute', 'ContainerInherit,ObjectInherit', 'None', 'Allow')))
    Set-Acl -Path $dir -AclObject $acl
    "Locked down $dir (Administrators/SYSTEM write, Users read)"
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
