# smpaste

`smpaste` is a small resident client for shell-manager. It watches the local
clipboard (macOS, Linux, or Windows), and every time a new image appears it
pushes that image to one or more shell-manager hosts over persistent SSH
connections. Each host is expected to run `sm clipboard-image --stream`.

By default `smpaste` discovers the hosts itself: it scans the process list for
interactive `ssh` sessions and keeps one stream per host. Pass
`--ssh-host <alias>` to forward to a single fixed host instead.

Each image is sent as one frame: a 4-byte big-endian length followed by a
32-bit, bottom-up, BGRA BMP (at most 64 MiB). Images are only sent when their
content changes. After an SSH failure the client retries with a short backoff
(two seconds, doubling up to 60 seconds for discovered hosts) and replays the
last image once when the stream comes back. Discovered hosts are rescanned
every five seconds; a host that appears later immediately receives the most
recent image.

On Windows and macOS the watcher first checks the platform clipboard change
counter and only reads image data after the clipboard actually changed. This
keeps the resident process cheap and avoids repeated macOS clipboard-permission
prompts. Linux has no portable change counter, so it reads the clipboard on
every poll.

When Otty pastes an image, the host receives its path and waits about 400 ms
for this watcher to deliver the image before injecting the paste key. Keep the
poll interval at or below roughly 300 ms (the default is 200 ms), or raise the
host's wait with `SM_PASTE_DELAY_MS`.

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
smpaste                                          # discover interactive ssh sessions
smpaste --ssh-host my-server                     # forward to one host only
smpaste --ssh-host user@example.com --remote-sm /usr/local/bin/sm --interval-ms 200
```

| Option | Default | Meaning |
| --- | --- | --- |
| `--ssh-host <alias>` | discovery | Host alias from `~/.ssh/config`, or `user@host`. When given, only that host is used and no scanning happens. |
| `--remote-sm <command>` | `sm` | Remote shell-manager command |
| `--interval-ms <ms>` | `200` | Clipboard poll interval |
| `--scan-ms <ms>` | `5000` | Process-list scan interval (only used by automatic discovery) |

Logs are written to stderr. For each host the process starts
`ssh -o BatchMode=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -C -T
<destination> <remote-sm> clipboard-image --stream`, keeps its stdin open, and
exits normally if killed.

## Automatic host discovery

Without `--ssh-host`, `smpaste` runs `ps` every `--scan-ms` (five seconds by
default) and parses the command line of each process. A process is treated as
an interactive SSH session when:

- the executable's basename is exactly `ssh`;
- it has a destination and **no remote command**, so `git` over SSH, `scp`,
  VS Code Remote, and smpaste's own `clipboard-image --stream` connections are
  ignored;
- it does not use `-T`, `-N`, `-W`, `-D`, `-n`, `-f`, or another
  non-interactive or forwarding option.

The destination (`user@host` or an alias from `~/.ssh/config`) and the
connection options that matter (`-F`, `-i`, `-J`, `-l`, `-p`, `-S`, and
non-conflicting `-o` settings) are reused for the clipboard stream. Local,
remote, and dynamic forwards are deliberately dropped: a new connection that
binds the same ports again would fail.

Otty starts its interactive sessions as `ssh … <host> sh -c 'export
OTTY_REMOTE=1; …'`. That wrapper is recognised as the terminal's login
command, so the session still counts as interactive. A carried
`-o ControlPath=…` makes the background stream reuse the existing SSH master:
it normally shares the already authenticated connection, so no separate key
check is needed for the stream, and ssh falls back to a direct connection when
the master is gone. Relative `-i`, `-F`, and `-S` paths are not inherited
because the background process does not share the original working directory;
only absolute and `~`-rooted paths are reused.

Every discovered host gets its own stream. A host that disappears is closed;
one that appears later is connected and immediately receives the newest image.
Discovery looks at the machine's process list, so run `smpaste` as the same
user whose `ssh` sessions you want it to follow; other users' sessions are not
something it can authenticate as.

Limitations:

- Only standalone `ssh` processes are visible, including the `sh -c` wrapper
  Otty adds. SSH implemented inside other applications and `mosh` sessions are
  not.
- Every discovered host must accept non-interactive authentication (see
  below), because the clipboard stream has no terminal. Sessions with a
  `ControlPath` that still reaches a live master are exempt, since they reuse
  that authenticated connection.
- Automatic discovery is available on macOS and Linux only. On Windows pass
  `--ssh-host <alias>`.

## SSH authentication

The stream must start without any interactive prompt (there is no terminal
attached to the remote command). Use key-based authentication and verify it on
every host you want to forward to, including hosts found by automatic
discovery:

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

Add `--ssh-host my-server` to the `ProgramArguments` array to pin a single
host instead of using discovery.

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
ExecStart=%h/.cargo/bin/smpaste
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

Add `--ssh-host my-server` to `ExecStart` to pin a single host instead of
using discovery.

The service needs access to the graphical session (`DISPLAY` or
`WAYLAND_DISPLAY`). On Wayland compositors that do not expose images through
the default X11 path, rebuild with arboard's data-control feature:

```sh
cargo build --release -p smpaste --features arboard/wayland-data-control
```

### Windows

Run `smpaste.exe` at login from Task Scheduler, or place a shortcut in
`shell:startup`. No clipboard permission is normally required. Automatic host
discovery is not available on Windows, so pass `--ssh-host <alias>`.
