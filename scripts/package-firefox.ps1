# Packages browser/firefox as a Firefox add-on (XPI) for local and temporary
# testing and for AMO submission.
#
# The produced XPI is UNSIGNED. Release Firefox refuses to install an unsigned
# add-on permanently; load it through about:debugging ("Load Temporary
# Add-on") for testing, or upload the same file to https://addons.mozilla.org
# and use the signed XPI Mozilla returns. The archive is a plain ZIP whose root
# holds manifest.json, which is what AMO and web-ext expect.
#
# Usage:
#   powershell -File scripts\package-firefox.ps1                  (artifacts/<version>/extension)
#   powershell -File scripts\package-firefox.ps1 -OutputDirectory <dir>
[CmdletBinding()]
param(
    [string]$OutputDirectory = ''   # artifacts/<version>/extension by default
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression
Add-Type -AssemblyName System.IO.Compression.FileSystem

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.toml') -Raw
$Version = [regex]::Match($CargoText, '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
if (-not $Version) { throw 'Cargo.toml version is missing' }

$FirefoxDir = Join-Path $ProjectRoot 'browser\firefox'
$ManifestPath = Join-Path $FirefoxDir 'manifest.json'
if (-not (Test-Path $ManifestPath -PathType Leaf)) {
    throw "Firefox manifest missing: $ManifestPath (browser/firefox must be a directly loadable add-on)"
}

$Manifest = Get-Content -LiteralPath $ManifestPath -Raw | ConvertFrom-Json
if (-not $Manifest.version) { throw "manifest.json has no version: $ManifestPath" }
if ($Manifest.version -ne $Version) {
    throw "manifest.json version $($Manifest.version) does not match Cargo.toml $Version"
}

if (-not $OutputDirectory) { $OutputDirectory = "artifacts/$Version/extension" }
$Dist = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $OutputDirectory))
$XpiPath = Join-Path $Dist "SSDownload-$Version-firefox.xpi"
if (Test-Path -LiteralPath $XpiPath) { throw "Output already exists; choose a fresh -OutputDirectory: $XpiPath" }

New-Item -ItemType Directory -Force -Path $Dist | Out-Null
# ZipArchive with explicit forward-slash entry names: Compress-Archive on
# Windows PowerShell 5.1 writes backslashes, which add-on tooling rejects.
$stream = [System.IO.File]::Open($XpiPath, [System.IO.FileMode]::CreateNew)
try {
    $zip = [System.IO.Compression.ZipArchive]::new(
        $stream,
        [System.IO.Compression.ZipArchiveMode]::Create,
        $false
    )
    try {
        foreach ($file in Get-ChildItem -LiteralPath $FirefoxDir -Recurse -File | Sort-Object FullName) {
            $relative = $file.FullName.Substring($FirefoxDir.Length + 1).Replace('\', '/')
            [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile(
                $zip,
                $file.FullName,
                $relative,
                [System.IO.Compression.CompressionLevel]::Optimal
            ) | Out-Null
        }
    }
    finally { $zip.Dispose() }
}
catch {
    $stream.Dispose()
    Remove-Item -LiteralPath $XpiPath -Force -ErrorAction SilentlyContinue
    throw
}
finally { $stream.Dispose() }

$hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $XpiPath).Hash.ToLowerInvariant()
Write-Host "Unsigned Firefox package (permanent install requires AMO signing):"
Write-Host "  $XpiPath"
Write-Host "  SHA-256: $hash"
