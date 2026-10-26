# shell-manager

Persistent Windows shells that survive SSH disconnects.

[中文说明](README.zh-CN.md)

[![ci](https://github.com/sparkjokerben/shell-manager/actions/workflows/ci.yml/badge.svg)](https://github.com/sparkjokerben/shell-manager/actions/workflows/ci.yml)
[![release](https://github.com/sparkjokerben/shell-manager/actions/workflows/release.yml/badge.svg)](https://github.com/sparkjokerben/shell-manager/releases)
[![license: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

`sm` is the client you type into; `smd` is a small background daemon. The daemon
owns a shell hosted in a Windows pseudo console (ConPTY). Close the SSH window,
lose the connection, or detach on purpose - the shell keeps running. Reconnect
and `sm continue <name>` puts you back in front of the same shell, with its
scrollback, running programs and working directory intact.

The hosted shell is not modified in any way: `pwsh.exe` / `wsl.exe` start exactly
as they always do, with your own registry environment, so profiles such as
oh-my-posh render normally.

## Why

An SSH session to Windows ends when the connection does, and so does everything
started in it. `tmux`/`screen` do not exist for native Windows consoles, and WSL
sessions cannot host PowerShell or anything that needs the Windows desktop.

## Requirements

- Windows 10 1809+ or Windows 11 (ConPTY is required).
- Rust 1.74+ if you build from source.

## Install

Download `shell-manager-vX.Y.Z-x64.zip` from
[Releases](https://github.com/sparkjokerben/shell-manager/releases), then
put `sm.exe` and `smd.exe` somewhere on `PATH` (for example `%USERPROFILE%\bin`).

Start the daemon in your interactive desktop session - it must live there,
because ConPTY children inherit its session and only the interactive desktop can
host GUI windows:

```powershell
schtasks /create /tn smd /tr "wscript.exe \"$env:USERPROFILE\bin\smd-launch.vbs\"" /sc onlogon /it /f
schtasks /run /tn smd
```

Simpler alternative: place a shortcut to `smd.exe` in `shell:startup`.

Building from source installs both binaries and registers the logon task:

```powershell
git clone https://github.com/sparkjokerben/shell-manager
cd shell-manager
.\deploy.ps1
```

## Usage

```text
sm start <name>            # create a session and attach to it
sm start <name> --wsl      # host wsl.exe instead of PowerShell
sm continue <name>         # reattach to an existing session
sm list                    # sessions and their attach state
sm detach <name>           # disconnect the attached client, keep the shell
sm stop <name>             # destroy the session and its shell
```

Detach from inside an attached shell with `Ctrl+]` or `F12`; the session keeps
running in the background. `sm detach <name>` does the same from another window
and works no matter what the attached client is doing.

If a command is running while you detach (a build, a test suite, a TUI such as
Claude Code), it keeps running: the daemon keeps draining the pseudo console so
the program never blocks on a full output pipe, and a reattach repaints the
whole screen.

## How it works

```text
terminal ── sm.exe ──┬── control pipe ──┐
                     └── data pipes ────┤
                                        smd.exe ── ConPTY ── pwsh.exe / wsl.exe
```

- `smd` keeps a table of sessions. Each session owns a ConPTY, the shell process
  and two unidirectional named pipes (`out`, `in`) for the attached client.
- While no client is attached, a drain thread keeps reading the PTY output, so a
  program writing to the console never blocks.
- On reattach the client toggles the pseudo-console height (shrink one row, then
  restore) which makes the hosted application redraw its whole screen.
- Shell selection prefers an unpackaged PowerShell 7 (`Program Files` or
  per-user install), then `pwsh.exe` from `PATH`, then the Microsoft Store alias,
  then Windows PowerShell 5.1, and tries them in order until one starts.

## Limitations

- **The daemon lives in your interactive session.** Logging off (or rebooting)
  ends `smd` and every session it hosts. SSH disconnects are unaffected.
- **GUI work needs that desktop session.** Electron apps, screen automation and
  anything else that touches a window only work because hosted shells live in
  the interactive session.
- **The Microsoft Store build of PowerShell 7 cannot be started from a
  non-interactive session** (its `WindowsApps` alias is an MSIX app). In session 0
  the client falls back to the next candidate; in the interactive session it
  works fine.
- **Win32 input mode is suppressed.** PowerShell 7 asks the terminal for it
  (`DECSET 9001`); a terminal that honours the request stops sending plain key
  bytes, which breaks detach key detection and mouse filtering. The client hides
  the request and switches the mode off, so PSReadLine's extended combinations
  (`Shift+Enter`, `Ctrl+Enter`) degrade to plain `Enter` inside sessions.
- **Hover-only mouse motion is filtered out** so it is not typed into a plain
  prompt; clicks, wheel and drag events still reach TUI applications.

## Debugging

- `%TEMP%\smd.log` - daemon start, resolved shell, per-session start results.
- `%TEMP%\sm-client.log` - client build id, console modes, raw stdin bytes and
  detach decisions.

## License

MIT
