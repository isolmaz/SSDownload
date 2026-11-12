# Writes the Scoop and winget manifests of a release from its published artifacts.
#
# Run it after scripts\prepare-release.ps1: it reads the release folder's SHA256SUMS, so the
# manifests name exactly the Setup and portable ZIP of that release. Output:
#   <ReleaseDir>\scoop\ssdownload.json        bucket/ssdownload.json of isolmaz/scoop-bucket
#   <ReleaseDir>\winget\isolmaz.SSDownload*.yaml   manifests/i/isolmaz/SSDownload/<version>/ of
#                                                 microsoft/winget-pkgs
# Both point at the release's GitHub assets, so publish the release before submitting them.
#
# Usage:
#   powershell -File scripts\package-managers.ps1                       (artifacts/<version>/release)
#   powershell -File scripts\package-managers.ps1 -ReleaseDir <dir>
[CmdletBinding()]
param(
    [string]$ReleaseDir = '',
    [string]$Repository = 'isolmaz/SSDownload'
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.toml') -Raw
$Version = [regex]::Match($CargoText, '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
if (-not $Version) { throw 'Cargo.toml version is missing' }
$Description = [regex]::Match($CargoText, '(?m)^description = "([^"]+)"').Groups[1].Value
$License = [regex]::Match($CargoText, '(?m)^license = "([^"]+)"').Groups[1].Value
if (-not $Description -or -not $License) { throw 'Cargo.toml description or license is missing' }

if (-not $ReleaseDir) { $ReleaseDir = "artifacts/$Version/release" }
$Dist = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $ReleaseDir))
$SetupName = "SSDownload-$Version-Setup.exe"
$ZipName = "SSDownload-$Version-win64.zip"

# The hashes come from the release's own checksum list and are checked against the bytes, so a
# manifest can never name a file other than the one published.
$sums = @{}
foreach ($line in Get-Content -LiteralPath (Join-Path $Dist 'SHA256SUMS')) {
    $parts = $line -split '\s+', 2
    if ($parts.Count -eq 2) { $sums[$parts[1].Trim()] = $parts[0].ToLowerInvariant() }
}
foreach ($name in @($SetupName, $ZipName)) {
    if (-not $sums.ContainsKey($name)) { throw "SHA256SUMS has no row for $name" }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Dist $name)).Hash.ToLowerInvariant()
    if ($actual -ne $sums[$name]) { throw "Checksum mismatch for $name" }
}

$ReleaseBase = "https://github.com/$Repository/releases/download/v$Version"
$RepoUrl = "https://github.com/$Repository"
$Homepage = 'https://ssdownload.isolmaz.com'
$Utf8 = [System.Text.UTF8Encoding]::new($false)
function Write-Text([string]$Path, [string]$Text) {
    New-Item -ItemType Directory -Force -Path (Split-Path $Path) | Out-Null
    [System.IO.File]::WriteAllText($Path, ($Text -replace "`r`n", "`n"), $Utf8)
}

# Scoop runs the portable copy. The native host is registered for the copy's own path, which
# Scoop's post_install names through the `current` junction; the app resolves it to the version
# folder, so every update registers the new folder and pre_uninstall removes the old one. The
# console CLI is called because PowerShell does not wait for the GUI executable.
# Windows PowerShell's ConvertTo-Json escapes & and ' and misaligns nesting, so the manifest is
# written as text and parsed back to prove it is valid JSON.
$scoopPath = Join-Path $Dist 'scoop\ssdownload.json'
Write-Text $scoopPath @"
{
    "version": "$Version",
    "description": "$Description",
    "homepage": "$Homepage",
    "license": "$License",
    "architecture": {
        "64bit": {
            "url": "$ReleaseBase/$ZipName",
            "hash": "$($sums[$ZipName])",
            "extract_dir": "SSDownload-$Version-win64"
        }
    },
    "bin": [["ssdownload-cli.exe", "ssdownload"]],
    "shortcuts": [["ssdownload.exe", "SSDownload"]],
    "post_install": [
        "& \"`$dir\\ssdownload-cli.exe\" --register-host",
        "if (`$LASTEXITCODE -ne 0) { warn 'The SSDownload browser integration could not be registered; run: ssdownload --register-host' }"
    ],
    "pre_uninstall": [
        "& \"`$dir\\ssdownload-cli.exe\" --unregister-host",
        "if (`$LASTEXITCODE -ne 0) { warn 'The SSDownload browser integration could not be removed; run: ssdownload --unregister-host' }"
    ],
    "checkver": {
        "github": "$RepoUrl"
    },
    "autoupdate": {
        "architecture": {
            "64bit": {
                "url": "$RepoUrl/releases/download/v`$version/SSDownload-`$version-win64.zip",
                "extract_dir": "SSDownload-`$version-win64",
                "hash": {
                    "url": "`$baseurl/SHA256SUMS"
                }
            }
        }
    }
}

"@
Get-Content -LiteralPath $scoopPath -Raw | ConvertFrom-Json | Out-Null

# winget: the Setup is a per-user NSIS installer (`/S` silent, `/D=` folder), registered in
# HKCU\...\Uninstall\SSDownload, which is the ProductCode winget matches installed copies by.
$id = 'isolmaz.SSDownload'
$schema = '1.10.0'
$header = { param([string]$Type) "# yaml-language-server: `$schema=https://aka.ms/winget-manifest.$Type.$schema.schema.json`n" }
$wingetDir = Join-Path $Dist 'winget'
Write-Text (Join-Path $wingetDir "$id.yaml") ((& $header 'version') + @"

PackageIdentifier: $id
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $schema
"@)
Write-Text (Join-Path $wingetDir "$id.installer.yaml") ((& $header 'installer') + @"

PackageIdentifier: $id
PackageVersion: $Version
InstallerType: nullsoft
Scope: user
InstallModes:
- interactive
- silent
- silentWithProgress
UpgradeBehavior: install
ProductCode: SSDownload
ReleaseDate: $(Get-Date -Format 'yyyy-MM-dd')
AppsAndFeaturesEntries:
- DisplayName: SSDownload
  Publisher: SSDownload Project
  DisplayVersion: $Version
  ProductCode: SSDownload
Installers:
- Architecture: x64
  InstallerUrl: $ReleaseBase/$SetupName
  InstallerSha256: $($sums[$SetupName].ToUpperInvariant())
ManifestType: installer
ManifestVersion: $schema
"@)
Write-Text (Join-Path $wingetDir "$id.locale.en-US.yaml") ((& $header 'defaultLocale') + @"

PackageIdentifier: $id
PackageVersion: $Version
PackageLocale: en-US
Publisher: SSDownload Project
PublisherUrl: https://github.com/isolmaz
PublisherSupportUrl: $RepoUrl/issues
PrivacyUrl: $RepoUrl/blob/main/PRIVACY.md
PackageName: SSDownload
PackageUrl: $Homepage
License: $License
LicenseUrl: $RepoUrl/blob/main/LICENSE.txt
ShortDescription: $Description
Moniker: ssdownload
Tags:
- download-manager
- downloader
- video-downloader
ReleaseNotesUrl: $RepoUrl/releases/tag/v$Version
ManifestType: defaultLocale
ManifestVersion: $schema
"@)

Write-Host 'Package manager manifests:'
Write-Host "  Scoop:  $scoopPath"
Get-ChildItem -LiteralPath $wingetDir | ForEach-Object { Write-Host "  winget: $($_.FullName)" }
