# Collects the final release artifact set into one folder, generates and signs the
# update feed and either prints the publication command or creates the GitHub
# release of this repository (`isolmaz/SSDownload`). The release assets are the
# only distribution channel: the desktop application reads
# `releases/latest/download/version.json` and Chrome reads
# `releases/latest/download/update.xml`.
#
# Expected inputs (produced by scripts/package.ps1 and
# scripts/package-extension.ps1):
#   <PackageDir>\SSDownload-<version>-Setup.exe
#   <PackageDir>\SSDownload-<version>-win64.zip
#   <PackageDir>\SHA256SUMS
#   <ExtensionDir>\SSDownload-<version>.crx
#   <ExtensionDir>\update.xml
#   <ExtensionDir>\SHA256SUMS
#
# Usage:
#   powershell -File scripts\prepare-release.ps1 -PackageDir artifacts/<v>/app -ExtensionDir artifacts/<v>/extension -Notes "<text>"
#   powershell -File scripts\prepare-release.ps1 ... -Publish
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$PackageDir,
    [Parameter(Mandatory = $true)][string]$ExtensionDir,
    [string]$OutputDirectory = '',   # artifacts/<version>/release by default
    [string]$Notes = '',
    [string]$Repository = 'isolmaz/SSDownload',
    [string]$SigningKeyPath = $(if ($env:SSDOWNLOAD_UPDATE_KEY) { $env:SSDOWNLOAD_UPDATE_KEY } else { Join-Path $env:LOCALAPPDATA 'SSDownload\keys\update-signing.key' }),
    [switch]$Publish
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.toml') -Raw
$Version = [regex]::Match($CargoText, '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
if (-not $Version) { throw 'Cargo.toml version is missing' }

if (-not $OutputDirectory) { $OutputDirectory = "artifacts/$Version/release" }
$PackageRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $PackageDir))
$ExtensionRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $ExtensionDir))
$SetupName = "SSDownload-$Version-Setup.exe"
$ZipName = "SSDownload-$Version-win64.zip"
$CrxName = "SSDownload-$Version.crx"
$Inputs = @(
    @{ Source = (Join-Path $PackageRoot $SetupName); Name = $SetupName },
    @{ Source = (Join-Path $PackageRoot $ZipName); Name = $ZipName },
    @{ Source = (Join-Path $ExtensionRoot $CrxName); Name = $CrxName },
    @{ Source = (Join-Path $ExtensionRoot 'update.xml'); Name = 'update.xml' }
)
foreach ($input in $Inputs) {
    if (-not (Test-Path -LiteralPath $input.Source -PathType Leaf)) {
        throw "Missing release input: $($input.Source)"
    }
}

# Fail before anything is written or published: a feed signed by a key the
# application does not embed is rejected by every shipped build, so the signing
# key must match FEED_PUBLIC_KEY before an output folder exists at all.
if (-not (Test-Path -LiteralPath $SigningKeyPath -PathType Leaf)) {
    throw "Update signing key missing: $SigningKeyPath (create it with: openssl genpkey -algorithm ED25519 -out <path>)"
}
$preflightDer = Join-Path ([System.IO.Path]::GetTempPath()) ("ssdownload-feed-key-" + [Guid]::NewGuid().ToString('N') + '.der')
try {
    & openssl pkey -in $SigningKeyPath -pubout -outform DER -out $preflightDer 2>$null
    if ($LASTEXITCODE -ne 0) { throw 'Exporting the update signing public key failed' }
    $preflightBytes = [IO.File]::ReadAllBytes($preflightDer)
    $feedPublicKey = [Convert]::ToBase64String($preflightBytes[($preflightBytes.Length - 32)..($preflightBytes.Length - 1)])
}
finally {
    Remove-Item -LiteralPath $preflightDer -Force -ErrorAction SilentlyContinue
}
$updateSource = Get-Content -LiteralPath (Join-Path $ProjectRoot 'src\update.rs') -Raw
$expectedKey = [regex]::Match($updateSource, '(?m)^const FEED_PUBLIC_KEY: &str = "([^"]+)";').Groups[1].Value
if (-not $expectedKey) { throw 'FEED_PUBLIC_KEY is missing from src/update.rs' }
# Base64 is case-sensitive, so a mixed-case literal decodes to a different key:
# the comparison must be case-sensitive or the release would be signed for a key
# the shipped application cannot derive.
if ($expectedKey -cne $feedPublicKey) {
    throw "The update signing key does not match FEED_PUBLIC_KEY in src/update.rs; the published feed could not be verified by the application."
}

