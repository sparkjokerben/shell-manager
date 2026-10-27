# smpaste

`smpaste` is a small resident client for shell-manager. It watches the local
clipboard (macOS, Linux, or Windows), and every time a new image appears it
pushes that image to a shell-manager host over one persistent SSH connection.
The host is expected to run `sm clipboard-image --stream`.

Each image is sent as one frame: a 4-byte big-endian length followed by a
32-bit, bottom-up, BGRA BMP (at most 64 MiB). Images are only sent when their
content changes. After an SSH failure the client reconnects every two seconds
and replays the last image once.

On Windows and macOS the watcher first checks the platform clipboard change
counter and only reads image data after the clipboard actually changed. This
keeps the resident process cheap and avoids repeated macOS clipboard-permission
prompts. Linux has no portable change counter, so it reads the clipboard on
every poll.

## Build

```sh
cargo build --release -p smpaste
```

The binary is written to `target/release/smpaste` (`smpaste.exe` on Windows).

## Prebuilt binaries

The `smpaste` GitHub Actions workflow builds artifacts for macOS (arm64 and
x64), Linux (x64 and arm64) and Windows (x64). Download the artifact for your
platform from the workflow run, extract `smpaste`, then make it executable:

```sh
chmod +x smpaste
xattr -d com.apple.quarantine smpaste   # macOS only, for a downloaded binary
```

The binaries are not code-signed or notarized; macOS may ask you to confirm the
first launch.

## Run

```sh
smpaste --ssh-host my-server
smpaste --ssh-host user@example.com --remote-sm /usr/local/bin/sm --interval-ms 500
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--ssh-host <alias>` | required | Host alias from `~/.ssh/config`, or `user@host` |
| `--remote-sm <command>` | `sm` | Remote shell-manager command |
| `--interval-ms <ms>` | `500` | Clipboard poll interval |

Logs are written to stderr. The process starts
`ssh -T <ssh-host> <remote-sm> clipboard-image --stream`, keeps its stdin open,
and exits normally if killed.

## SSH authentication

The stream must start without any interactive prompt (there is no terminal
attached to the remote command). Use key-based authentication and verify it:

```sh
ssh-keygen -t ed25519            # if you do not have a key yet
ssh-copy-id my-server
ssh -o BatchMode=yes my-server true
```

A convenient `~/.ssh/config` entry:

```sshconfig
Host my-server
    HostName example.com
    User me
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
```

## Autostart examples

These are examples only; adjust paths, user names, and host aliases.

### macOS (launchd)

Create `~/Library/LaunchAgents/io.github.smpaste.plist`:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.github.smpaste</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/you/.cargo/bin/smpaste</string>
        <string>--ssh-host</string>
        <string>my-server</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/smpaste.out</string>
    <key>StandardErrorPath</key>
    <string>/tmp/smpaste.err</string>
</dict>
</plist>
```

```sh
launchctl load -w ~/Library/LaunchAgents/io.github.smpaste.plist
```

Launch agents run in the logged-in user's GUI session. If clipboard reads are
blocked by the OS privacy controls on your macOS version, run `smpaste`
once from Terminal to see the prompt, or grant the hosting app the requested
clipboard/accessibility permission.

### Linux (systemd user service)

Create `~/.config/systemd/user/smpaste.service`:

```ini
[Unit]
Description=Forward clipboard images to shell-manager
After=graphical-session.target

[Service]
ExecStart=%h/.cargo/bin/smpaste --ssh-host my-server
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
```

```sh
systemctl --user daemon-reload
systemctl --user enable --now smpaste
journalctl --user -u smpaste -f
```

The service needs access to the graphical session (`DISPLAY` or
`WAYLAND_DISPLAY`). On Wayland compositors that do not expose images through
the default X11 path, rebuild with arboard's data-control feature:

```sh
cargo build --release -p smpaste --features arboard/wayland-data-control
```

### Windows

Run `smpaste.exe` at login from Task Scheduler, or place a shortcut in
`shell:startup`. No clipboard permission is normally required.
