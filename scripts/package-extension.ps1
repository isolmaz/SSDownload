# Packages browser/chromium for the Chrome Web Store (also accepted by
# Microsoft Edge Add-ons).
#
# The store package is the extension folder without the manifest "key" (the
# stores reject it). Chrome and Edge install the store copy through the
# per-user registry entries SSDownload writes (src/extension.rs), so the store
# item must keep the ID the manifest key derives: hgndggnlfpnflkmnbddmcnfniamckham.
# The Chrome Web Store fixes an item's ID from a `key.pem` at the root of the
# FIRST upload only; build that one upload with -FirstUpload. That ZIP contains
# the private key: upload it to the store and never publish or commit it.
#
# The private key never enters the repository: -PrivateKeyPath (default
# %LOCALAPPDATA%\SSDownload\keys\extension.key) or SSDOWNLOAD_CRX_KEY. Its public
# half must match the manifest "key"; the script refuses to package otherwise.
#
# Usage:
#   powershell -File scripts\package-extension.ps1                  (artifacts/<version>/extension)
#   powershell -File scripts\package-extension.ps1 -FirstUpload     (adds key.pem for the first store upload)
#   powershell -File scripts\package-extension.ps1 -OutputDirectory <dir>
[CmdletBinding()]
param(
    [string]$OutputDirectory = '',   # artifacts/<version>/extension by default
    [string]$PrivateKeyPath = $(if ($env:SSDOWNLOAD_CRX_KEY) { $env:SSDOWNLOAD_CRX_KEY } else { Join-Path $env:LOCALAPPDATA 'SSDownload\keys\extension.key' }),
    [switch]$FirstUpload
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.toml') -Raw
$Version = [regex]::Match($CargoText, '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
if (-not $Version) { throw 'Cargo.toml version is missing' }

$ChromiumDir = Join-Path $ProjectRoot 'browser\chromium'
$ManifestPath = Join-Path $ChromiumDir 'manifest.json'
if (-not (Test-Path $ManifestPath -PathType Leaf)) { throw "Chromium manifest missing: $ManifestPath" }

if (-not $OutputDirectory) { $OutputDirectory = "artifacts/$Version/extension" }
$Dist = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $OutputDirectory))
$ZipName = if ($FirstUpload) { "SSDownload-$Version-store-first-upload.zip" } else { "SSDownload-$Version-store.zip" }
$ZipPath = Join-Path $Dist $ZipName
if (Test-Path -LiteralPath $ZipPath) { throw "Output already exists; choose a fresh -OutputDirectory: $ZipPath" }

# --- Identity: the signing key must be the one the manifest key came from ----
if (-not (Test-Path $PrivateKeyPath -PathType Leaf)) {
    throw "Extension key missing: $PrivateKeyPath. Restore the fixed key backup (or set SSDOWNLOAD_CRX_KEY): a new key would change the extension ID."
}
$staging = Join-Path ([System.IO.Path]::GetTempPath()) ("ssdownload-store-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $staging | Out-Null
try {
    $publicDerPath = Join-Path $staging 'public.der'
    & cmd /c "openssl pkey -in `"$PrivateKeyPath`" -pubout -outform DER -out `"$publicDerPath`" 2>NUL"
    if ($LASTEXITCODE -ne 0) { throw 'openssl could not export the public key' }
    $publicB64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($publicDerPath))

    $manifestText = Get-Content -LiteralPath $ManifestPath -Raw
    $manifest = $manifestText | ConvertFrom-Json
    if ($manifest.version -ne $Version) { throw "manifest.json version $($manifest.version) does not match Cargo.toml $Version" }
    # Base64 is case-sensitive: a mixed-case value is a different key.
    if ($manifest.key -cne $publicB64) { throw 'manifest.json key does not match the public half of the extension key' }
    if ($manifest.PSObject.Properties.Name -contains 'update_url') { throw 'manifest.json must not carry update_url; the store serves updates' }

    $sha = [System.Security.Cryptography.SHA256]::Create()
    $digest = [BitConverter]::ToString($sha.ComputeHash([Convert]::FromBase64String($publicB64))).Replace('-', '').ToLowerInvariant()
    $extensionId = -join ($digest.Substring(0, 32).ToCharArray() | ForEach-Object { [char]([byte][char]'a' + [Convert]::ToInt32([string]$_, 16)) })

    # Remove only the "key" line so the rest of the manifest stays byte-identical.
    $storeManifest = [regex]::Replace($manifestText, '(?m)^\s*"key"\s*:\s*"[^"]*",\r?\n', '')
    if ($storeManifest -match '"key"\s*:') { throw 'Could not strip the manifest key' }
    $null = $storeManifest | ConvertFrom-Json

    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    # ZipArchive with explicit forward-slash entry names: Compress-Archive on
    # Windows PowerShell 5.1 writes backslashes, which the store rejects.
    $zip = [System.IO.Compression.ZipFile]::Open($ZipPath, [System.IO.Compression.ZipArchiveMode]::Create)
    try {
        foreach ($file in Get-ChildItem -LiteralPath $ChromiumDir -Recurse -File) {
            $relative = $file.FullName.Substring($ChromiumDir.Length + 1).Replace('\', '/')
            if ($relative -eq 'manifest.json') {
                $entry = $zip.CreateEntry($relative, [System.IO.Compression.CompressionLevel]::Optimal)
                $writer = New-Object System.IO.StreamWriter($entry.Open(), [System.Text.UTF8Encoding]::new($false))
                try { $writer.Write($storeManifest) } finally { $writer.Dispose() }
            }
            else {
                [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $file.FullName, $relative, [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
            }
        }
        if ($FirstUpload) {
            $pemPath = Join-Path $staging 'key.pem'
            & cmd /c "openssl pkey -in `"$PrivateKeyPath`" -out `"$pemPath`" 2>NUL"
            if ($LASTEXITCODE -ne 0) { throw 'openssl could not write key.pem' }
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $pemPath, 'key.pem', [System.IO.Compression.CompressionLevel]::Optimal) | Out-Null
        }
    }
    finally {
        $zip.Dispose()
    }
}
finally {
    Remove-Item -LiteralPath $staging -Recurse -Force -ErrorAction SilentlyContinue
}

$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $ZipPath).Hash.ToLowerInvariant()
Write-Host "Extension ID: $extensionId"
Write-Host "Store package: $ZipPath"
Write-Host "SHA-256:       $hash"
if ($FirstUpload) {
    Write-Warning 'This ZIP contains the private extension key (key.pem). Use it only for the first Chrome Web Store upload, then delete it; never publish or commit it.'
}