$Dist = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $OutputDirectory))
if (Test-Path -LiteralPath $Dist) {
    # NO-DELETE policy: a stale release folder moves into a timestamped
    # quarantine folder instead of being deleted.
    $stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
    $quarantineRoot = Join-Path $ProjectRoot ".qa\release-quarantine\$stamp"
    New-Item -ItemType Directory -Path $quarantineRoot -Force | Out-Null
    Move-Item -LiteralPath $Dist -Destination (Join-Path $quarantineRoot (Split-Path $Dist -Leaf)) -Force
    Write-Warning "Previous release folder moved to $quarantineRoot"
}
New-Item -ItemType Directory -Path $Dist | Out-Null
foreach ($input in $Inputs) {
    Copy-Item -LiteralPath $input.Source -Destination (Join-Path $Dist $input.Name)
}

# Update feed: the app checks <feed>/version.json for version/notes. The app
# reads the newest release's copy of the feed, while the Setup URL must stay
# resolvable after a later release becomes "latest", so the two bases differ:
# the feed is read from /latest/download and the asset is pinned to its tag.
$feedBase = "https://github.com/$Repository/releases/latest/download"
$releaseBase = "https://github.com/$Repository/releases/download/v$Version"
$setupHash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Dist $SetupName)).Hash.ToLowerInvariant()
$feed = [ordered]@{
    version = $Version
    url     = "$releaseBase/$SetupName"
    sha256  = $setupHash
    notes   = $Notes
}
$feedPath = Join-Path $Dist 'version.json'
$feedJson = $feed | ConvertTo-Json -Depth 3
[System.IO.File]::WriteAllText($feedPath, $feedJson + "`n", [System.Text.UTF8Encoding]::new($false))

# Feed integrity: Ed25519 signature over the exact version.json bytes. The
# application verifies it against the public key embedded in src/update.rs, so a
# compromised feed host cannot substitute a different Setup. The key itself was
# already proven to match FEED_PUBLIC_KEY before this folder existed.
$signaturePath = Join-Path $Dist 'version.json.sig'
& openssl pkeyutl -sign -rawin -inkey $SigningKeyPath -in $feedPath -out $signaturePath
if ($LASTEXITCODE -ne 0) { throw 'Signing version.json failed' }
$signatureBase64 = [Convert]::ToBase64String([IO.File]::ReadAllBytes($signaturePath))
[System.IO.File]::WriteAllText($signaturePath, $signatureBase64, [System.Text.Encoding]::ASCII)
Write-Host "version.json signed (Ed25519). Public key: $feedPublicKey (matches FEED_PUBLIC_KEY)"

