$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

# Unlink from every session and clean target/.
& $Exe uninstall

Write-Host ""
Write-Host "Deleted from Herdr. To fully remove the plugin, delete this directory:"
Write-Host "  Remove-Item -Recurse $Root"
Write-Host "Existing herdr_right_click.ahk was not changed."