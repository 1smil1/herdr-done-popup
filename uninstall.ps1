$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

# 1. Unlink from every session.
& $Exe stop

# 2. Stop the watcher (if any).
Get-Process herdr-done-popup -ErrorAction SilentlyContinue | Where-Object {
    $_.MainWindowTitle -eq '' -and $_.StartTime -lt (Get-Date).AddMinutes(-1)
} | Stop-Process -Force -ErrorAction SilentlyContinue

Write-Host "Uninstalled. Existing herdr_right_click.ahk was not changed."