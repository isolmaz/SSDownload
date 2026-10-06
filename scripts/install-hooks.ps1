# Installs the local quality gate as a git pre-push hook.
#
# There is no hosted CI: every push runs scripts/check.ps1 locally and is refused
# when it fails. The hook is a small POSIX shell file (Git for Windows runs hooks
# through its bundled sh) that calls the PowerShell gate.
#
# Usage:
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install-hooks.ps1
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest

$ProjectRoot = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$HooksDir = (& git -C $ProjectRoot rev-parse --git-path hooks).Trim()
if (-not [System.IO.Path]::IsPathRooted($HooksDir)) { $HooksDir = Join-Path $ProjectRoot $HooksDir }
New-Item -ItemType Directory -Force -Path $HooksDir | Out-Null
$Hook = Join-Path $HooksDir 'pre-push'
$Body = @'
#!/bin/sh
# Installed by scripts/install-hooks.ps1: the local quality gate (no hosted CI).
exec powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -File scripts/check.ps1
'@
[System.IO.File]::WriteAllText($Hook, ($Body -replace "`r`n", "`n"), [System.Text.UTF8Encoding]::new($false))
Write-Host "pre-push hook installed: $Hook"
