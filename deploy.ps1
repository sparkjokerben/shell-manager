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
#
# -Force stops every live session (via the daemon's Stop op, then by killing the
# daemon's process tree), replaces smd as well, and restarts smd/smw. Because it
# kills the shell it would be running in, run it from a plain SSH command or a
# terminal that is not hosted by sm.
param([switch]$RestartDaemon, [switch]$Force)

$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

# Whether this script is running inside a shell hosted by an smd process. A
# forced update would otherwise terminate the shell halfway through the deploy.
function Test-InsideHostedSession {
    $current = $PID
    for ($i = 0; $i -lt 32; $i++) {
        $proc = Get-CimInstance Win32_Process -Filter "ProcessId=$current" -ErrorAction SilentlyContinue
        if (-not $proc) { return $false }
        $parent = [int]$proc.ParentProcessId
        if ($parent -le 0 -or $parent -eq $current) { return $false }
        $parentProc = Get-CimInstance Win32_Process -Filter "ProcessId=$parent" -ErrorAction SilentlyContinue
        if (-not $parentProc) { return $false }
        if ($parentProc.Name -ieq "smd.exe") { return $true }
        $current = $parent
    }
    return $false
}

function Stop-ProcessTree([int]$Id) {
    $children = @(
        Get-CimInstance Win32_Process -Filter "ParentProcessId=$Id" -ErrorAction SilentlyContinue
    )
    foreach ($child in $children) {
        Stop-ProcessTree -Id ([int]$child.ProcessId)
    }
    Stop-Process -Id $Id -Force -ErrorAction SilentlyContinue
}

if ($Force -and (Test-InsideHostedSession)) {
    throw "deploy.ps1 -Force stops the daemon that hosts this shell. Run it from a plain SSH command or a non-sm terminal."
}

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

# The fixed API token lives in the user environment (HKCU\Environment), so it
# survives reboots and never appears in the logon task's command line where any
# local process could read it. Without it smw still starts, but accepts
# unauthenticated loopback requests only.
$smwToken = [Environment]::GetEnvironmentVariable("SMW_TOKEN", "User")
if (-not $smwToken) {
    Write-Warning "SMW_TOKEN is not set for this user; smw will listen without a token on 127.0.0.1 only. Set one with [Environment]::SetEnvironmentVariable('SMW_TOKEN','...','User')."
}

$smwVbs = Join-Path $bin "smw-launch.vbs"
Copy-Item "smw-launch.vbs" $smwVbs -Force

function Get-ProdWebBridge {
    Get-Process -Name smw -ErrorAction SilentlyContinue | Where-Object {
        $path = $null
        try { $path = $_.Path } catch { }
        $path -and ($path -ieq $smw)
    }
}

# smw is stateless: sessions live in smd, so restarting it only drops browser
# connections. Stop the old image before starting the new one, otherwise both
# would race for port 27183.
$runningSmw = @(Get-ProdWebBridge)
if ($runningSmw.Count -gt 0) {
    $runningSmw | Stop-Process -Force -ErrorAction SilentlyContinue
    $runningSmw | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
}

# Single autostart mechanism for the bridge, mirroring smd: an interactive logon
# task, not a Run key. Overwriting an existing task can fail with access denied
# for a normal user, so keep one that already points at this launcher.
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "smw" -ErrorAction SilentlyContinue
$smwTaskExists = $false
$smwTaskOk = $false
$null = schtasks /query /tn "smw" /fo LIST 2>$null
if ($LASTEXITCODE -eq 0) {
    $smwTaskExists = $true
    $details = schtasks /query /tn "smw" /fo LIST /v 2>$null | Out-String
    $smwTaskOk = $details -match [regex]::Escape($smwVbs)
}
if (-not $smwTaskExists) {
    $smwAction = 'wscript.exe "' + $smwVbs + '"'
    schtasks /create /tn "smw" /tr $smwAction /sc onlogon /it /f | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "schtasks /create for smw failed" }
    Write-Host "Registered logon task 'smw'."
} elseif ($smwTaskOk) {
    Write-Host "Logon task 'smw' already points at $smwVbs; kept as is."
} else {
    Write-Warning "Task 'smw' exists but does not point at $smwVbs; update or remove it manually."
}

# Start it now. The task inherits the user environment, so it picks up
# SMW_TOKEN. Fall back to a direct hidden launch (with the token injected into
# this process) if the scheduler refuses because the old instance is still
# shown as running.
schtasks /run /tn "smw" | Out-Null
if ($LASTEXITCODE -ne 0) {
    if ($smwToken) { $env:SMW_TOKEN = $smwToken }
    Start-Process -FilePath "wscript.exe" -ArgumentList ('"' + $smwVbs + '"') -WindowStyle Hidden
}
Write-Host "Web bridge restarted (task 'smw' runs it at logon)."
Write-Host "Open http://127.0.0.1:27183 (the default bind address)."

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

# A forced update is allowed even with live sessions: stop them through the
# daemon first, then kill what remains so no shell is left orphaned.
if ($Force) {
    $listed = & $sm list 2>$null
    if ($LASTEXITCODE -eq 0) {
        foreach ($line in $listed) {
            $name = ("$line".Trim() -replace '^\*', '').Trim()
            if ($name) {
                & $sm stop $name 2>$null | Out-Null
            }
        }
    } else {
        Write-Warning "Could not list sessions; killing the daemon process tree."
    }
    foreach ($d in @(Get-ProdDaemon)) {
        Write-Host "Force-stopping daemon pid $($d.Id) and its sessions..."
        Stop-ProcessTree -Id $d.Id
        $d | Wait-Process -Timeout 10 -ErrorAction SilentlyContinue
    }
    # Let the scheduler notice the old task instance is gone before re-running it.
    Start-Sleep -Milliseconds 500
    Copy-Item "target\release\smd.exe" $smd -Force
    Start-Daemon
    Write-Host "All sessions stopped; sm, smd and smw updated and restarted."
    exit 0
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
