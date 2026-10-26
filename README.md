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
running programs and working directory intact and its current screen redrawn.

The hosted shell starts with your own registry environment (plus
`TERM=xterm-256color`, which describes the pseudo console accurately), so
profiles such as oh-my-posh render normally.

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
put `sm.exe`, `smd.exe` and (for the browser UI) `smw.exe` somewhere on `PATH`
(for example `%USERPROFILE%\bin`).

Start the daemon in your interactive desktop session - it must live there,
because ConPTY children inherit its session and only the interactive desktop can
host GUI windows:

```powershell
schtasks /create /tn smd /tr "wscript.exe \"$env:USERPROFILE\bin\smd-launch.vbs\"" /sc onlogon /it /f
schtasks /run /tn smd
```

Simpler alternative: place a shortcut to `smd.exe` in `shell:startup`.

Building from source builds the web UI, installs all three binaries and
registers the logon task for `smd`:

```powershell
git clone https://github.com/sparkjokerben/shell-manager
cd shell-manager
.\deploy.ps1
```

## Usage

```text
sm <name>                  # attach to <name>, creating it if it does not exist
sm                         # back to the most recently used session
sm ls                      # sessions and their attach state
sm detach [name]           # disconnect the attached client, keep the shell
sm stop [name]             # destroy the session and its shell
sm start <name> --wsl      # create only (fails if it exists), hosting wsl.exe
```

`a`/`attach`, `n`/`new`, `l`/`list`, `d`/`detach` and `k`/`kill` are accepted as
short aliases; a name is required for `start` and optional everywhere else
(omitted means the most recently used session).

Detach from inside an attached shell with `Ctrl+]` or `F12`; the session keeps
running in the background. `sm detach <name>` does the same from another window
and works no matter what the attached client is doing.

If a command is running while you detach (a build, a test suite, a TUI such as
Claude Code), it keeps running: the daemon keeps draining the pseudo console so
the program never blocks on a full output pipe, and a reattach repaints the
whole screen.

## Web UI

`smw` is an optional third binary that puts a terminal in the browser on top of
the same daemon:

```text
browser ── smw.exe (HTTP/WebSocket) ── named-pipe protocol ── smd.exe ── ConPTY
```

It does not change `smd` or the existing protocol; it speaks the same named
pipes as `sm`. The frontend is a Vite + React + TypeScript + xterm.js app, and
its static files are embedded into `smw.exe` at compile time.

The frontend build is not committed, so build it before compiling `smw`. A plain
`cargo build` still succeeds without it, but `smw` then serves only a
placeholder page that says the UI is missing:

```powershell
cd web
npm ci
npm run build
cd ..
cargo build --release --bin smw
```

Rebuild `smw` after any frontend change, because the whole `web/dist` directory
is embedded at compile time.

Start the bridge under the same Windows user as `smd` - `smw` itself does not
need the interactive desktop - then open <http://127.0.0.1:8787>:

```text
> smw
smw 0.1.1 instance=prod listening on http://127.0.0.1:8787 (token off)
```

Options:

```text
smw [--bind <addr>] [--port <port>] [--token <token>] [--instance <name>] [-h] [-V]
  --bind <addr>      address to listen on (default: 127.0.0.1)
  --port <port>      TCP port to listen on (default: 8787)
  --token <token>    API token (default: $SMW_TOKEN)
  --instance <name>  instance namespace (overrides $SM_INSTANCE)
  -h, --help         show help
  -V, --version      show the version
```

The instance rules match `sm`/`smd`: `SM_INSTANCE` selects a namespace, and an
executable under this project's `target/` directory automatically uses `dev`.
`smw` talks to the daemon in the same namespace.

The browser sees the same sessions as `sm`. Closing the tab or pressing Detach
detaches the client and leaves the shell running; Force takeover first
disconnects whichever client is attached, then connects the browser.

