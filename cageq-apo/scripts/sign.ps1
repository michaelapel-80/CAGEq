#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Self-sign CAGEqApo.dll. RETAINED AS A NEGATIVE RESULT — this does not make it load.

.DESCRIPTION
    ⚠ VM ONLY — installs a self-signed CA into the machine's Root and TrustedPublisher
    stores. A real trust decision; do not run it on a machine you care about.

    MEASURED ON THE VM, 2026-08-30 — signing is NOT the gate:

      DisableProtectedAudioDG = 0/unset : does NOT load, unsigned OR self-signed
      DisableProtectedAudioDG = 1       : loads unsigned

    So Windows' APO signature check is not satisfiable by a self-signed certificate (it
    wants a Microsoft-rooted chain, which is not self-serviceable), and the registry key —
    which disables that check outright — is what actually decides it. Equalizer APO's own
    installer sets the same key (Setup/Setup.nsi:279) and its Developer.txt says so
    explicitly, so this is the incumbent's deployment model, not a CAGEq compromise.

    Kept rather than deleted so the negative result stays reproducible: if a future Windows
    build changes APO signing policy, re-running this is how you would find out. It has no
    role in normal bring-up — use the registry key.
#>
$ErrorActionPreference = 'Stop'

$dll = Join-Path (Split-Path $PSScriptRoot -Parent) 'build\CAGEqApo.dll'
if (-not (Test-Path $dll)) { throw "Build first (build.bat) — $dll not found" }

$subject = 'CN=CAGEq Test Signing'

$cert = Get-ChildItem Cert:\CurrentUser\My | Where-Object { $_.Subject -eq $subject } | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate `
        -Subject $subject `
        -Type CodeSigningCert `
        -CertStoreLocation Cert:\CurrentUser\My `
        -NotAfter (Get-Date).AddYears(2)
    "Created certificate: $($cert.Thumbprint)"
} else {
    "Reusing certificate: $($cert.Thumbprint)"
}

# Trust it machine-wide: Root so the chain validates, TrustedPublisher so Windows will
# load it without prompting. Exported without the private key — only the public half is
# needed to trust a signature.
$cerPath = Join-Path $env:TEMP 'cageq-test-signing.cer'
Export-Certificate -Cert $cert -FilePath $cerPath -Force | Out-Null
& certutil -addstore -f Root $cerPath | Out-Null
& certutil -addstore -f TrustedPublisher $cerPath | Out-Null
"Trusted in Root + TrustedPublisher"

$result = Set-AuthenticodeSignature -FilePath $dll -Certificate $cert -HashAlgorithm SHA256
"Signature status: $($result.Status)"
if ($result.Status -ne 'Valid') { throw "Signing failed: $($result.StatusMessage)" }

"`nSigned $dll — now re-run register.ps1."
