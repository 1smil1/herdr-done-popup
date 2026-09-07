$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

# One-time permanent setup: build + link everywhere.
& $Exe install

# Auto-reattach after herdr restarts. A scheduled task runs `start` every
# 5 minutes; each call is idempotent (server reload-config + enable).
# It re-registers the [[events]] hook that herdr loses on restart.
$TaskName = 'Herdr Done Popup re-attach'
$Action = New-ScheduledTaskAction -Execute $Exe -Argument 'start'
$Trigger = New-ScheduledTaskTrigger -Once -At (Get-Date) `
    -RepetitionInterval (New-TimeSpan -Minutes 5) `
    -RepetitionDuration (New-TimeSpan -Days 3650)
$Settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
Register-ScheduledTask -TaskName $TaskName -Action $Action -Trigger $Trigger `
    -Settings $Settings -Description "Re-attach herdr-done-popup to all Herdr sessions every 5 minutes (herdr loses [[events]] hooks on restart)." `
    -Force | Out-Null
Write-Host "Scheduled task '$TaskName' created (runs 'start' every 5 min, idempotent)."

Write-Host ""
Write-Host "Installed and auto-reattaching. To turn the plugin off:"
Write-Host "  $Exe stop"
Write-Host "To fully remove:"
Write-Host "  $Exe uninstall"
Write-Host "Existing herdr_right_click.ahk was not changed."