# Validate every checksum-manifest row against the bytes it names, then build the
# public list from the artifacts this run actually publishes. The package
# manifest covers the Setup, the win64 ZIP and the local-only symbols ZIP; the
# extension manifest covers the CRX. A mismatch aborts the release instead of
# being silently rewritten, because this folder is what users verify against.
# A row must be a bare file name: a duplicate row, a missing required row or a
# path that escapes the staging folder is rejected rather than parsed loosely.
function Assert-ManifestBytes {
    param([string]$ManifestPath, [string[]]$RequiredNames, [string]$Directory)
    $seen = @{}
    foreach ($line in Get-Content -LiteralPath $ManifestPath) {
        if (-not $line.Trim()) { continue }
        $parts = $line -split '\s+', 2
        if ($parts.Count -ne 2 -or -not $parts[1].Trim()) { throw "Malformed checksum row in ${ManifestPath}: $line" }
        $expected = $parts[0].ToLowerInvariant()
        $name = $parts[1].Trim()
        if ($expected -notmatch '^[0-9a-f]{64}$') { throw "Malformed checksum value in ${ManifestPath}: $expected" }
        if ($name -ne [System.IO.Path]::GetFileName($name) -or [System.IO.Path]::IsPathRooted($name) -or $name -in @('.', '..')) {
            throw "Checksum row does not name a plain artifact file in ${ManifestPath}: $name"
        }
        if ($seen.ContainsKey($name)) { throw "Duplicate checksum row for $name in $ManifestPath" }
        $target = Join-Path $Directory $name
        if (-not (Test-Path -LiteralPath $target -PathType Leaf)) { throw "Checksum row names a missing artifact: $name ($ManifestPath)" }
        $seen[$name] = $true
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash.ToLowerInvariant()
        if ($actual -ne $expected) { throw "Checksum mismatch for $name in $ManifestPath" }
    }
    foreach ($required in $RequiredNames) {
        if (-not $seen.ContainsKey($required)) { throw "Checksum manifest $ManifestPath is missing a row for $required" }
    }
}
Assert-ManifestBytes -ManifestPath (Join-Path $PackageRoot 'SHA256SUMS') -Directory $PackageRoot `
    -RequiredNames @($SetupName, $ZipName, "SSDownload-$Version-symbols.zip")
Assert-ManifestBytes -ManifestPath (Join-Path $ExtensionRoot 'SHA256SUMS') -Directory $ExtensionRoot `
    -RequiredNames @($CrxName)

# The update manifest must describe this exact extension package: a version, an
# extension id, a codebase and a CRX hash that belong to different builds must
# not be assembled into one release.
$extensionManifest = Get-Content -LiteralPath (Join-Path $ProjectRoot 'browser\chromium\manifest.json') -Raw | ConvertFrom-Json
if ($extensionManifest.version -ne $Version) {
    throw "browser/chromium/manifest.json reports version $($extensionManifest.version) instead of $Version; update it and repackage the extension."
}
if (-not $extensionManifest.key) { throw 'browser/chromium/manifest.json has no key; the extension identity cannot be derived.' }
if ($extensionManifest.update_url -cne "$feedBase/update.xml") {
    throw "browser/chromium/manifest.json update_url is not $feedBase/update.xml"
}
$manifestDer = [Convert]::FromBase64String($extensionManifest.key)
$manifestDigest = [BitConverter]::ToString(([System.Security.Cryptography.SHA256]::Create()).ComputeHash($manifestDer)).Replace('-', '').ToLowerInvariant()
$manifestExtensionId = -join ($manifestDigest.Substring(0, 32).ToCharArray() | ForEach-Object { [char]([byte][char]'a' + [Convert]::ToInt32([string]$_, 16)) })
$crxHash = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Dist $CrxName)).Hash.ToLowerInvariant()
[xml]$updateXml = Get-Content -LiteralPath (Join-Path $Dist 'update.xml') -Raw
$updateApp = $updateXml.gupdate.app
if (-not $updateApp) { throw 'update.xml has no gupdate/app element' }
if ($updateApp.appid -ne $manifestExtensionId) {
    throw "update.xml targets extension id $($updateApp.appid) but manifest.json derives $manifestExtensionId"
}
$updateCheck = $updateApp.updatecheck
if (-not $updateCheck) { throw 'update.xml has no updatecheck element' }
if ($updateCheck.version -ne $Version) { throw "update.xml reports version $($updateCheck.version) instead of $Version" }
# Chrome fetches a policy-installed CRX only from the update manifest's own origin.
# update.xml and the CRX are assets of the same release, so the codebase must be
# exactly this release's asset URL; a same-basename URL on another host would
# otherwise be assembled into the release unnoticed.
$expectedCodebase = "$releaseBase/$CrxName"
if ($updateCheck.codebase -cne $expectedCodebase) {
    throw "update.xml codebase $($updateCheck.codebase) is not the release asset $expectedCodebase"
}
if ($updateCheck.sha256 -ne $crxHash) { throw 'update.xml sha256 does not match the CRX in this release' }

