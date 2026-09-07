$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

# Build the binary (herdr-plugin.toml's [[build]] does the same on the
# user's machine when they run `herdr plugin install`).
Push-Location $Root
try { cargo build --release } finally { Pop-Location }
if (-not (Test-Path $Exe)) { throw "Build did not produce $Exe" }

# Activate in every currently running Herdr session.
# `herdr plugin link` is per-session, so we loop and link each.
& $Exe start

Write-Host ""
Write-Host "Installed. To activate in a brand-new Herdr session later, run from inside it:"
Write-Host "  herdr plugin install $Root"
Write-Host "Or to link to all sessions in one go:"
Write-Host "  $Exe start"
Write-Host "Existing herdr_right_click.ahk was not changed."