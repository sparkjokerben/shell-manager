# Build release binaries and deploy them to %USERPROFILE%\bin (already on PATH).
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$bin = Join-Path $env:USERPROFILE "bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null
Copy-Item "target\release\sd.exe" (Join-Path $bin "sd.exe") -Force
Copy-Item "target\release\shell-daemon.exe" (Join-Path $bin "shell-daemon.exe") -Force

Write-Host "Deployed to $bin"
Write-Host "Restart the daemon to pick up changes:"
Write-Host "  Stop-Process -Name shell-daemon -Force -ErrorAction SilentlyContinue"
Write-Host "  wscript.exe `"$(Join-Path $bin 'shell-daemon-launch.vbs')`""
