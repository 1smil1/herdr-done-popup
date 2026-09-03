$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
foreach ($Session in @('default', 'dse')) {
    Write-Host "Unlinking plugin from Herdr session: $Session"
    & herdr --session $Session plugin unlink herdr-done-popup
    if ($LASTEXITCODE -ne 0) { Write-Warning "Unlink failed for $Session (it may not be linked)." }
}
Write-Host "Plugin unlinked. Existing herdr_right_click.ahk and quota plugin were not changed."
