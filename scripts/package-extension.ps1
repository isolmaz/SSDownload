# Packages the Chromium extension as a CRX v3 file pinned to the repository's
# fixed extension identity. Firefox and other Chromium browsers are out of
# scope: native messaging registration already works wherever the browser is
# Chromium-compatible; no extra per-browser effort is spent here.
#
# Chromium requires a policy-installed CRX to come from the same origin as the
# update manifest. Both are assets of the project's GitHub release: update.xml is
# read from releases/latest/download and the codebase is pinned to the release tag
# (-CrxDownloadBaseUrl, releases/download/v<version> by default).
#
# CRX v3 is signed with a dedicated RSA private key. The key never enters the
# repository: pass its path with -PrivateKeyPath (the fixed key; there is no
# first-run generation, because a new key would change the extension ID) or set
# SSDOWNLOAD_CRX_KEY. The extension ID derives from the public key, which must
# match the "key" field in browser/chromium/manifest.json and the origin listed
# in the native messaging host manifest.
#
# Usage:
#   powershell -File scripts\package-extension.ps1                          (artifacts/<version>/extension)
#   powershell -File scripts\package-extension.ps1 -OutputDirectory <dir>
# Optional: -PrivateKeyPath <path> (default %LOCALAPPDATA%\SSDownload\keys\extension.key)
[CmdletBinding()]
param(
    [string]$OutputDirectory = '',   # artifacts/<version>/extension by default
    [string]$PrivateKeyPath = $(if ($env:SSDOWNLOAD_CRX_KEY) { $env:SSDOWNLOAD_CRX_KEY } else { Join-Path $env:LOCALAPPDATA 'SSDownload\keys\extension.key' }),
    [string]$UpdateXmlUrl = 'https://github.com/isolmaz/SSDownload/releases/latest/download/update.xml',
    [string]$CrxDownloadBaseUrl = ''   # https://github.com/isolmaz/SSDownload/releases/download/v<version> by default
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.toml') -Raw
$Version = [regex]::Match($CargoText, '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
if (-not $Version) { throw 'Cargo.toml version is missing' }
if (-not $CrxDownloadBaseUrl) { $CrxDownloadBaseUrl = "https://github.com/isolmaz/SSDownload/releases/download/v$Version" }

$ChromiumDir = Join-Path $ProjectRoot 'browser\chromium'
$ManifestPath = Join-Path $ChromiumDir 'manifest.json'
if (-not (Test-Path $ManifestPath -PathType Leaf)) { throw "Chromium manifest missing: $ManifestPath" }

if (-not $OutputDirectory) { $OutputDirectory = "artifacts/$Version/extension" }
$Dist = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $OutputDirectory))
Write-Host "Output directory: $Dist"
$CrxPath = Join-Path $Dist "SSDownload-$Version.crx"
$UpdateXmlPath = Join-Path $Dist 'update.xml'
$CrxHashPath = Join-Path $Dist 'SHA256SUMS'
foreach ($ExistingOutput in @($CrxPath, $UpdateXmlPath, $CrxHashPath)) {
    if (Test-Path -LiteralPath $ExistingOutput) { throw "Output already exists; choose a fresh -OutputDirectory: $ExistingOutput" }
}

# --- Identity: derive the CRX public key and extension ID --------------------
if (-not (Test-Path $PrivateKeyPath -PathType Leaf)) {
    throw "CRX signing key missing: $PrivateKeyPath. Restore the fixed key backup (or set SSDOWNLOAD_CRX_KEY): generating a new key would change the extension ID, invalidate the published manifest key and break the native-host allowed origin."
}
$publicDerPath = Join-Path ([System.IO.Path]::GetTempPath()) ("ssdownload-pub-" + [Guid]::NewGuid().ToString('N') + '.der')
$stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$quarantine = Join-Path $Dist "packaging-quarantine\$stamp"

# NO-DELETE policy: staging leftovers are never deleted. They move into a
# timestamped quarantine folder beside the output directory and the path is
# reported so the user can review and remove them manually.
function Move-IntoQuarantine {
    param([string]$Source, [string]$QuarantineRoot)
    if (-not (Test-Path -LiteralPath $Source)) { return $null }
    if (-not (Test-Path -LiteralPath $QuarantineRoot -PathType Container)) {
        New-Item -ItemType Directory -Path $QuarantineRoot -Force | Out-Null
    }
    $destination = Join-Path $QuarantineRoot (Split-Path $Source -Leaf)
    Move-Item -LiteralPath $Source -Destination $destination -Force
    return $destination
}

