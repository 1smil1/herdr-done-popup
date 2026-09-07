$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

# Unlink from every session and clean target/.
& $Exe uninstall

# Remove the auto-reattach scheduled task if present.
$TaskName = 'Herdr Done Popup re-attach'
if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "Removed scheduled task '$TaskName'."
}

Write-Host ""
Write-Host "Deleted from Herdr. To fully remove the plugin, delete this directory:"
Write-Host "  Remove-Item -Recurse $Root"
Write-Host "Existing herdr_right_click.ahk was not changed."