The JSON API mirrors the client commands:

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/api/health` | liveness, instance, version, whether a token is set |
| GET | `/api/sessions` | sessions and their attach state |
| POST | `/api/sessions` | create a session (`{"name": "...", "wsl": false}`) |
| POST | `/api/sessions/{name}/detach` | detach the attached client |
| DELETE | `/api/sessions/{name}` | stop the session and its shell |
| GET | `/ws/{name}` | WebSocket terminal for that session |

### Security

Without a token every request is accepted, which is only safe because `smw`
listens on `127.0.0.1` by default. Set a token with `--token` or `SMW_TOKEN` to
require authentication. Then HTTP requests need `Authorization: Bearer <token>`
or `X-SMW-Token: <token>`, and WebSocket upgrades also accept `?token=`. The
static page and `/api/health` stay public so the browser can load the UI and
learn whether a token is configured. Browser WebSocket clients cannot send
custom headers, so the token appears in the URL query string; treat that URL as
a secret.

Never expose a tokenless `smw` beyond the local machine. Binding with
`--bind 0.0.0.0` or forwarding the port is your responsibility.

### frp

`smw` works behind [frp](https://github.com/fatedier/frp). Run `frpc` on the same
machine and forward the local port to your `frps`:

```toml
# frpc.toml
serverAddr = "frps.example.com"
serverPort = 7000

[[proxies]]
name = "smw"
type = "tcp"
localIP = "127.0.0.1"
localPort = 8787
remotePort = 18787
```

A public deployment must set a token (`--token` or `SMW_TOKEN`), and you should
enable TLS on the `frps` side (`transport.tls` in `frps.toml`) so the terminal
traffic is encrypted. Do not forward a tokenless `smw` port.

### Limitations

- **One client per session.** A native `sm` attachment and a browser terminal
  are mutually exclusive; while one is attached the other reports the session
  as busy until it detaches or is taken over.
- **Reconnecting restores the current screen only.** ConPTY repaints what the
  application redraws, so output produced while detached is not kept as
  scrollback history.
- **Same Windows user.** `smw` and `smd` must run under the same account; the
  named pipes use the default DACL, so another user cannot open them.
- **`smd` must still live in the interactive desktop session**, exactly as it
  does for `sm`.

## Development instances

Builds run from this repository are isolated from the installed production
daemon, so you can hack on `sm`/`smd` while your everyday sessions keep running:

- An executable located under this project's `target/` directory automatically
  uses the `dev` instance.
- Any other executable uses `prod`, which keeps the historical names.
- `SM_INSTANCE=<name>` selects another namespace. The value is trimmed and
  lowercased; it must be 1-32 characters of `A-Z a-z 0-9 - _`. An invalid value
  is ignored (with a warning) and falls through to the rules above, so a typo
  can never reconnect a development build to production.

Every instance has its own control pipe, data pipes, single-instance mutex and
logs (`%TEMP%\smd-<instance>.log`, `%TEMP%\sm-client-<instance>.log`).
Production keeps `\\.\pipe\shell-manager\...`, `Global\smd-single-instance` and
`%TEMP%\smd.log`, so upgrading does not disturb a running daemon.

Run a development daemon and client next to the installed production pair:

```powershell
cargo run --bin smd      # serves the "dev" namespace
cargo run --bin sm -- ls # talks to that dev daemon
```

`sm --version` prints the instance it resolved.

## How it works

```text
terminal ── sm.exe ──┬── control pipe ──┐
                     └── data pipes ────┤
                                        smd.exe ── ConPTY ── pwsh.exe / wsl.exe
```

- `smd` keeps a table of sessions. Each session owns a ConPTY, the shell process
  and two unidirectional named pipes (`out`, `in`) for the attached client.
- Hosted shells are told they run on `xterm-256color`. ConPTY is a VT terminal,
  and Unicode-aware programs use that variable to decide whether to draw `○`,
  `□` and `☒` or ASCII fallbacks such as `()`, `[]` and `[x]`.
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

- `%TEMP%\smd.log` (or `%TEMP%\smd-<instance>.log` outside production) -
  daemon start, resolved shell, per-session start results.
- `%TEMP%\sm-client.log` (or `%TEMP%\sm-client-<instance>.log`) - client build
  id, console modes, raw stdin bytes and detach decisions.

## License

MIT