try {
    & cmd /c "openssl rsa -in `"$PrivateKeyPath`" -pubout -outform DER -out `"$publicDerPath`" 2>NUL"
    if ($LASTEXITCODE -ne 0) { throw 'openssl could not export the public key' }
    $publicDer = [System.IO.File]::ReadAllBytes($publicDerPath)
}
finally {
    Move-IntoQuarantine -Source $publicDerPath -QuarantineRoot $quarantine | Out-Null
}
$publicB64 = [Convert]::ToBase64String([byte[]]$publicDer)
$sha = [System.Security.Cryptography.SHA256]::Create()
$digest = [BitConverter]::ToString($sha.ComputeHash([byte[]]$publicDer)).Replace('-', '').ToLowerInvariant()
# Extension ID: the first 16 bytes of SHA-256(public DER), each hex digit mapped
# to [a-p] - exactly how Chromium derives it from the manifest "key".
$extensionId = -join ($digest.Substring(0, 32).ToCharArray() | ForEach-Object { [char]([byte][char]'a' + [Convert]::ToInt32([string]$_, 16)) })

$manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
if ($manifest.version -ne $Version) { throw "manifest.json version $($manifest.version) does not match Cargo.toml $Version" }
# The manifest key is Base64, so a mixed-case value is a different key: the
# identity comparison must be case-sensitive.
if ($manifest.key -cne $publicB64) { throw "manifest.json key does not match the signing key public half; expected $publicB64" }
# The update URL is a pinned contract whose path is case-sensitive, so the
# comparison is case-sensitive as well.
if ($manifest.update_url -cne $UpdateXmlUrl) { throw "manifest.json update_url does not match -UpdateXmlUrl $($manifest.update_url)" }

# --- CRX v3 -----------------------------------------------------------------
# Chrome itself produces the CRX v3 container (`--pack-extension`), which keeps
# the protobuf header, CRX_ID and AsymmetricKeyProof exactly canonical. The
# private key must live outside the repository; its public half must match the
# manifest "key" so the derived extension ID stays stable.
$staging = Join-Path ([System.IO.Path]::GetTempPath()) ("ssdownload-crx-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null
try {
    Copy-Item -LiteralPath $ChromiumDir -Destination (Join-Path $staging 'payload') -Recurse
    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    $chromeCandidates = @(
        (Join-Path $env:ProgramFiles 'Google\Chrome\Application\chrome.exe'),
        (Join-Path ${env:ProgramFiles(x86)} 'Google\Chrome\Application\chrome.exe'),
        (Join-Path $env:LOCALAPPDATA 'Google\Chrome\Application\chrome.exe')
    )
    $chrome = $chromeCandidates | Where-Object { Test-Path $_ -PathType Leaf } | Select-Object -First 1
    if (-not $chrome) { throw 'Chrome was not found; --pack-extension requires a Chrome installation.' }
    # Chrome writes <dir>.crx and <dir>.pem next to the payload directory.
    # This packaging command uses headless Chrome and a staging-owned profile; it does not
    # use the user profile or show a browser window. cmd /c avoids PowerShell treating
    # Chrome's stderr chatter as errors and keeps the = separated arguments intact.
    & cmd /c "`"$chrome`" --pack-extension=`"$(Join-Path $staging 'payload')`" --pack-extension-key=`"$PrivateKeyPath`" --user-data-dir=`"$(Join-Path $staging 'profile')`" --headless=new --no-startup-window --no-first-run --no-default-browser-check --noerrdialogs --no-sandbox 2>NUL"
    $packedCrx = (Join-Path $staging 'payload.crx')
    if (-not (Test-Path $packedCrx -PathType Leaf)) {
        throw "Chrome did not produce the expected CRX at $packedCrx"
    }
    Move-Item -LiteralPath $packedCrx -Destination $CrxPath -Force
}
finally {
    $quarantined = Move-IntoQuarantine -Source $staging -QuarantineRoot $quarantine
    if ($quarantined) { Write-Host "Paketleme kalıntıları taşındı: $quarantined" }
}

# --- Update manifest (GUpdate style, Edge-compatible) -----------------------
$crxHash = (Get-FileHash -Algorithm SHA256 -LiteralPath $CrxPath).Hash.ToLowerInvariant()
$updateXml = @"
<?xml version='1.0' encoding='UTF-8'?>
<gupdate xmlns='http://www.google.com/update2/response' protocol='2.0'>
  <app appid='$extensionId'>
    <updatecheck codebase='$CrxDownloadBaseUrl/SSDownload-$Version.crx' version='$Version' sha256='$crxHash' />
  </app>
</gupdate>
"@
[System.IO.File]::WriteAllText($UpdateXmlPath, $updateXml, [System.Text.UTF8Encoding]::new($false))

$hashLines = @(
    "{0}  {1}" -f (Get-FileHash -Algorithm SHA256 -LiteralPath $CrxPath).Hash.ToLowerInvariant(), (Split-Path $CrxPath -Leaf)
)
[System.IO.File]::WriteAllText($CrxHashPath, (($hashLines -join "`n") + "`n"), [System.Text.Encoding]::ASCII)

Write-Host "Extension ID: $extensionId"
Write-Host "Created extension artifacts:"
Write-Host "  $CrxPath"
Write-Host "  $UpdateXmlPath"
Write-Host "  $CrxHashPath"
