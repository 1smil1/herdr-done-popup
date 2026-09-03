$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

Write-Host "Building Herdr Done Popup..."
Push-Location $Root
try { cargo build --release } finally { Pop-Location }
if (-not (Test-Path $Exe)) { throw "Build did not produce $Exe" }

# Each named Herdr server has its own plugin registry.
foreach ($Session in @('default', 'dse')) {
    Write-Host "Linking plugin into Herdr session: $Session"
    & herdr --session $Session plugin link $Root --enabled
    if ($LASTEXITCODE -ne 0) { throw "Plugin link failed for session $Session" }
    & herdr --session $Session server reload-config
    if ($LASTEXITCODE -ne 0) { Write-Warning "Config reload failed for $Session; restart that session if needed." }
}

# Start one shared receiver. A second start is harmless: its TCP port is occupied.
Start-Process -FilePath $Exe -ArgumentList 'receiver' -WindowStyle Hidden
Write-Host "Installed. Receiver started."
Write-Host "Existing herdr_right_click.ahk was not changed."
