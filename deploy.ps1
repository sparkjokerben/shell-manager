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

# The web UI is embedded into smw.exe at compile time. Build it first so the
# deployed smw serves the real frontend instead of the "not built" placeholder.
$web = Join-Path $PSScriptRoot "web"
if (-not (Get-Command npm -ErrorAction SilentlyContinue)) {
    throw "npm was not found on PATH; install Node.js (22.12 or newer) to build the web UI for smw.exe"
}
$lockFile = Join-Path $web "package-lock.json"
$installedLock = Join-Path $web "node_modules\.package-lock.json"
$needsInstall = -not (Test-Path $installedLock)
if (-not $needsInstall -and (Test-Path $lockFile)) {
    # Reinstall when the lockfile changed since the last npm ci.
    $needsInstall = (Get-Item $lockFile).LastWriteTimeUtc -gt (Get-Item $installedLock).LastWriteTimeUtc
}
if ($needsInstall) {
    Write-Host "Installing web UI dependencies (npm ci)..."
    & npm --prefix $web ci
    if ($LASTEXITCODE -ne 0) { throw "npm ci failed in $web" }
}
Write-Host "Building web UI (npm run build)..."
& npm --prefix $web run build
if ($LASTEXITCODE -ne 0) {
    throw "npm run build failed in $web; smw.exe would only serve a placeholder page"
}

cargo build --release --locked
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$bin = Join-Path $env:USERPROFILE "bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null

# Client first: replacing sm.exe never disturbs a running session. A running
# client keeps its image locked, but renaming the image out of the way works.
$stamp = Get-Date -Format "yyyyMMddHHmmss"
$sm = Join-Path $bin "sm.exe"
if (Test-Path $sm) { Move-Item -Force $sm (Join-Path $bin "sm.old.$stamp.exe") }
Copy-Item "target\release\sm.exe" $sm -Force
Write-Host "Client deployed to $bin"

# Browser bridge. It is a service started on demand, so no logon task is
# registered for it; replace a running image the same way as sm.exe.
$smw = Join-Path $bin "smw.exe"
if (Test-Path $smw) { Move-Item -Force $smw (Join-Path $bin "smw.old.$stamp.exe") }
Copy-Item "target\release\smw.exe" $smw -Force
Write-Host "Web bridge deployed to $bin"
Write-Host "If an smw instance is running, restart it to load the new embedded UI."
Write-Host "smw is not started at logon. Start it manually, for example:"
Write-Host "  smw --token <token>"
Write-Host "then open http://127.0.0.1:8787 (the default bind address)."

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

$smd = Join-Path $bin "smd.exe"

# A development daemon (run from target\debug or target\release) has the same
# image name but its own instance namespace and its own sessions. Only the
# installed copy at $smd is the production daemon, so probe and stop by image
# path instead of by process name.
function Get-ProdDaemon {
    Get-Process -Name smd -ErrorAction SilentlyContinue | Where-Object {
        $path = $null
        try { $path = $_.Path } catch { }
        $path -and ($path -ieq $smd)
    }
}

$prodDaemons = @(Get-ProdDaemon)
if ($prodDaemons.Count -eq 0) {
    # Nothing running, nothing to protect.
    Copy-Item "target\release\smd.exe" $smd -Force
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
Stop-Process -Id $prodDaemons.Id -Force -ErrorAction SilentlyContinue
$prodDaemons | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
Copy-Item "target\release\smd.exe" $smd -Force
Start-Daemon
