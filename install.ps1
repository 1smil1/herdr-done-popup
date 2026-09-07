$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

# One-time permanent setup: build + link everywhere.
& $Exe install

Write-Host ""
Write-Host "Installed (linked but not enabled). To turn it on:"
Write-Host "  $Exe start"
Write-Host "To turn off (keeps it linked):"
Write-Host "  $Exe stop"
Write-Host "To fully remove:"
Write-Host "  $Exe uninstall"
Write-Host "For new Herdr sessions later, just run inside them:"
Write-Host "  herdr plugin install $Root"
Write-Host "  -- our [[startup]] action will call `start` for you."
Write-Host "Existing herdr_right_click.ahk was not changed."