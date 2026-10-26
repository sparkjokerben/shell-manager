# Build release binaries, deploy them to %USERPROFILE%\bin, and (re)start the daemon.
#
# The daemon must run in the user's interactive session: ConPTY children inherit
# the daemon's session, and GUI work (Electron, screen automation) only runs on
# the interactive desktop. An AtLogOn task with an interactive principal lands
# there, and unlike the old HKCU Run entry it can also be started on demand from
# an SSH window (schtasks /run /tn shell-daemon).
$ErrorActionPreference = "Stop"
Set-Location $PSScriptRoot

cargo build --release
if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

$bin = Join-Path $env:USERPROFILE "bin"
New-Item -ItemType Directory -Force -Path $bin | Out-Null

# Client first: replacing sd.exe never disturbs a running session. A running
# client keeps its image locked, but renaming the image out of the way works.
$sd = Join-Path $bin "sd.exe"
if (Test-Path $sd) { Move-Item -Force $sd (Join-Path $bin "sd.old.exe") }
Copy-Item "target\release\sd.exe" $sd -Force

# Never restart the daemon while it still hosts sessions: that would kill them.
$sessions = & $sd list 2>$null
if ($sessions) {
    Write-Warning "Live sessions exist, daemon left running (it keeps its image until the next restart):"
    $sessions | ForEach-Object { Write-Host "  $_" }
    exit 0
}

# Stop the daemon before copying: a running image cannot be overwritten.
Stop-Process -Name shell-daemon -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 300
Copy-Item "target\release\shell-daemon.exe" (Join-Path $bin "shell-daemon.exe") -Force

# Hidden launcher used by the logon task; it also pins the daemon's working
# directory, which hosted shells inherit.
$vbs = Join-Path $bin "shell-daemon-launch.vbs"
@'
' Hidden launcher for shell-daemon (no console window).
Dim sh, fso, exe
Set sh = CreateObject("WScript.Shell")
Set fso = CreateObject("Scripting.FileSystemObject")
exe = fso.BuildPath(fso.GetParentFolderName(WScript.ScriptFullName), "shell-daemon.exe")
sh.CurrentDirectory = sh.ExpandEnvironmentStrings("%USERPROFILE%")
sh.Run """" & exe & """", 0, False
'@ | Set-Content -Encoding ascii $vbs

Write-Host "Deployed to $bin"

# Single autostart mechanism: the interactive logon task replaces the old Run key.
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name "shell-daemon" -ErrorAction SilentlyContinue
$action = 'wscript.exe "' + $vbs + '"'
schtasks /create /tn "shell-daemon" /tr $action /sc onlogon /it /f | Out-Null
if ($LASTEXITCODE -ne 0) { throw "schtasks /create failed" }
schtasks /run /tn "shell-daemon" | Out-Null
if ($LASTEXITCODE -ne 0) { throw "schtasks /run failed" }

Write-Host "Daemon restarted in the interactive session (task 'shell-daemon')."
Write-Host "Start it again anytime with: schtasks /run /tn shell-daemon"
