$ErrorActionPreference = 'Stop'
$Root = $PSScriptRoot
$Exe = Join-Path $Root 'target\release\herdr-done-popup.exe'

Write-Host "Building Herdr Done Popup..."
Push-Location $Root
try { cargo build --release } finally { Pop-Location }
if (-not (Test-Path $Exe)) { throw "Build did not produce $Exe" }

# Link to every current session. The watcher process (started separately) will
# pick up new sessions automatically.
& $Exe start