[CmdletBinding()]
param(
    [ValidatePattern('^(\d+\.\d+\.\d+)?$')]
    [string] $Version = '',
    [switch] $Offline,
    [string] $OutputDirectory = '',   # artifacts/<version>/app by default
    [string] $Toolchain = "",
    [string] $CertificateThumbprint = "",
    [string] $TimestampServer = "",
    [string] $SigningProfile = "",    # Azure Artifact Signing metadata.json (signtool /dlib)
    [string] $Makensis = "$env:LOCALAPPDATA\Programs\nsis-3.10\makensis.exe"
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$CargoText = Get-Content -LiteralPath (Join-Path $ProjectRoot 'Cargo.toml') -Raw
$CargoVersion = [regex]::Match($CargoText, '(?m)^version = "(\d+\.\d+\.\d+)"').Groups[1].Value
if (-not $CargoVersion) { throw 'Cargo.toml version is missing' }
if ($Version -and $Version -ne $CargoVersion) { throw 'Version must match Cargo.toml' }
$Version = $CargoVersion
$ExpectedRust = [regex]::Match((Get-Content (Join-Path $ProjectRoot 'rust-toolchain.toml') -Raw), 'channel = "([^"]+)"').Groups[1].Value
$CompilerArgs = @(); if ($Toolchain) { $CompilerArgs += "+$Toolchain" }
$CompilerVersion = & rustc @CompilerArgs --version
if ($CompilerVersion -notmatch "^rustc $([regex]::Escape($ExpectedRust)) ") { throw "Compiler must be Rust $ExpectedRust" }
if ($CertificateThumbprint -and $SigningProfile) { throw 'Use either -CertificateThumbprint or -SigningProfile' }
if ($SigningProfile) { $SigningProfile = (Resolve-Path -LiteralPath $SigningProfile).Path }
if ($SigningProfile -match '[%$`"'']') { throw 'Signing profile path contains unsupported command characters' }
$Signing = [bool]($CertificateThumbprint -or $SigningProfile)
$SignArgs = if ($SigningProfile) {
    # NSIS signs the uninstaller in a child PowerShell; the profile reaches it through
    # the environment instead of a quoted path inside the NSIS command string.
    $env:SSDOWNLOAD_SIGNING_PROFILE = $SigningProfile
    $env:SSDOWNLOAD_SIGNING_TIMESTAMP = $TimestampServer
    @{ FromEnvironment = $true }
} else {
    @{ CertificateThumbprint = $CertificateThumbprint; TimestampServer = $TimestampServer }
}
if ($CertificateThumbprint -and -not $TimestampServer) { throw 'A timestamp server is required for signing' }
if ($CertificateThumbprint -and $CertificateThumbprint -notmatch '^[A-Fa-f0-9]{40}$') { throw 'Invalid certificate thumbprint' }
if ($CertificateThumbprint -and $TimestampServer -notmatch '^https?://[A-Za-z0-9][A-Za-z0-9./:_-]*$') { throw 'Use a plain HTTP(S) timestamp endpoint without shell characters' }
$Target = 'x86_64-pc-windows-msvc'
$TargetRoot = if ($env:CARGO_TARGET_DIR) { [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $env:CARGO_TARGET_DIR)) } else { Join-Path $ProjectRoot 'target' }
if (-not $OutputDirectory) { $OutputDirectory = "artifacts/$Version/app" }
$Dist = [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($ProjectRoot, $OutputDirectory))
Write-Host "Output directory: $Dist"
$PackageName = "SSDownload-$Version-win64"
$PackageRoot = Join-Path $Dist $PackageName
$ZipPath = Join-Path $Dist "$PackageName.zip"
$SetupPath = Join-Path $Dist "SSDownload-$Version-Setup.exe"
$ChecksumPath = Join-Path $Dist 'SHA256SUMS'
$SymbolsPath = Join-Path $Dist "SSDownload-$Version-symbols.zip"
foreach ($ExistingOutput in @($PackageRoot, $ZipPath, $SetupPath, $ChecksumPath, $SymbolsPath)) {
    if (Test-Path -LiteralPath $ExistingOutput) { throw "Output already exists; choose a fresh -OutputDirectory: $ExistingOutput" }
}
$ExePath = Join-Path $TargetRoot "$Target\release\ssdownload.exe"
$CliPath = Join-Path $TargetRoot "$Target\release\ssdownload-cli.exe"
$BrowserPath = Join-Path $ProjectRoot 'browser'

