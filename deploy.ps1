# Build release binaries, deploy them to %USERPROFILE%\bin, and manage the daemon.
#
# smd must run in the user's interactive session: ConPTY children inherit the
# daemon's session, and GUI work (Electron, screen automation) only runs on the
# interactive desktop. An AtLogOn task with an interactive principal lands there,
# and unlike a HKCU Run entry it can also be started on demand with
#   schtasks /run /tn smd
#
# Restarting smd kills every hosted session, so it only happens when no sessions
# exist and -RestartDaemon is passed explicitly. A failed probe of the daemon is
# never treated as "no sessions".
param([switch]$RestartDaemon)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$bin = Join-Path $env:USERPROFILE "bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null

# Client first: replacing sm.exe never disturbs a running session. A running
# client keeps its image locked, but renaming the image out of the way works.
$sm = Join-Path $bin "sm.exe"
if (Test-Path $sm) { Move-Item -Force $sm (Join-Path $bin "sm.old.exe") }
Copy-Item "target\release\sm.exe" $sm -Force
Write-Host "Client deployed to $bin"

# Hidden launcher used by the logon task; it also pins the daemon's working
# directory, which hosted shells inherit.
$vbs = Join-Path $bin "smd-launch.vbs"
Copy-Item "smd-launch.vbs" $vbs -Force

function Start-Daemon {
    # Single autostart mechanism: the interactive logon task (no Run key).
    Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "smd" -ErrorAction SilentlyContinue
    $action = 'wscript.exe "' + $vbs + '"'
    schtasks /create /tn "smd" /tr $action /sc onlogon /it /f | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "schtasks /create failed" }
    schtasks /run /tn "smd" | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "schtasks /run failed" }
    Write-Host "Daemon started in the interactive session (task 'smd')."
}

if (-not (Get-Process smd -ErrorAction SilentlyContinue)) {
    # Nothing running, nothing to protect.
    Copy-Item "target\release\smd.exe" (Join-Path $bin "smd.exe") -Force
    Start-Daemon
    exit 0
}

# The daemon is running: find out whether it hosts anything.
$listOk = $false
$sessions = @()
for ($i = 0; $i -lt 3 -and -not $listOk; $i++) {
    $out = & $sm list 2>$null
    if ($LASTEXITCODE -eq 0) {
        $listOk = $true
        $sessions = @($out | Where-Object { "$_".Trim() -ne "" })
    } else {
        Start-Sleep -Milliseconds 500
    }
}
if (-not $listOk) {
    Write-Warning "Could not query the daemon; refusing to restart it. Client only was updated."
    exit 1
}
if ($sessions.Count -gt 0) {
    Write-Warning "Live sessions exist; daemon left running (it keeps its image):"
    $sessions | ForEach-Object { Write-Host "  $_" }
    exit 0
}
if (-not $RestartDaemon) {
    Write-Host "No live sessions. Daemon left untouched; pass -RestartDaemon to restart it."
    exit 0
}

# Stop the daemon before copying: a running image cannot be overwritten.
Stop-Process -Name smd -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Copy-Item "target\release\smd.exe" (Join-Path $bin "smd.exe") -Force
Start-Daemon
