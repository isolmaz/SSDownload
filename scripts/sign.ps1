# Authenticode-signs one file and verifies the signature.
#
# Three sources are supported:
#   -SigningProfile <metadata.json>  Azure Artifact Signing (Trusted Signing) through
#                                    signtool's /dlib client; the dlib path comes from
#                                    -Dlib or SSDOWNLOAD_SIGNING_DLIB, the timestamp
#                                    server from -TimestampServer (Microsoft's by default).
#   -FromEnvironment                 the same, with the profile and timestamp server read
#                                    from SSDOWNLOAD_SIGNING_PROFILE / _TIMESTAMP.
#   -CertificateThumbprint <sha1>    a code-signing certificate in Cert:\CurrentUser\My,
#                                    with -TimestampServer.
[CmdletBinding(DefaultParameterSetName = 'Certificate')]
param(
    [Parameter(Mandatory)] [string] $Path,
    [Parameter(Mandatory, ParameterSetName = 'Certificate')]
    [ValidatePattern('^[A-Fa-f0-9]{40}$')] [string] $CertificateThumbprint,
    [Parameter(Mandatory, ParameterSetName = 'Profile')] [string] $SigningProfile,
    [Parameter(ParameterSetName = 'Profile')] [string] $Dlib = $env:SSDOWNLOAD_SIGNING_DLIB,
    # Reads the profile and timestamp server from SSDOWNLOAD_SIGNING_PROFILE and
    # SSDOWNLOAD_SIGNING_TIMESTAMP (used by the NSIS uninstaller hook).
    [Parameter(Mandatory, ParameterSetName = 'Environment')] [switch] $FromEnvironment,
    [string] $TimestampServer = ''
)
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

if ($PSCmdlet.ParameterSetName -eq 'Environment') {
    $SigningProfile = $env:SSDOWNLOAD_SIGNING_PROFILE
    if (-not $TimestampServer) { $TimestampServer = $env:SSDOWNLOAD_SIGNING_TIMESTAMP }
    if (-not $SigningProfile) { throw 'SSDOWNLOAD_SIGNING_PROFILE is not set' }
    if (-not $Dlib) { $Dlib = $env:SSDOWNLOAD_SIGNING_DLIB }
}
if ($PSCmdlet.ParameterSetName -ne 'Certificate') {
    if (-not (Test-Path -LiteralPath $SigningProfile -PathType Leaf)) { throw "Signing profile not found: $SigningProfile" }
    if (-not $Dlib -or -not (Test-Path -LiteralPath $Dlib -PathType Leaf)) {
        throw 'Azure.CodeSigning.Dlib.dll was not found; pass -Dlib or set SSDOWNLOAD_SIGNING_DLIB'
    }
    if (-not $TimestampServer) { $TimestampServer = 'http://timestamp.acs.microsoft.com' }
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $signtool = Get-ChildItem -LiteralPath $kits -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } | Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $signtool) { throw 'signtool.exe (Windows SDK, x64) was not found' }
    & $signtool.FullName sign /v /fd SHA256 /tr $TimestampServer /td SHA256 /dlib $Dlib /dmdf $SigningProfile $Path
    if ($LASTEXITCODE -ne 0) { throw "signtool failed with exit code $LASTEXITCODE" }
} else {
    if (-not $TimestampServer) { throw 'A timestamp server is required for certificate signing' }
    $Certificate = Get-Item -LiteralPath "Cert:\CurrentUser\My\$CertificateThumbprint"
    if (-not $Certificate.HasPrivateKey) { throw 'Code-signing certificate has no accessible private key' }
    $Result = Set-AuthenticodeSignature -LiteralPath $Path -Certificate $Certificate -HashAlgorithm SHA256 -TimestampServer $TimestampServer
    if ($Result.Status -ne 'Valid') { throw "Signing failed: $($Result.StatusMessage)" }
}
if ((Get-AuthenticodeSignature -LiteralPath $Path).Status -ne 'Valid') { throw 'Signature verification failed' }
