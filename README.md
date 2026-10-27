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
- Rust 1.89+ if you build from source.
- Node.js 22.12+ if you build the web UI from source (`deploy.ps1` builds it for you).

## Install

Download `shell-manager-vX.Y.Z-x64.zip` from
[Releases](https://github.com/sparkjokerben/shell-manager/releases), then
put `sm.exe`, `smd.exe` and (for the browser UI) `smw.exe` somewhere on `PATH`
(for example `%USERPROFILE%\bin`).

Start the daemon in your interactive desktop session - it must live there,
because ConPTY children inherit its session and only the interactive desktop can
host GUI windows:

```powershell
$vbs = "$env:USERPROFILE\bin\smd-launch.vbs"
schtasks /create /tn smd /tr ('wscript.exe "{0}"' -f $vbs) /sc onlogon /it /f
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

`deploy.ps1` replaces `sm.exe` and `smw.exe` immediately and restarts `smw`
(browser connections drop; sessions keep running). It only replaces `smd.exe`
when the installed daemon is not running or hosts no sessions; a running daemon
is restarted only with `-RestartDaemon`, which ends every hosted shell.

`-Force` is the unconditional update: it stops every hosted session, replaces
`smd.exe` too, and restarts `smd`/`smw`. Because it kills the shell it would be
running in, invoke it from a plain SSH command or a terminal that is not hosted
by `sm`:

```powershell
.\deploy.ps1 -Force
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

## Pasting images from an SSH client

SSH carries text only, and Claude Code reads images from the clipboard of the
machine that runs it. `smpaste` (the small client binary in `smpaste/`) closes
that gap: it watches the local clipboard and pushes every new image to the
hosts it finds over persistent SSH connections. By default it discovers the
hosts itself by scanning the local process list for interactive `ssh`
sessions, so you can just start it; pass `--ssh-host <alias>` to pin a single
host instead.

On the SSH client, download the artifact for your platform from the `smpaste`
GitHub Actions run (macOS arm64/x64, Linux x64/arm64, Windows x64), or build it
yourself:

```sh
cargo build --release -p smpaste
smpaste
```

To forward to one specific host, run
`smpaste --ssh-host <your-ssh-alias>`; the discovery interval can be tuned with
`--scan-ms` (default 5000 ms). Automatic discovery is available on macOS and
Linux only; on Windows pass `--ssh-host`.

The host stages the newest image in `%TEMP%\shell-manager-paste\latest.bmp`.

The preferred way to paste is `Cmd+V` in Otty: Otty writes the copied image to
`$TMPDIR/otty-paste/image-<number>.png` on the client and sends that absolute
path as plain text. When a whole input is such a path, `sm` drops the text,
waits about 400 ms for the watcher to deliver the image, asks the daemon to
publish it on the Windows clipboard as `CF_DIB`, and then sends Claude Code its
normal image-paste key, so the image becomes an `[Image #N]` attachment instead
of a path. If the host has no staged image yet, the path is forwarded as
ordinary text instead. Because `sm` cannot see which program owns the
foreground, a pasted path is also recognised at an ordinary shell prompt; the
text is only swallowed when a staged image exists. The wait can be tuned with
`SM_PASTE_DELAY_MS` (milliseconds, 1-10000).

As a manual fallback, press `Ctrl+'` (sent as `0x1c`; `Ctrl+\` on many
keyboards) inside an attached `sm` session; it asks the daemon to publish the
staged image and then sends the paste key in the same way. The daemon and
Claude Code must run in the same interactive Windows session, and the watcher
must already have delivered the image.

The watcher forwards **every** clipboard image, not only the ones intended for
the host. Run it only on a client you are comfortable sending that data from.
To find SSH sessions it also reads the local process list. Autostart examples
and SSH authentication notes are in `smpaste/README.md`.

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
need the interactive desktop - then open <http://127.0.0.1:27183>:

```text
> smw
smw 0.1.1 instance=prod listening on http://127.0.0.1:27183 (token off)
```

Options:

```text
smw [--bind <addr>] [--port <port>] [--token <token>] [--instance <name>]
    [--allow-insecure] [-h] [-V]
  --bind <addr>      address to listen on (default: 127.0.0.1)
  --port <port>      TCP port to listen on (default: 27183)
  --token <token>    API token (default: $SMW_TOKEN; prefer the env var)
  --instance <name>  instance namespace (overrides $SM_INSTANCE)
  --allow-insecure   allow a non-loopback bind without a token (dangerous)
  -h, --help         show help
  -V, --version      show the version
```

### Autostart and a fixed token

A production deployment usually starts `smw` at logon. `deploy.ps1`:

- copies the hidden `smw-launch.vbs` into `%USERPROFILE%\bin`;
- registers the logon task `smw` (`schtasks /create /tn smw /sc onlogon /it`);
- stops the old `smw` and starts the new one immediately, so the freshly
  embedded UI takes effect.

The token lives in the **user environment variable** `SMW_TOKEN`. It is stored in
`HKCU\Environment`, survives reboots and re-logins, and never appears in the task
definition or a process command line:

```powershell
[Environment]::SetEnvironmentVariable('SMW_TOKEN', '<token>', 'User')
```

Only processes started afterwards see it. Read, replace or delete it with:

```powershell
[Environment]::GetEnvironmentVariable('SMW_TOKEN','User')
[Environment]::SetEnvironmentVariable('SMW_TOKEN','new-token','User')
[Environment]::SetEnvironmentVariable('SMW_TOKEN',$null,'User')
```

Do not combine an empty token with a non-loopback bind: `smw` refuses that unless
`--allow-insecure` is passed explicitly. To disable autostart:

```powershell
schtasks /delete /tn smw /f
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

Without a token every request is accepted. `smw` binds to `127.0.0.1` by default,
which keeps it off the network - but loopback is not a user boundary: on a
multi-user machine another local account can reach the port, and a web page can
open a WebSocket to it. Set a token (`--token` or, preferably, `SMW_TOKEN`) on
shared machines.

With a token, HTTP requests need `Authorization: Bearer <token>` or
`X-SMW-Token: <token>`, and WebSocket upgrades also accept `?token=`. The static
page and `/api/health` stay public so the browser can load the UI and learn
whether a token is configured. Browser WebSocket clients cannot send custom
headers, so the token appears in the URL query string; treat that URL as a
secret and keep it out of access logs.

`smw` validates `Origin`/`Host` on every API and WebSocket request: a browser
origin must match the `Host`, and a tokenless loopback listener also rejects
non-loopback host names (DNS rebinding). Scripts that send no `Origin` are
allowed. Binding to a non-loopback address without a token is refused unless
`--allow-insecure` is passed explicitly.

Never expose a tokenless `smw` beyond the local machine. Forwarding the port
(including through frp) is your responsibility.

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
localPort = 27183
remotePort = 18787
```

A public deployment must set a token (`--token`, or preferably `SMW_TOKEN`).
Note what `transport.tls` does: it encrypts only the frpc-to-frps tunnel. A
`type = "tcp"` proxy still exposes plain HTTP/WebSocket from the browser to
`frps`, so the token in the URL and all terminal traffic are readable on that
leg. Terminate TLS in front of frps (for example with `vhostHTTPSPort` and the
`https2http` plugin, or with nginx/Caddy) and use `https://`/`wss://`, or put the
tunnel behind a VPN. Redact query strings in proxy access logs. Do not forward a
tokenless `smw` port.

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
- `%TEMP%\smw.log` (or `%TEMP%\smw-<instance>.log`) - web bridge start, session
  operations, attach/detach and errors (never tokens or key content).

## License

MIT
