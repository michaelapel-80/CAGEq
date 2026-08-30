#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Self-sign CAGEqApo.dll with a locally-trusted Authenticode certificate.

.DESCRIPTION
    ⚠ VM ONLY — this installs a self-signed CA into the machine's Root and TrustedPublisher
    stores. That is a real trust decision; do not run it on a machine you care about.

    Only needed if the DLL does not load unsigned. The earlier spike established that an
    APO is user-mode, so this is **Authenticode**, self-serviceable and free — NOT the
    kernel driver signing wall (WHQL/attestation via Partner Center + an EV certificate).
    That distinction is the whole reason Path A is viable for a non-commercial project.

    Deliberately uses only in-box tooling (New-SelfSignedCertificate + Set-AuthenticodeSignature
    + certutil), so it runs on a bare Windows VM with no SDK or signtool installed.

    Note that loading an unsigned/self-signed APO may also need
    HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Audio!DisableProtectedAudioDG = 1.
    That key governs DRM coexistence, not basic loading — try without it first, and record
    which combination actually worked, because that answer is what stage D has to ship.
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