# The public list covers exactly the published artifacts, so every row resolves
# to an asset of this release. The symbols ZIP stays local to the package folder.
$publishedNames = @($SetupName, $ZipName, $CrxName, 'update.xml', 'version.json', 'version.json.sig')
$checksumRows = foreach ($name in $publishedNames) {
    $target = Join-Path $Dist $name
    if (-not (Test-Path -LiteralPath $target -PathType Leaf)) { throw "Missing published artifact: $target" }
    "{0}  {1}" -f (Get-FileHash -Algorithm SHA256 -LiteralPath $target).Hash.ToLowerInvariant(), $name
}
[System.IO.File]::WriteAllText((Join-Path $Dist 'SHA256SUMS'), (($checksumRows -join "`n") + "`n"), [System.Text.Encoding]::ASCII)

# Release notes: the maintainer's text followed by the checksums of the
# downloads people run.
$notesPath = Join-Path $Dist 'notes.md'
$body = if ($Notes) { $Notes } else { "SSDownload $Version" }
$body += "`n`nSHA-256:`n"
foreach ($name in @($SetupName, $ZipName, $CrxName)) {
    $body += "- ``{0}``: ``{1}```n" -f $name, (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $Dist $name)).Hash.ToLowerInvariant()
}
[System.IO.File]::WriteAllText($notesPath, $body, [System.Text.UTF8Encoding]::new($false))

Write-Host ''
Write-Host 'Release folder ready:'
Get-ChildItem -LiteralPath $Dist | ForEach-Object { Write-Host ("  {0}  ({1} bytes)" -f $_.Name, $_.Length) }

$releaseTag = "v$Version"
$uploadNames = @($SetupName, $ZipName, $CrxName, 'update.xml', 'version.json', 'version.json.sig', 'SHA256SUMS')
$assetPaths = $uploadNames | ForEach-Object { Join-Path $Dist $_ }

if (-not $Publish) {
    Write-Host ''
    Write-Host 'Verify gh authentication and push the release commit, then publish with:'
    $assetList = $uploadNames | ForEach-Object { "$OutputDirectory/$_" }
    Write-Host ("  gh release create {0} --repo {1} --title `"SSDownload {2}`" --notes-file {3} {4}" -f $releaseTag, $Repository, $Version, $notesPath, ($assetList -join ' '))
    Write-Host '  (or rerun this script with -Publish)'
    exit 0
}

# --- Publication -------------------------------------------------------------
# Native stderr becomes a terminating error under $ErrorActionPreference='Stop',
# so the "does the release exist" probe runs with errors suppressed.
$probePreference = $ErrorActionPreference
$ErrorActionPreference = 'SilentlyContinue'
$existingRelease = & gh release view $releaseTag --repo $Repository --json tagName --jq '.tagName' 2>$null
$probeExit = $LASTEXITCODE
$ErrorActionPreference = $probePreference
if ($probeExit -eq 0 -and $existingRelease) {
    Write-Host "Release $releaseTag exists; uploading the current artifacts"
    & gh release upload $releaseTag --repo $Repository --clobber @assetPaths
    if ($LASTEXITCODE -ne 0) { throw 'gh release upload failed' }
}
else {
    & gh release create $releaseTag --repo $Repository --title "SSDownload $Version" --notes-file $notesPath @assetPaths
    if ($LASTEXITCODE -ne 0) { throw 'gh release create failed' }
}

Write-Host ''
Write-Host 'Published:'
Write-Host "  feed:      $feedBase/version.json"
Write-Host "  gupdate:   $feedBase/update.xml"
Write-Host "  release:   https://github.com/$Repository/releases/tag/$releaseTag"
