<#
  Signs files with the self-signed certificate, for windows.yml's
  "self-signed" signing mode (installer/SIGNING.md). CI only: it reads the
  certificate from SELF_SIGN_PFX_BASE64 and SELF_SIGN_PFX_PASSWORD, which
  installer/make-self-signed-cert.sh makes.

  It also trusts the certificate on the runner (a throwaway VM), so the
  workflow's signature check reports Valid, as it does for a real
  certificate. It writes the certificate's thumbprint to the step's outputs,
  and with -ExportCer, saves its public half for the release.

    ./installer/self-sign.ps1 -Path a.exe, b.exe [-ExportCer cert.cer]
#>
param(
    [Parameter(Mandatory)][string[]]$Path,
    [string]$ExportCer
)
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Security
$X509 = 'System.Security.Cryptography.X509Certificates'

if (-not $env:SELF_SIGN_PFX_BASE64) { throw 'SELF_SIGN_PFX_BASE64 is not set (see installer/SIGNING.md).' }
$bytes = [Convert]::FromBase64String($env:SELF_SIGN_PFX_BASE64)
$flags = [System.Security.Cryptography.X509Certificates.X509KeyStorageFlags]'UserKeySet, PersistKeySet'
$cert = New-Object "$X509.X509Certificate2" -ArgumentList $bytes, $env:SELF_SIGN_PFX_PASSWORD, $flags
if (-not $cert.HasPrivateKey) { throw 'The PFX in SELF_SIGN_PFX_BASE64 has no private key.' }
if ($cert.NotAfter -lt (Get-Date)) { throw "The self-signed certificate expired on $($cert.NotAfter); make a new one." }
Write-Output "Certificate: $($cert.Subject), thumbprint $($cert.Thumbprint), valid until $($cert.NotAfter)"

# Trust it on this runner only.
$public = New-Object "$X509.X509Certificate2" -ArgumentList (, $cert.RawData)
foreach ($name in 'Root', 'TrustedPublisher') {
    $store = New-Object "$X509.X509Store" -ArgumentList $name, 'LocalMachine'
    $store.Open('ReadWrite')
    $store.Add($public)
    $store.Close()
}

# Timestamped, so signatures stay valid after the certificate expires.
# Timestamp servers are occasionally slow to answer, so it retries.
$timestampServers = 'http://timestamp.digicert.com', 'http://timestamp.sectigo.com'
foreach ($file in $Path) {
    $signed = $false
    foreach ($attempt in 1..3) {
        $server = $timestampServers[($attempt - 1) % $timestampServers.Count]
        $result = Set-AuthenticodeSignature -FilePath $file -Certificate $cert -HashAlgorithm SHA256 -TimestampServer $server
        if ($result.Status -eq 'Valid') { $signed = $true; break }
        Write-Warning "Signing $file with timestamp server $server : $($result.Status) $($result.StatusMessage)"
        Start-Sleep -Seconds (5 * $attempt)
    }
    if (-not $signed) { throw "Couldn't sign $file." }
    Write-Output "Signed $file"
}

if ($ExportCer) {
    [IO.File]::WriteAllBytes($ExportCer, $cert.RawData)
    Write-Output "Exported the public certificate to $ExportCer"
}
if ($env:GITHUB_OUTPUT) {
    "thumbprint=$($cert.Thumbprint)" | Out-File -FilePath $env:GITHUB_OUTPUT -Append -Encoding utf8
}