if (-not (Test-Path (Join-Path $BrowserPath 'chromium\manifest.json') -PathType Leaf)) {
    throw 'browser/chromium must contain a directly loadable Chrome extension manifest before packaging.'
}
if (-not (Test-Path $Makensis -PathType Leaf)) {
    throw "NSIS compiler was not found: $Makensis"
}

$NsisVersion = & $Makensis /VERSION
if ($NsisVersion.Trim() -ne 'v3.10') { throw 'NSIS 3.10 is required' }
Push-Location $ProjectRoot
try {
    # Building happens only when a maintainer explicitly invokes this release script.
    $BuildArgs = @('build', '--release', '--locked', '--target', $Target)
    if ($Offline) { $BuildArgs += '--offline' }
    # Published binaries must not carry this machine's paths (user name, checkout
    # location) in panic locations or in the PDB reference. These flags replace the
    # target rustflags of .cargo/config.toml for this build, so they repeat them.
    $CargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $RustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
    $ReleaseRustFlags = @(
        '-C', 'target-feature=+crt-static',
        '-C', 'link-arg=/PDBALTPATH:%_PDB%',
        "--remap-path-prefix=$([System.IO.Path]::GetFullPath($CargoHome))=/cargo",
        "--remap-path-prefix=$([System.IO.Path]::GetFullPath($RustupHome))=/rustup",
        "--remap-path-prefix=$ProjectRoot=/ssdownload"
    )
    $env:CARGO_ENCODED_RUSTFLAGS = $ReleaseRustFlags -join [char]0x1f
    try {
        & cargo @CompilerArgs @BuildArgs
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
    }
    finally {
        Remove-Item Env:CARGO_ENCODED_RUSTFLAGS -ErrorAction SilentlyContinue
    }

    if (-not (Test-Path $ExePath -PathType Leaf) -or -not (Test-Path $CliPath -PathType Leaf)) {
        throw "Release executable was not produced: $ExePath"
    }

    New-Item -ItemType Directory -Force -Path $Dist | Out-Null
    New-Item -ItemType Directory -Path $PackageRoot | Out-Null

    Copy-Item -LiteralPath $ExePath -Destination (Join-Path $PackageRoot 'ssdownload.exe')
    Copy-Item -LiteralPath $CliPath -Destination (Join-Path $PackageRoot 'ssdownload-cli.exe')
    # Only the loadable extension ships; browser/tests stays in the repository.
    New-Item -ItemType Directory -Path (Join-Path $PackageRoot 'browser') | Out-Null
    Copy-Item -LiteralPath (Join-Path $BrowserPath 'chromium') -Destination (Join-Path $PackageRoot 'browser\chromium') -Recurse
    foreach ($Doc in @('README.md', 'PRIVACY.md')) {
        Copy-Item -LiteralPath (Join-Path $ProjectRoot $Doc) -Destination $PackageRoot
    }
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'LICENSE.txt') -Destination $PackageRoot
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'THIRD-PARTY-NOTICES.txt') -Destination $PackageRoot
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'assets\app.ico') -Destination $PackageRoot
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'packaging\register-native-host.cmd') -Destination $PackageRoot
    Copy-Item -LiteralPath (Join-Path $ProjectRoot 'packaging\unregister-native-host.cmd') -Destination $PackageRoot

    $InventoryArgs = @((Join-Path $PSScriptRoot 'release-inventory.py'), '--output', $PackageRoot, '--nsis', $Makensis)
    if ($Toolchain) { $InventoryArgs += @('--toolchain', $Toolchain) }
    if ($Offline) { $InventoryArgs += '--offline' }
    & python @InventoryArgs
    if ($LASTEXITCODE) { throw 'License inventory generation failed' }
    if ($Signing) {
        & (Join-Path $PSScriptRoot 'sign.ps1') -Path (Join-Path $PackageRoot 'ssdownload.exe') @SignArgs
        & (Join-Path $PSScriptRoot 'sign.ps1') -Path (Join-Path $PackageRoot 'ssdownload-cli.exe') @SignArgs
    }
    $Pdb = Join-Path $TargetRoot "$Target\release\ssdownload.pdb"
    if (-not (Test-Path -LiteralPath $Pdb)) { throw 'Release PDB was not generated' }
    $CliPdb = Join-Path $TargetRoot "$Target\release\ssdownload_cli.pdb"
    if (-not (Test-Path -LiteralPath $CliPdb)) { throw 'CLI release PDB was not generated' }
    Compress-Archive -LiteralPath @($Pdb, $CliPdb) -DestinationPath $SymbolsPath
    # Produce a deterministic ZIP: sorted entry names and a fixed legal ZIP timestamp.
    Add-Type -AssemblyName System.IO.Compression
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $stream = [System.IO.File]::Open($ZipPath, [System.IO.FileMode]::CreateNew)
    try {
        $archive = [System.IO.Compression.ZipArchive]::new(
            $stream,
            [System.IO.Compression.ZipArchiveMode]::Create,
            $false
        )
        try {
            $fixedTime = [DateTimeOffset]::new(2000, 1, 1, 0, 0, 0, [TimeSpan]::Zero)
            $files = Get-ChildItem -LiteralPath $PackageRoot -File -Recurse | Sort-Object FullName
            foreach ($file in $files) {
                $relative = $file.FullName.Substring($Dist.Length + 1).Replace('\', '/')
                $entry = $archive.CreateEntry($relative, [System.IO.Compression.CompressionLevel]::Optimal)
                $entry.LastWriteTime = $fixedTime
                $input = $file.OpenRead()
                $output = $entry.Open()
                try { $input.CopyTo($output) }
                finally { $output.Dispose(); $input.Dispose() }
            }
        }
        finally { $archive.Dispose() }
    }
    finally { $stream.Dispose() }

    $nsisArgs = @(
        '/INPUTCHARSET', 'UTF8',
        "/DVERSION=$Version",
        "/DSTAGE_DIR=$PackageRoot",
        "/DOUTPUT_FILE=$SetupPath",
        (Join-Path $ProjectRoot 'packaging\installer.nsi')
    )
    if ($Signing) {
        $SignScript = (Join-Path $PSScriptRoot 'sign.ps1')
        if ($SignScript -match '[%$`"'']') { throw 'Signing script path contains unsupported command characters' }
        $NsisSignArgs = if ($SigningProfile) {
            '-FromEnvironment'
        } else {
            "-CertificateThumbprint $CertificateThumbprint -TimestampServer $TimestampServer"
        }
        $nsisArgs = $nsisArgs[0..($nsisArgs.Length-2)] + @("/DSIGN_SCRIPT=$SignScript", "/DSIGN_ARGS=$NsisSignArgs", $nsisArgs[-1])
    }
    & $Makensis @nsisArgs
    if ($LASTEXITCODE -ne 0) { throw "makensis failed with exit code $LASTEXITCODE" }
    if (-not (Test-Path $SetupPath -PathType Leaf)) { throw 'NSIS did not produce the expected setup executable.' }

    if ($Signing) {
        & (Join-Path $PSScriptRoot 'sign.ps1') -Path $SetupPath @SignArgs
    }
    $hashLines = foreach ($artifact in @($ZipPath, $SetupPath, $SymbolsPath)) {
        $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $artifact).Hash.ToLowerInvariant()
        "$hash  $([System.IO.Path]::GetFileName($artifact))"
    }
    [System.IO.File]::WriteAllText($ChecksumPath, ($hashLines -join "`n") + "`n", [System.Text.Encoding]::ASCII)

    Write-Host "Created release artifacts (signed: $Signing):"
    Write-Host "  $ZipPath"
    Write-Host "  $SetupPath"
    Write-Host "  $ChecksumPath"
    if (-not $Signing) { Write-Host 'Unsigned build. No browser-store publishing performed.' }
}
finally {
    Pop-Location
}
