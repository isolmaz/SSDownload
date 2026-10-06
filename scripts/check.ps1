# Local quality gate. There is no hosted CI: run this before every push (the
# pre-push hook from scripts/install-hooks.ps1 does it automatically) and before
# every release.
#
# Steps: cargo fmt --check, Clippy with warnings as errors, the Rust tests, the
# browser-extension tests (Node.js) and, when installed, cargo audit. A step that
# cannot run is reported as skipped, never as passed.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\check.ps1

[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
Push-Location $ProjectRoot
$failures = @()
$skipped = @()

function Invoke-Step {
    param([string]$Name, [scriptblock]$Body)
    Write-Host "`n=== $Name ===" -ForegroundColor Cyan
    # Native stderr is progress output, not a terminating PowerShell error.
    $ErrorActionPreference = 'Continue'
    & $Body | Out-Host
    $code = $LASTEXITCODE
    $ErrorActionPreference = 'Stop'
    if ($code -eq 0) {
        Write-Host "PASS: $Name" -ForegroundColor Green
    } else {
        $script:failures += "$Name (exit code $code)"
        Write-Host "FAIL: $Name (exit code $code)" -ForegroundColor Red
    }
}

function Test-Command([string]$Name) {
    return [bool](Get-Command $Name -ErrorAction SilentlyContinue)
}

try {
    Invoke-Step 'cargo fmt --check' { cargo fmt --check }
    Invoke-Step 'cargo clippy -D warnings' { cargo clippy --locked --all-targets -- -D warnings }
    Invoke-Step 'cargo test' { cargo test --locked }
    if (Test-Command 'node') {
        $suites = Get-ChildItem -LiteralPath (Join-Path $ProjectRoot 'browser\tests') -Filter '*.test.js' | ForEach-Object { $_.FullName }
        Invoke-Step 'browser extension tests' { node --test @suites }
    } else {
        $skipped += 'browser extension tests (Node.js 20+ is not installed)'
    }
    $auditVersion = $null
    try { $auditVersion = (& cargo audit --version 2>$null | Select-Object -First 1) } catch { $auditVersion = $null }
    if ($auditVersion) {
        Invoke-Step "cargo audit ($auditVersion)" { cargo audit }
    } else {
        $skipped += 'cargo audit (not installed: cargo install cargo-audit)'
    }
} finally {
    Pop-Location
}

Write-Host ''
if ($skipped.Count -gt 0) {
    Write-Host 'Skipped (not covered by this run):' -ForegroundColor Yellow
    $skipped | ForEach-Object { Write-Host "  - $_" }
}
if ($failures.Count -gt 0) {
    Write-Host 'Result: FAILED' -ForegroundColor Red
    $failures | ForEach-Object { Write-Host "  - $_" }
    exit 1
}
Write-Host 'Result: PASS' -ForegroundColor Green
exit 0
