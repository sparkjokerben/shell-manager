use std::borrow::Cow;
use std::os::windows::ffi::OsStrExt;
use std::ffi::OsStr;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use anyhow::{Context, Result};
use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, ReadFile, WriteFile,
};
use windows::Win32::System::Console::{
    GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    GetConsoleMode, SetConsoleMode,
    GetConsoleScreenBufferInfo, CONSOLE_SCREEN_BUFFER_INFO,
    GetConsoleCP, GetConsoleOutputCP, SetConsoleCP, SetConsoleOutputCP,
    ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_WINDOW_INPUT, ENABLE_MOUSE_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, CONSOLE_MODE,
    ENABLE_EXTENDED_FLAGS, ENABLE_QUICK_EDIT_MODE, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_PROCESSED_OUTPUT, DISABLE_NEWLINE_AUTO_RETURN,
};
use windows::core::PCWSTR;
use shell_manager::protocol::{self, Request, Op};
use shell_manager::session::{for_each_mouse_mode, mouse_bit, mouse_carry_tail};

const USAGE: &str = "\
usage: sm [name]          attach to <name>, creating it if needed
       sm                 attach to the most recently used session
       sm start <name>    create a session (fails if it exists)
       sm detach [name]   disconnect the attached client, keep the shell
       sm stop [name]     destroy the session and its shell
       sm ls              list sessions
       sm clipboard-image [--stream]  stage a BMP frame from stdin for paste
options: --wsl  create the session with wsl.exe instead of PowerShell
         --stream  keep reading frames until stdin reaches EOF (clipboard-image)
environment: SM_INSTANCE  use an isolated namespace for pipes, mutex and logs
                          (1-32 chars of A-Z a-z 0-9 - _; default: prod, or dev
                          when the executable runs under target/)
           SM_PASTE_TRIGGER  image-paste trigger byte (default 0x1c; 0xNN or
                          decimal)
           SM_PASTE_DELAY_MS  wait for a just-copied image before pasting a
                          detected Otty path (default 400; 1-10000)";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Cmd {
    /// Attach to the session, creating it when it does not exist. `None` means
    /// "the most recently used session".
    Attach { name: Option<String>, wsl: bool },
    /// Create only; fails when the name is taken.
    Start { name: String, wsl: bool },
    Detach { name: Option<String> },
    Stop { name: Option<String> },
    List,
    /// Stage a BMP frame received on stdin for the image-paste bridge.
    ClipboardImage { stream: bool },
    Help,
    Version,
}

fn parse(args: &[String]) -> std::result::Result<Cmd, String> {
    // The paste bridge runs this as a separate SSH command, before any session
    // name is involved. It is resolved first so `--stream` is a legal option
    // here while still being rejected everywhere else.
    if args.first().map(String::as_str) == Some("clipboard-image") {
        let mut stream = false;
        for arg in &args[1..] {
            match arg.as_str() {
                "--stream" => stream = true,
                other => return Err(format!("unknown option: {}", other)),
            }
        }
        return Ok(Cmd::ClipboardImage { stream });
    }

    let mut wsl = false;
    let mut words: Vec<&str> = Vec::with_capacity(args.len());
    for arg in args {
        match arg.as_str() {
            "--wsl" => wsl = true,
            "-h" | "--help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            // A bare `help` is still handled as the first word below, so a
            // session actually named "help" keeps working as an argument.
            other if other.starts_with("--") => {
                return Err(format!("unknown option: {}", other));
            }
            other => words.push(other),
        }
    }

    let (first, rest) = match words.split_first() {
        Some((first, rest)) => (*first, rest),
        None => return Ok(Cmd::Attach { name: None, wsl }),
    };
    let name = rest.first().map(|s| s.to_string());

    Ok(match first {
        "-h" | "help" => Cmd::Help,
        "ls" | "l" | "list" => Cmd::List,
        "a" | "attach" | "c" | "continue" => Cmd::Attach { name, wsl },
        "n" | "new" | "start" => match name {
            Some(name) => Cmd::Start { name, wsl },
            None => return Err(USAGE.to_string()),
        },
        "d" | "detach" => Cmd::Detach { name },
        "k" | "kill" | "rm" | "stop" => Cmd::Stop { name },
        // Anything else is a session name.
        other => Cmd::Attach {
            name: Some(other.to_string()),
            wsl,
        },
    })
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match parse(&args) {
        Ok(cmd) => cmd,
        Err(msg) => {
            eprintln!("{}", msg);
            std::process::exit(1);
        }
    };

    match cmd {
        Cmd::Help => {
            println!("{}", USAGE);
            Ok(())
        }
        Cmd::Version => {
            println!("{} (instance {})", env!("CARGO_PKG_VERSION"), protocol::instance());
            Ok(())
        }
        Cmd::List => {
            let resp = request(Op::List, "")?;
            if resp.ok {
                if let Some(d) = resp.data {
                    for info in d.as_array().cloned().unwrap_or_default() {
                        let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let attached = info
                            .get("attached")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        println!("{} {}", if attached { "*" } else { " " }, name);
                    }
                }
            } else {
                eprintln!("list failed: {}", resp.error);
            }
            Ok(())
        }
        Cmd::Attach { name, wsl } => {
            let name = resolve(name)?;
            attach_or_create(&name, wsl)
        }
        Cmd::Start { name, wsl } => {
            let (cols, rows) = current_size();
            let resp = request_sized(Op::Start, &name, Some(wsl), cols, rows)?;
            if !resp.ok {
                eprintln!("start failed: {}", resp.error);
                std::process::exit(1);
            }
            attach_session(&name, &resp, false)
        }
        Cmd::Detach { name } => {
            let name = resolve(name)?;
            control(Op::Detach, &name, "detach")?;
            println!("detached");
            Ok(())
        }
        Cmd::Stop { name } => {
            let name = resolve(name)?;
            control(Op::Stop, &name, "stop")?;
            println!("stopped");
            Ok(())
        }
        Cmd::ClipboardImage { stream } => clipboard_image(stream),
    }
}

/// Upper bound on one staged frame (64 MiB).
const MAX_PASTE_FRAME: usize = 64 * 1024 * 1024;

/// Read frames from stdin (4-byte big-endian length + BMP) and stage the latest
/// one for the daemon to publish on the clipboard.
///
/// Runs in its own SSH session and never talks to the daemon: it only writes
/// the shared staging file, which `Op::PasteImage` later reads.
fn clipboard_image(stream: bool) -> Result<()> {
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    loop {
        let Some(frame) = read_paste_frame(&mut input)? else {
            if stream {
                return Ok(());
            }
            anyhow::bail!("no image frame received on stdin");
        };
        write_staged_image(&frame)?;
        if !stream {
            return Ok(());
        }
    }
}

/// Read one length-prefixed frame. `Ok(None)` means a clean EOF before a new
/// frame started.
fn read_paste_frame<R: Read>(input: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    let mut filled = 0;
    while filled < len_buf.len() {
        match input.read(&mut len_buf[filled..]) {
            Ok(0) if filled == 0 => return Ok(None),
            Ok(0) => anyhow::bail!("truncated frame length on stdin"),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("failed to read frame length from stdin"),
        }
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_PASTE_FRAME {
        anyhow::bail!(
            "paste frame too large: {} bytes (limit {} bytes)",
            len,
            MAX_PASTE_FRAME
        );
    }
    if len == 0 {
        anyhow::bail!("empty paste frame");
    }

    let mut frame = vec![0u8; len];
    input
        .read_exact(&mut frame)
        .context("failed to read paste frame body from stdin")?;
    Ok(Some(frame))
}

/// Validate a frame and replace the staging file atomically.
fn write_staged_image(frame: &[u8]) -> Result<()> {
    if frame.len() < 2 || &frame[0..2] != b"BM" {
        anyhow::bail!("paste frame is not a BMP");
    }
    let path = shell_manager::clipboard::stage_path();
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid staging path"))?;
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create staging directory {}", dir.display()))?;

    // Write beside the target and rename: the daemon then never reads a
    // half-written image.
    let tmp = dir.join(format!("latest.tmp.{}", std::process::id()));
    std::fs::write(&tmp, frame)
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("failed to publish {}", path.display()))?;
    Ok(())
}

fn request(op: Op, name: &str) -> Result<protocol::Response> {
    request_sized(op, name, None, None, None)
}

fn request_sized(
    op: Op,
    name: &str,
    wsl: Option<bool>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<protocol::Response> {
    let req = Request {
        op,
        name: name.to_string(),
        wsl,
        cols,
        rows,
    };
    protocol::pipe_request(protocol::control_pipe(), &req)
}

fn control(op: Op, name: &str, action: &str) -> Result<()> {
    let resp = request(op, name)?;
    if !resp.ok {
        eprintln!("{} failed: {}", action, resp.error);
        std::process::exit(1);
    }
    Ok(())
}

/// Name given on the command line, or the session used most recently.
fn resolve(name: Option<String>) -> Result<String> {
    if let Some(name) = name {
        return Ok(name);
    }
    let resp = request(Op::Last, "")?;
    if let Some(name) = resp
        .data
        .as_ref()
        .and_then(|d| d.get("name"))
        .and_then(|v| v.as_str())
    {
        return Ok(name.to_string());
    }
    eprintln!("no session yet; run `sm <name>`");
    std::process::exit(1);
}

fn attach_or_create(name: &str, wsl: bool) -> Result<()> {
    let (cols, rows) = current_size();
    let resp = request_sized(Op::Continue, name, None, cols, rows)?;
    if resp.ok {
        return attach_session(name, &resp, true);
    }

    // A session that is gone (never existed, or its shell exited) is created on
    // the fly; any other error is reported as-is.
    let missing = resp.error.contains("not found") || resp.error.contains("has exited");
    if !missing {
        eprintln!("attach failed: {}", resp.error);
        std::process::exit(1);
    }

    let started = request_sized(Op::Start, name, Some(wsl), cols, rows)?;
    if !started.ok {
        // Two clients racing to create the same name: the loser sees "already
        // exists" because the winner just created it. Attach to that session
        // instead of failing.
        if started.error.contains("already exists") {
            let retried = request_sized(Op::Continue, name, None, cols, rows)?;
            if retried.ok {
                return attach_session(name, &retried, true);
            }
            eprintln!("attach failed: {}", retried.error);
            std::process::exit(1);
        }
        eprintln!("start failed: {}", started.error);
        std::process::exit(1);
    }
    attach_session(name, &started, false)
}

/// `repaint` is true when reattaching to an existing session: its screen was
/// being drained while detached, so the app must redraw. A fresh session has
/// just been created and needs no size toggle (which can disturb a TUI that is
/// starting up).
fn attach_session(name: &str, resp: &protocol::Response, repaint: bool) -> Result<()> {
    let (pipe_out, pipe_in, mouse) = extract_pipes(&resp.data);
    if let Err(e) = attach(name, &pipe_out, &pipe_in, mouse, repaint) {
        eprintln!(
            "attach failed: {:#}\nhint: if another window is attached to '{}', run `sm detach {}` there",
            e, name, name
        );
        std::process::exit(1);
    }
    Ok(())
}

fn extract_pipes(data: &Option<serde_json::Value>) -> (String, String, u8) {
    let out = data
        .as_ref()
        .and_then(|d| d.get("pipe_out"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let inp = data
        .as_ref()
        .and_then(|d| d.get("pipe_in"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mouse = data
        .as_ref()
        .and_then(|d| d.get("mouse"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u8;
    (out, inp, mouse)
}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Write the whole buffer to a handle. Returns false on error.
fn write_all(h: HANDLE, data: &[u8]) -> Result<bool> {
    let mut pos = 0;
    while pos < data.len() {
        let mut written: u32 = 0;
        let r = unsafe { WriteFile(h, Some(&data[pos..]), Some(&mut written), None) };
        match r {
            Ok(()) if written > 0 => pos += written as usize,
            Ok(()) => return Ok(false),
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

/// Length of the prefix consisting of complete UTF-8 sequences.
/// A trailing incomplete sequence (split across reads) is excluded.
fn utf8_complete_len(b: &[u8]) -> usize {
    match std::str::from_utf8(b) {
        Ok(_) => b.len(),
        Err(e) => {
            if e.error_len().is_none() {
                e.valid_up_to()
            } else {
                // Genuinely invalid bytes: forward as-is.
                b.len()
            }
        }
    }
}

/// Current console window size as (columns, rows), if stdout is a console.
fn current_size() -> (Option<u16>, Option<u16>) {
    unsafe {
        let h = match GetStdHandle(STD_OUTPUT_HANDLE) {
            Ok(h) => h,
            Err(_) => return (None, None),
        };
        let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
        if GetConsoleScreenBufferInfo(h, &mut info).is_ok() {
            let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
            (Some(cols), Some(rows))
        } else {
            (None, None)
        }
    }
}

/// Force the hosted app to redraw its whole screen after attaching.
///
/// Apps redraw when the window size changes, so shrink the pseudo console by a
/// row and restore it. The pause between the two is essential: sent back to
/// back, the app can see only the final size and skip the redraw, which leaves
/// the last line of a TUI untouched - the screen then looks restored except for
/// its bottom row.
fn force_repaint(name: &str) {
    let (Some(cols), Some(rows)) = current_size() else {
        return;
    };
    if rows < 2 {
        return;
    }
    let resize = |rows: u16| {
        let req = Request {
            op: Op::Resize,
            name: name.to_string(),
            wsl: None,
            cols: Some(cols),
            rows: Some(rows),
        };
        // Failures are non-fatal: the session may be gone already.
        let _ = protocol::pipe_request(protocol::control_pipe(), &req);
    };
    resize(rows - 1);
    std::thread::sleep(std::time::Duration::from_millis(150));
    resize(rows);
}

/// Open one end of a session pipe with the required access.
///
/// The daemon creates a single pipe instance per direction, so a previous
/// client that is still tearing down makes CreateFile fail with ERROR_PIPE_BUSY.
/// Wait for the instance to become free instead of failing the attach.
///
/// The wait is deliberately short: two clients cannot share one session, so if
/// the instance stays busy another client is attached and waiting longer only
/// delays the clear failure (the caller prints the `sm detach` hint).
fn open_pipe(name: &str, access: u32) -> Result<HANDLE> {
    const ERROR_PIPE_BUSY: u32 = 0x8007_00E7;
    const BUSY_WAIT_MS: u32 = 500;
    const BUSY_ATTEMPTS: u32 = 6;
    let w = wide(name);

    for _ in 0..BUSY_ATTEMPTS {
        let h = unsafe {
            CreateFileW(
                PCWSTR::from_raw(w.as_ptr()),
                access,
                FILE_SHARE_MODE(0),
                None,
                OPEN_EXISTING,
                SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                None,
            )
        };
        match h {
            Ok(h) => return Ok(h),
            Err(e) if (e.code().0 as u32) == ERROR_PIPE_BUSY => unsafe {
                let _ = windows::Win32::System::Pipes::WaitNamedPipeW(
                    PCWSTR::from_raw(w.as_ptr()),
                    BUSY_WAIT_MS,
                );
            },
            Err(e) => anyhow::bail!("failed to connect to data pipe {}: {}", name, e),
        }
    }

    anyhow::bail!("timed out connecting to data pipe {}", name)
}

/// Bumped whenever the client's input handling changes, so a stale attach is
/// easy to spot in the banner.
const CLIENT_BUILD: &str = "client 2026-10-27b";

/// Appends a diagnostic line to %TEMP%\sm-client[-<instance>].log.
///
/// Always on and best-effort: it records events and byte counts, never key
/// content, so passwords or API keys typed into the session cannot end up on
/// disk. The tracked file size avoids a metadata() call on the key hot path.
fn client_log(msg: &str) {
    use std::io::Write as _;
    use std::sync::{Mutex, OnceLock};

    struct Log {
        file: Option<std::fs::File>,
        size: u64,
    }

    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    let slot = LOG.get_or_init(|| {
        let path = std::env::temp_dir().join(protocol::client_log_file());
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok();
        let size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map(|m| m.len())
            .unwrap_or(0);
        Mutex::new(Log { file, size })
    });
    if let Ok(mut guard) = slot.lock() {
        let Log { file, size } = &mut *guard;
        if let Some(file) = file.as_mut() {
            if *size > 5_000_000 {
                let _ = file.set_len(0);
                *size = 0;
            }
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let line = format!("{} {}\n", ms, msg);
            if file.write_all(line.as_bytes()).is_ok() {
                *size += line.len() as u64;
            }
        }
    }
}

/// Ctrl+' — Otty sends it as 0x1c (the FS control byte; Ctrl+\ produces the
/// same byte on a standard keyboard). It asks the daemon to publish the
/// client-staged image and then injects the app-level paste key; it is never
/// forwarded to the hosted program.
const PASTE_TRIGGER: u8 = 0x1c;

/// Name of the environment override for the trigger byte.
const PASTE_TRIGGER_ENV: &str = "SM_PASTE_TRIGGER";

/// Parse `0xNN` or a decimal byte value.
fn parse_paste_trigger(raw: &str) -> Option<u8> {
    let raw = raw.trim();
    let value = if let Some(hex) = raw.strip_prefix("0x").or_else(|| raw.strip_prefix("0X")) {
        u16::from_str_radix(hex, 16).ok()?
    } else {
        raw.parse::<u16>().ok()?
    };
    u8::try_from(value).ok()
}

/// Trigger byte from `SM_PASTE_TRIGGER`, falling back to `PASTE_TRIGGER`.
fn paste_trigger_byte() -> u8 {
    match std::env::var(PASTE_TRIGGER_ENV) {
        Ok(raw) => parse_paste_trigger(&raw).unwrap_or_else(|| {
            client_log(&format!(
                "ignoring invalid {}={:?}; using 0x{:02x}",
                PASTE_TRIGGER_ENV, raw, PASTE_TRIGGER
            ));
            PASTE_TRIGGER
        }),
        Err(_) => PASTE_TRIGGER,
    }
}

/// Claude Code's image-paste key on Windows (Alt+V, sent as ESC v).
const PASTE_KEY: &[u8] = b"\x1bv";

/// Split forwarded input on the image-paste trigger, dropping each trigger
/// byte. Every gap between segments means "paste the staged image".
fn split_paste_triggers(data: &[u8], trigger: u8) -> Vec<&[u8]> {
    data.split(|&b| b == trigger).collect()
}

/// Drop trigger bytes without acting on them (used when detach wins).
fn strip_paste_triggers(data: &[u8], trigger: u8) -> Vec<u8> {
    data.iter().copied().filter(|&b| b != trigger).collect()
}

/// A pasted clipboard image path written by Otty.
struct ImagePastePath {
    /// Client-side path, for logging only.
    path: String,
    /// Bytes that followed the path in the same read (for example a newline
    /// typed right after pasting). Forwarded after the paste key so the paste
    /// still lands in order.
    trailing: Vec<u8>,
}

/// Bracketed-paste wrappers. Otty wraps a Cmd+V image path in these markers,
/// and the markers may be split from the path across stdin reads.
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

/// Longest bracketed paste worth holding back. Anything larger cannot be a
/// single image path, so it is forwarded unchanged once it crosses this.
const MAX_BRACKETED_PASTE: usize = 4096;

/// Image extensions accepted for a pasted Otty path.
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "avif"];

/// Recognise input whose whole content is a client-side Otty image path.
///
/// Otty stores a pasted image in `$TMPDIR/otty-paste/` and sends the absolute
/// path wrapped in a bracketed paste. The start marker and the path can arrive
/// in different stdin reads; `BracketedPaste` reassembles them before this
/// check. A bare path is also accepted for terminals that do not wrap. `sm`
/// turns the path back into a real clipboard paste instead of forwarding it.
fn image_paste_path(data: &[u8]) -> Option<ImagePastePath> {
    // Everything is text: a stray non-UTF-8 byte means this is not a path.
    let text = std::str::from_utf8(data).ok()?;
    let (candidate, after) = if let Some(rest) = data.strip_prefix(BRACKETED_PASTE_START) {
        let end = rest
            .windows(BRACKETED_PASTE_END.len())
            .position(|w| w == BRACKETED_PASTE_END)?;
        let candidate = std::str::from_utf8(&rest[..end]).ok()?;
        (candidate, &rest[end + BRACKETED_PASTE_END.len()..])
    } else {
        (text, &[][..])
    };

    // Trim the trailing whitespace Otty or the user added after the path; those
    // bytes still have to reach the hosted program after the paste key.
    let token = candidate.trim_end_matches(|c| matches!(c, ' ' | '\t' | '\r' | '\n'));
    let trimmed = &candidate[token.len()..];
    let mut trailing = Vec::with_capacity(trimmed.len() + after.len());
    trailing.extend_from_slice(trimmed.as_bytes());
    trailing.extend_from_slice(after);

    // A shell may quote a pasted path. Only a matching pair is acceptable.
    let path = if let Some(inner) = token
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
    {
        inner
    } else if let Some(inner) = token
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
    {
        inner
    } else if token.contains(['\'', '"']) {
        return None;
    } else {
        token
    };

    if path.is_empty()
        || !path.starts_with('/')
        || path.chars().any(|c| c.is_whitespace() || c.is_control())
        || !path.contains("/otty-paste/")
    {
        return None;
    }
    let name = path.rsplit('/').next().unwrap_or("");
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if !IMAGE_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }

    Some(ImagePastePath {
        path: path.to_string(),
        trailing,
    })
}

/// Whether `data` contains the bracketed-paste end marker.
fn contains_bracketed_paste_end(data: &[u8]) -> bool {
    data.windows(BRACKETED_PASTE_END.len())
        .any(|window| window == BRACKETED_PASTE_END)
}

/// Reassembles a bracketed paste that arrives split across reads.
#[derive(Default)]
struct BracketedPaste {
    pending: Option<Vec<u8>>,
}

enum PasteStep {
    /// Not an incomplete bracketed paste; process this read normally.
    PassThrough,
    /// The read was buffered; nothing may be forwarded yet.
    Buffered,
    /// A complete paste (or one too large to be a path) to process as a whole.
    Complete(Vec<u8>),
}

impl BracketedPaste {
    /// Feed one stdin read, holding back a paste that has not ended yet.
    fn feed(&mut self, data: &[u8]) -> PasteStep {
        if self.pending.is_some() {
            let complete = {
                let pending = self.pending.as_mut().expect("pending paste");
                pending.extend_from_slice(data);
                contains_bracketed_paste_end(pending) || pending.len() > MAX_BRACKETED_PASTE
            };
            if complete {
                return PasteStep::Complete(self.pending.take().unwrap_or_default());
            }
            return PasteStep::Buffered;
        }

        // The start marker always arrives whole, so only a complete opening
        // marker is worth buffering.
        if data.starts_with(BRACKETED_PASTE_START) && !contains_bracketed_paste_end(data) {
            self.pending = Some(data.to_vec());
            return PasteStep::Buffered;
        }

        PasteStep::PassThrough
    }
}

/// Time to wait for a just-copied image to reach the host before the paste
/// key is injected. The client watcher polls every 200 ms.
const PASTE_STAGE_DELAY: Duration = Duration::from_millis(400);

/// Name of the environment override for the staging delay.
const PASTE_STAGE_DELAY_ENV: &str = "SM_PASTE_DELAY_MS";

/// Parse the staging delay in milliseconds, accepting 1 through 10_000.
fn parse_paste_delay(raw: &str) -> Option<Duration> {
    let ms: u64 = raw.trim().parse().ok()?;
    (1..=10_000).contains(&ms).then(|| Duration::from_millis(ms))
}

/// Staging delay from `SM_PASTE_DELAY_MS`, falling back to `PASTE_STAGE_DELAY`.
fn paste_stage_delay() -> Duration {
    match std::env::var(PASTE_STAGE_DELAY_ENV) {
        Ok(raw) => parse_paste_delay(&raw).unwrap_or_else(|| {
            client_log(&format!(
                "ignoring invalid {}={:?}; using {:?}",
                PASTE_STAGE_DELAY_ENV, raw, PASTE_STAGE_DELAY
            ));
            PASTE_STAGE_DELAY
        }),
        Err(_) => PASTE_STAGE_DELAY,
    }
}

/// Ask the daemon to publish the staged image on the clipboard.
fn request_paste_image(name: &str) -> bool {
    let req = Request {
        op: Op::PasteImage,
        name: name.to_string(),
        wsl: None,
        cols: None,
        rows: None,
    };
    match protocol::pipe_request(protocol::control_pipe(), &req) {
        Ok(resp) if resp.ok => true,
        Ok(resp) => {
            client_log(&format!("image paste failed: {}", resp.error));
            false
        }
        Err(e) => {
            client_log(&format!("image paste request failed: {:#}", e));
            false
        }
    }
}

/// Detach triggers. The printable sequence works from any keyboard and is not
/// consumed by ssh (unlike "~."); F12 and Ctrl+] are kept as extras.
/// Keys that detach the client. Ctrl+] is the control byte GS; F12 is the VT
/// sequence. A Win32-input-mode report for either is handled separately.
const DETACH_SEQS: &[&[u8]] = &[b"\x1b[24~", b"\x1d"];

/// Disable terminal mouse reporting. A TUI that exits without restoring its
/// mouse mode otherwise leaves the terminal sending SGR mouse reports, which
/// leak into the shell as typed garbage.
const MOUSE_RESET: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/// Leave the alternate screen buffer (all historical variants).
///
/// The client does not own the alternate screen for the attach. A hosted app
/// that wants one enters it itself, and forwarding that request keeps the
/// terminal's wheel and scrollback behaviour identical to a plain SSH session.
/// Staying on the primary screen is what gives apps such as Claude Code inside
/// WSL a scrollback; on the alternate screen the wheel became an Up/Down key
/// (`ESC O A/B`). The client still leaves the alternate buffer on attach and
/// detach so a stale or crashed app cannot leave the local terminal stuck on
/// it.
const LEAVE_ALT_SCREEN: &[u8] = b"\x1b[?1049l\x1b[?1047l\x1b[?47l";

/// Show the cursor and reset SGR attributes, restoring a usable local prompt.
const SHOW_CURSOR_RESET_SGR: &[u8] = b"\x1b[?25h\x1b[0m";

/// DECRST 9001: turn "Win32 input mode" off (Windows Terminal extension).
///
/// PowerShell 7 enables it via `CSI ? 9001 h`. A terminal that honours it stops
/// sending plain VT keystrokes and instead encodes every key as
/// `CSI Vk;Sc;Uc;Kd;Cs;Rc _`, which matches none of the detach sequences and
/// leaves the mouse filter nothing to recognise. The client therefore hides the
/// request from the terminal and switches the mode off on every attach.
const WIN32_INPUT_OFF: &[u8] = b"\x1b[?9001l";

/// DEC private mode switches the client strips from the PTY output.
///
/// 9001 is Win32 input mode: hiding it keeps the terminal sending plain VT
/// keystrokes. Alternate-screen modes (47/1047/1049) are deliberately forwarded:
/// the hosted app's own screen handling must reach the terminal, otherwise the
/// wheel and the scrollback behave differently from a plain SSH session.
fn is_client_mode(mode: u32) -> bool {
    mode == 9001
}

/// Remove client-owned modes from one `CSI ? params h|l` parameter list.
///
/// Returns `None` when the list contains no client-owned mode, so the sequence
/// can be forwarded untouched. Otherwise returns the remaining parameters,
/// which may be empty when every mode in the sequence belongs to the client.
fn without_client_modes(params: &[u8]) -> Option<Vec<u8>> {
    let mut kept: Vec<&[u8]> = Vec::new();
    let mut removed = false;
    for field in params.split(|&b| b == b';') {
        if field.is_empty() {
            kept.push(field);
            continue;
        }
        let Ok(text) = std::str::from_utf8(field) else {
            return None;
        };
        match text.parse::<u32>() {
            Ok(mode) if is_client_mode(mode) => removed = true,
            _ => kept.push(field),
        }
    }
    if !removed {
        return None;
    }
    let mut out = Vec::new();
    for (index, field) in kept.iter().enumerate() {
        if index > 0 {
            out.push(b';');
        }
        out.extend_from_slice(field);
    }
    Some(out)
}

/// Drop the DEC private mode switches the client owns from the PTY output.
///
/// Only `CSI ? ... h|l` sequences are inspected; a sequence that also carries
/// other modes is re-emitted without the owned ones, so the hosted app's mouse
/// settings still reach the terminal. A sequence split across reads is held in
/// `carry` until its final byte arrives.
fn strip_client_modes(data: &[u8], carry: &mut Vec<u8>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(carry.len() + data.len());
    buf.extend_from_slice(carry);
    buf.extend_from_slice(data);
    carry.clear();

    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != 0x1b {
            out.push(buf[i]);
            i += 1;
            continue;
        }
        let rest = &buf[i..];
        if rest.len() == 1 {
            // A trailing ESC is the start of a sequence split across reads. It
            // has no visible effect on its own, so holding it back cannot delay
            // any output, and it lets the next read recognise `ESC [` as a
            // whole - including a sequence that breaks right after the ESC.
            carry.extend_from_slice(rest);
            break;
        }
        if rest[1] != b'[' {
            out.push(buf[i]);
            i += 1;
            continue;
        }
        if rest.len() == 2 || rest[2] != b'?' {
            // `ESC [ X`: not a DEC private sequence. Hold only the `ESC [` that
            // may still grow into one.
            if rest.len() == 2 {
                carry.extend_from_slice(rest);
                break;
            }
            out.push(buf[i]);
            i += 1;
            continue;
        }

        let mut j = i + 3;
        while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
            j += 1;
        }
        if j >= buf.len() {
            // Final byte not seen yet.
            carry.extend_from_slice(&buf[i..]);
            break;
        }
        let final_byte = buf[j];
        if final_byte != b'h' && final_byte != b'l' {
            out.extend_from_slice(&buf[i..=j]);
            i = j + 1;
            continue;
        }
        match without_client_modes(&buf[i + 3..j]) {
            Some(kept) => {
                // All-empty parameters carry no meaning; drop the sequence.
                if kept.iter().any(|b| b.is_ascii_digit()) {
                    out.extend_from_slice(b"\x1b[?");
                    out.extend_from_slice(&kept);
                    out.push(final_byte);
                }
            }
            None => out.extend_from_slice(&buf[i..=j]),
        }
        i = j + 1;
    }
    out
}

/// Wait (bounded) for the reader thread to publish its thread id, then cancel
/// its blocking read until it reports done.
fn stop_reader(reader_tid: &AtomicU32, reader_done: &AtomicBool) {
    // The reader sets its id first thing, but the thread may not have been
    // scheduled yet.
    let mut waited = 0u32;
    while reader_tid.load(Ordering::Relaxed) == 0 && waited < 100 {
        std::thread::sleep(std::time::Duration::from_millis(10));
        waited += 1;
    }
    unsafe {
        use windows::Win32::System::IO::CancelSynchronousIo;
        use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
        // Retry until the reader has finished: the first cancellation may land
        // before it posts its read.
        while !reader_done.load(Ordering::Relaxed) {
            if let Ok(ht) = OpenThread(
                THREAD_TERMINATE,
                false,
                reader_tid.load(Ordering::Relaxed),
            ) {
                let _ = CancelSynchronousIo(ht);
                let _ = windows::Win32::Foundation::CloseHandle(ht);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}

/// Detects a detach key in the input stream, tolerating a sequence split
/// across reads.
struct DetachDetector {
    pending: Vec<u8>,
}

impl DetachDetector {
    fn new() -> Self {
        Self { pending: Vec::new() }
    }

    /// Returns the bytes to forward and whether detach was requested.
    fn feed(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        self.pending.extend_from_slice(data);

        let hit = DETACH_SEQS
            .iter()
            .filter_map(|seq| find_subslice(&self.pending, seq))
            .min();
        if let Some(pos) = hit {
            let forward = self.pending[..pos].to_vec();
            self.pending.clear();
            return (forward, true);
        }

        // Hold back a trailing partial sequence so a split key is still seen.
        let keep = DETACH_SEQS
            .iter()
            .map(|seq| partial_suffix(&self.pending, seq))
            .max()
            .unwrap_or(0);
        // A lone ESC is the Esc key, not the first byte of a split F12. Holding
        // it back would delay it until the next key arrives and turn "Esc, x"
        // into Alt+x; only multi-byte partial sequences are worth waiting for.
        let keep = if keep == 1 { 0 } else { keep };
        let split = self.pending.len() - keep;
        let forward = self.pending[..split].to_vec();
        self.pending.drain(..split);
        (forward, false)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Length of the longest suffix of `data` that is a prefix of `needle`.
fn partial_suffix(data: &[u8], needle: &[u8]) -> usize {
    let max = data.len().min(needle.len() - 1);
    (1..=max)
        .rev()
        .find(|&k| data.ends_with(&needle[..k]))
        .unwrap_or(0)
}

/// Recognises the detach keys inside Win32-input-mode key reports.
///
/// A terminal that honours `CSI ? 9001 h` sends every key as
/// `CSI Vk;Sc;Uc;Kd;Cs;Rc _` instead of plain bytes, so neither the control byte
/// nor the F12 sequence in `DETACH_SEQS` ever appears.
struct Win32Detach {
    /// Trailing bytes that may start a report split across reads.
    carry: Vec<u8>,
}

impl Win32Detach {
    fn new() -> Self {
        Self { carry: Vec::new() }
    }

    fn feed(&mut self, data: &[u8]) -> bool {
        let mut buf = Vec::with_capacity(self.carry.len() + data.len());
        buf.extend_from_slice(&self.carry);
        buf.extend_from_slice(data);
        self.carry.clear();

        let mut hit = false;
        let mut i = 0;
        while i + 1 < buf.len() {
            if buf[i] != 0x1b || buf[i + 1] != b'[' {
                i += 1;
                continue;
            }
            let mut j = i + 2;
            while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                j += 1;
            }
            if j >= buf.len() {
                // Incomplete report: keep it for the next read.
                self.carry.extend_from_slice(&buf[i..]);
                break;
            }
            if buf[j] == b'_' {
                hit |= is_detach_report(&buf[i + 2..j]);
                i = j + 1;
            } else {
                i += 1;
            }
        }
        hit
    }
}

/// `params` holds the text between `CSI` and the `_` terminator.
fn is_detach_report(params: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(params) else {
        return false;
    };
    let fields: Vec<u32> = text
        .split(';')
        .filter_map(|f| f.parse::<u32>().ok())
        .collect();
    // Vk;Sc;Uc;Kd;Cs;Rc
    if fields.len() != 6 {
        return false;
    }
    let (vk, uc, kd, cs) = (fields[0], fields[2], fields[3], fields[4]);
    if kd == 0 {
        return false; // key-up
    }
    const VK_F12: u32 = 0x7B;
    if vk == VK_F12 {
        return true;
    }
    // LEFT_CTRL_PRESSED | RIGHT_CTRL_PRESSED
    let ctrl = cs & (4 | 8) != 0;
    ctrl && (uc == 0x1d || uc == 0x5d)
}

/// Removes hover-only mouse motion reports (SGR and X10 encodings).
///
/// ConPTY asks the terminal for any-event mouse tracking whenever mouse input
/// is enabled, so simply moving the pointer at a plain shell prompt would type
/// escape sequences into it. Clicks, wheel and drag events still pass through
/// for the hosted TUI.
fn filter_motion(data: &[u8], pending: &mut Vec<u8>) -> Vec<u8> {
    let mut out = Vec::new();
    // Never let a malformed/truncated report stall all keystrokes.
    if pending.len() > 256 {
        out.append(pending);
    }
    let mut buf = std::mem::take(pending);
    buf.extend_from_slice(data);
    let mut i = 0;

    while i < buf.len() {
        if i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'<' {
            // SGR: ESC [ < button ; x ; y M|m
            let rest = &buf[i + 3..];
            let end = match rest.iter().position(|&c| c == b'M' || c == b'm') {
                Some(end) => end,
                None => {
                    pending.extend_from_slice(&buf[i..]);
                    return out;
                }
            };
            let button = rest[..end]
                .split(|&c| c == b';')
                .next()
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse::<u32>().ok());
            let hover = matches!(button, Some(b) if (b & 0x20) != 0 && (b & 0x03) == 3);
            if !hover {
                out.extend_from_slice(&buf[i..i + 3 + end + 1]);
            }
            i += 3 + end + 1;
            continue;
        }
        if i + 2 < buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' && buf[i + 2] == b'M' {
            // X10: ESC [ M button x y
            if i + 6 > buf.len() {
                pending.extend_from_slice(&buf[i..]);
                return out;
            }
            let button = buf[i + 3].wrapping_sub(32);
            let hover = (button & 0x20) != 0 && (button & 0x03) == 3;
            if !hover {
                out.extend_from_slice(&buf[i..i + 6]);
            }
            i += 6;
            continue;
        }
        // Hold back only a possible mouse-report prefix; a lone ESC is a normal
        // key (vim) and must not be delayed.
        if i + 2 == buf.len() && buf[i] == 0x1b && buf[i + 1] == b'[' {
            pending.extend_from_slice(&buf[i..]);
            return out;
        }
        out.push(buf[i]);
        i += 1;
    }

    out
}

fn attach(name: &str, pipe_out: &str, pipe_in: &str, mouse: u8, repaint: bool) -> Result<()> {
    let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
    let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE)? };
    let mut orig_in_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let mut orig_out_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let has_console = unsafe {
        GetConsoleMode(stdin, &mut orig_in_mode).is_ok()
            && GetConsoleMode(stdout, &mut orig_out_mode).is_ok()
    };
    // Mouse modes the hosted app had enabled when this client attached.
    let initial_mouse_on = mouse & 0b111 != 0;
    let (cols, rows) = current_size();
    let size = match (cols, rows) {
        (Some(c), Some(r)) => format!("{}x{}", c, r),
        _ => "unknown".to_string(),
    };

    client_log(&format!(
        "attach build={} pid={} has_console={} size={} in_mode={:#x} out_mode={:#x} mouse={} repaint={} stdin={} stdout={}",
        CLIENT_BUILD,
        std::process::id(),
        has_console,
        size,
        orig_in_mode.0,
        orig_out_mode.0,
        mouse,
        repaint,
        stdin.0,
        stdout.0,
    ));

    // The PTY stream is UTF-8. A Windows console interprets WriteFile bytes with
    // the active output code page, so switch it to UTF-8 (65001) for correct text.
    let orig_in_cp = unsafe { GetConsoleCP() };
    let orig_out_cp = unsafe { GetConsoleOutputCP() };
    if has_console {
        unsafe {
            let _ = SetConsoleCP(65001);
            let _ = SetConsoleOutputCP(65001);
        }
    } else {
        eprintln!("[sm] no console detected; forwarding without raw mode");
    }

    // Attaching silently keeps a running TUI's screen intact: any line printed
    // here would scroll it and lose the bottom row. The client log records the
    // attach instead, and the detach notice carries the build id.
    let _ = write_all(stdout, b"");

    // Two unidirectional handles: out is read-only, in is write-only.
    // Byte-type pipes use byte read mode by default, so no SetNamedPipeHandleState
    // is needed (it would require extra access rights on a read-only handle).
    let h_out = open_pipe(pipe_out, GENERIC_READ.0)?;
    let h_in = open_pipe(pipe_in, GENERIC_WRITE.0)?;

    // A reattach needs a forced full-screen redraw: while detached the app kept
    // running against a drained output pipe. A freshly created session must not
    // be resized here - the toggle can race a TUI that is starting up and leave
    // its bottom rows overwritten.
    if repaint {
        let repaint_name = name.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            force_repaint(&repaint_name);
        });
    }

    // Put the local console into raw mode so input passes through byte-for-byte.
    let mut raw_in_mode = CONSOLE_MODE(0);
    // Whether VT output could be enabled; ESC sequences may only be written when
    // it succeeded, otherwise they are rendered as literal garbage. Declared out
    // here so the teardown path can consult it too.
    let mut vt_out_ok = false;
    if has_console {
        // Raw input plus VT input, so arrows and function keys arrive as VT
        // sequences. Mouse input is enabled only when the hosted app had it on
        // before this client attached; the reader thread switches it on and off
        // as the app toggles its DEC modes. Hover-only motion reports are
        // filtered out below so they do not get typed into a plain shell prompt.
        // ENABLE_EXTENDED_FLAGS is required for QUICK_EDIT_MODE to take effect.
        // ENABLE_MOUSE_INPUT is cleared from the base mode: a fresh console has
        // it on by default, so leaving it here would keep mouse reporting enabled
        // even when the hosted app never asked for it (and a crash of this client
        // would leave the bit stuck on). It is added back explicitly only when
        // initial_mouse_on, or at runtime when the app turns a DEC mode on.
        let raw_in = (orig_in_mode.0
            | ENABLE_EXTENDED_FLAGS.0
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0)
            & !(ENABLE_LINE_INPUT.0
                | ENABLE_ECHO_INPUT.0
                | ENABLE_PROCESSED_INPUT.0
                | ENABLE_WINDOW_INPUT.0
                | ENABLE_MOUSE_INPUT.0
                | ENABLE_QUICK_EDIT_MODE.0);
        raw_in_mode = CONSOLE_MODE(raw_in);
        let initial_mode = if initial_mouse_on {
            CONSOLE_MODE(raw_in | ENABLE_MOUSE_INPUT.0)
        } else {
            raw_in_mode
        };
        // Processed output + VT rendering. DISABLE_NEWLINE_AUTO_RETURN makes the
        // console treat LF like a real terminal (no implicit carriage return).
        let raw_out = orig_out_mode.0
            | ENABLE_PROCESSED_OUTPUT.0
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0
            | DISABLE_NEWLINE_AUTO_RETURN.0;
        vt_out_ok = unsafe {
            if SetConsoleMode(stdin, initial_mode).is_err() {
                // VT input unsupported on this console: fall back to plain raw mode.
                let fallback = (orig_in_mode.0 | ENABLE_EXTENDED_FLAGS.0)
                    & !(ENABLE_LINE_INPUT.0
                        | ENABLE_ECHO_INPUT.0
                        | ENABLE_PROCESSED_INPUT.0
                        | ENABLE_WINDOW_INPUT.0
                        | ENABLE_MOUSE_INPUT.0
                        | ENABLE_QUICK_EDIT_MODE.0);
                let _ = SetConsoleMode(stdin, CONSOLE_MODE(fallback));
            }
            // VT output is best-effort (older consoles may reject the flag).
            SetConsoleMode(stdout, CONSOLE_MODE(raw_out)).is_ok()
        };
        if vt_out_ok {
            // These are VT sequences: written before VT output was enabled they
            // would show up as literal garbage on screen. Start on the primary
            // screen (a hosted app that wants the alternate buffer asks for it
            // itself), then restore the mouse modes it had enabled before this
            // client attached.
            let _ = write_all(stdout, LEAVE_ALT_SCREEN);
            let _ = write_all(stdout, WIN32_INPUT_OFF);
            let _ = write_all(stdout, MOUSE_RESET);
            if initial_mouse_on {
                let mut seq = Vec::new();
                if mouse & 1 != 0 {
                    seq.extend_from_slice(b"\x1b[?1000h");
                }
                if mouse & 2 != 0 {
                    seq.extend_from_slice(b"\x1b[?1002h");
                }
                if mouse & 4 != 0 {
                    seq.extend_from_slice(b"\x1b[?1003h");
                }
                if mouse & 8 != 0 {
                    seq.extend_from_slice(b"\x1b[?1006h");
                }
                let _ = write_all(stdout, &seq);
            }
        }
    }

    struct Guard {
        active: bool,
        stdin: HANDLE,
        stdout: HANDLE,
        in_mode: CONSOLE_MODE,
        out_mode: CONSOLE_MODE,
        in_cp: u32,
        out_cp: u32,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.active {
                unsafe {
                    let _ = SetConsoleMode(self.stdin, self.in_mode);
                    let _ = SetConsoleMode(self.stdout, self.out_mode);
                    let _ = SetConsoleCP(self.in_cp);
                    let _ = SetConsoleOutputCP(self.out_cp);
                }
            }
        }
    }
    let _guard = Guard {
        active: has_console,
        stdin,
        stdout,
        in_mode: orig_in_mode,
        out_mode: orig_out_mode,
        in_cp: orig_in_cp,
        out_cp: orig_out_cp,
    };

    // Keep the hosted PTY in sync with the local console size: TUI apps need the
    // resize event, and it is only reported to us as a changed console window size.
    if has_console {
        let resize_name = name.to_string();
        std::thread::spawn(move || {
            let mut last = current_size();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                let cur = current_size();
                if cur == last || cur.0.is_none() || cur.1.is_none() {
                    continue;
                }
                client_log(&format!(
                    "resize {}x{} -> {}x{}",
                    last.0.unwrap_or(0),
                    last.1.unwrap_or(0),
                    cur.0.unwrap_or(0),
                    cur.1.unwrap_or(0)
                ));
                last = cur;
                let req = Request {
                    op: Op::Resize,
                    name: resize_name.clone(),
                    wsl: None,
                    cols: cur.0,
                    rows: cur.1,
                };
                // Failures are non-fatal: the session may be gone already.
                let _ = protocol::pipe_request(protocol::control_pipe(), &req);
            }
        });
    }

    // The reader thread blocks on the out pipe, so the stdin thread needs a way
    // to cancel that read when the user detaches with "~.". It publishes its
    // thread id and a done flag for that handshake.
    let reader_done = Arc::new(AtomicBool::new(false));
    let reader_tid = Arc::new(AtomicU32::new(0));

    let stdin_done = reader_done.clone();
    let stdin_reader_tid = reader_tid.clone();
    let paste_name = name.to_string();
    let paste_trigger = paste_trigger_byte();
    let paste_delay = paste_stage_delay();
    client_log(&format!("image paste trigger byte 0x{:02x}", paste_trigger));

    let stdin_thread = std::thread::spawn(move || -> Result<()> {
        let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
        let mut buf = [0u8; 4096];
        let mut detector = DetachDetector::new();
        let mut motion_pending: Vec<u8> = Vec::new();
        let mut win32 = Win32Detach::new();
        let mut bracketed = BracketedPaste::default();
        let mut hover_reads: u32 = 0;
        client_log("stdin thread started");
        loop {
            let mut read: u32 = 0;
            let r = unsafe {
                ReadFile(
                    stdin_handle,
                    Some(&mut buf),
                    Some(&mut read),
                    None,
                )
            };
            match r {
                Ok(()) if read == 0 => {
                    client_log("stdin EOF");
                    break;
                }
                Ok(()) => {
                    let raw = &buf[..read as usize];
                    // A terminal in Win32 input mode encodes keys as reports, so
                    // the triggers have to be looked for there as well.
                    let win32_hit = win32.feed(raw);
                    let filtered = filter_motion(raw, &mut motion_pending);
                    let data = &filtered[..];
                    if data.is_empty() && !win32_hit {
                        hover_reads += 1;
                        if hover_reads % 100 == 0 {
                            client_log(&format!("hover reports dropped so far: {}", hover_reads));
                        }
                        continue;
                    }
                    if !data.is_empty() {
                        client_log(&format!(
                            "stdin read {} bytes, forwarding {} bytes",
                            read,
                            data.len()
                        ));
                    }
                    let (forward, detach) = detector.feed(data);
                    if detach || win32_hit {
                        client_log(if detach {
                            "detach matched"
                        } else {
                            "detach matched (win32 report)"
                        });
                        // Forward whatever preceded the key on this read before
                        // tearing the connection down. Detach wins over image
                        // paste, so any trigger in the prefix is dropped rather
                        // than acted on or forwarded.
                        let prefix = strip_paste_triggers(&forward, paste_trigger);
                        if !prefix.is_empty() {
                            let _ = write_all(h_in, &prefix);
                        }
                        // Unblock the reader thread's ReadFile on the out pipe.
                        stop_reader(&stdin_reader_tid, &stdin_done);
                        break;
                    }
                    // Otty sends a Cmd+V image path as a bracketed paste, and
                    // the closing marker usually lands in the next read. Hold
                    // the opening marker back until the paste is complete, then
                    // process the reassembled block below.
                    let payload: Cow<[u8]> = match bracketed.feed(&forward) {
                        PasteStep::Buffered => {
                            client_log("bracketed paste: waiting for end marker");
                            continue;
                        }
                        PasteStep::Complete(buf) => Cow::Owned(buf),
                        PasteStep::PassThrough => Cow::Borrowed(&forward[..]),
                    };
                    // A whole-read Otty image path (Cmd+V) is turned into a
                    // real paste. If the watcher has not delivered an image
                    // yet, the path is forwarded as ordinary text instead.
                    if let Some(paste) = image_paste_path(&payload) {
                        if shell_manager::clipboard::stage_path().exists() {
                            client_log(&format!(
                                "image path paste: {} (waiting {:?} for staging)",
                                paste.path, paste_delay
                            ));
                            std::thread::sleep(paste_delay);
                            client_log("image paste triggered (path)");
                            let _ = request_paste_image(&paste_name);
                            // Inject the paste key even when the clipboard
                            // update failed, so the failure stays visible
                            // instead of swallowing the user's action.
                            if !write_all(h_in, PASTE_KEY)? {
                                client_log("stdin forward failed");
                                return Ok(());
                            }
                            if !paste.trailing.is_empty()
                                && !write_all(h_in, &paste.trailing)?
                            {
                                client_log("stdin forward failed");
                                return Ok(());
                            }
                            continue;
                        }
                        client_log(
                            "image path paste seen but no staged image; forwarding path text",
                        );
                    }
                    // Ctrl+' arrives as the raw 0x1c byte. The Win32-input-mode
                    // report path above is deliberately not used for image
                    // paste: Otty (the paste bridge's terminal) does not
                    // implement CSI ? 9001 h, so keys are never encoded as
                    // reports.
                    let parts = split_paste_triggers(&payload, paste_trigger);
                    for (index, part) in parts.iter().enumerate() {
                        if !part.is_empty() && !write_all(h_in, part)? {
                            client_log("stdin forward failed");
                            return Ok(());
                        }
                        // A gap means a trigger was removed between `part` and
                        // the next segment. A single read may contain several.
                        if index + 1 < parts.len() {
                            client_log("image paste triggered");
                            let _ = request_paste_image(&paste_name);
                            // Inject the app-level paste key even when the
                            // clipboard update failed, so the failure stays
                            // visible instead of swallowing the keystroke.
                            if !write_all(h_in, PASTE_KEY)? {
                                client_log("stdin forward failed");
                                return Ok(());
                            }
                        }
                    }
                }
                Err(e) => {
                    client_log(&format!("stdin read error: {}", e));
                    break;
                }
            }
        }
        client_log("stdin thread exiting");
        Ok(())
    });

    // Reader thread: out pipe -> stdout. Keeping this off the main thread means
    // the cancel above only ever targets the blocking ReadFile here, never a
    // console write performed by the teardown path.
    let reader = std::thread::spawn(move || {
        unsafe {
            reader_tid.store(
                windows::Win32::System::Threading::GetCurrentThreadId(),
                Ordering::Relaxed,
            );
        }
        let mut buf = [0u8; 4096];
        let mut carry: Vec<u8> = Vec::new();
        // Mouse modes the hosted app has enabled, tracked from its output.
        let mut mouse_state = mouse;
        let mut mouse_on = mouse & 0b111 != 0;
        let mut mouse_carry: Vec<u8> = Vec::new();
        // Holds a split client-owned mode sequence between reads; see strip_client_modes.
        let mut mode_carry: Vec<u8> = Vec::new();
        loop {
            let mut read: u32 = 0;
            let r = unsafe {
                ReadFile(
                    h_out,
                    Some(&mut buf),
                    Some(&mut read),
                    None,
                )
            };
            match r {
                Ok(()) if read == 0 => {
                    client_log("out pipe EOF");
                    break;
                }
                Ok(()) => {
                    let chunk = &buf[..read as usize];

                    // Follow the app's DEC mouse modes: enable mouse input only
                    // while it actually wants mouse, so conhost does not ask the
                    // terminal for reports that nobody consumes.
                    if has_console {
                        let mut scan = Vec::with_capacity(mouse_carry.len() + chunk.len());
                        scan.extend_from_slice(&mouse_carry);
                        scan.extend_from_slice(chunk);
                        for_each_mouse_mode(&scan, |mode, on| {
                            if let Some(bit) = mouse_bit(mode) {
                                if on {
                                    mouse_state |= bit;
                                } else {
                                    mouse_state &= !bit;
                                }
                            }
                        });
                        mouse_carry.clear();
                        mouse_carry.extend_from_slice(mouse_carry_tail(&scan));

                        let now = mouse_state & 0b111 != 0;
                        if now != mouse_on {
                            mouse_on = now;
                            let mode = if now {
                                CONSOLE_MODE(raw_in_mode.0 | ENABLE_MOUSE_INPUT.0)
                            } else {
                                raw_in_mode
                            };
                            unsafe {
                                let _ = SetConsoleMode(stdin, mode);
                            }
                        }
                    }

                    let data = if has_console {
                        // The console renders WriteFile bytes using its code page
                        // (set to UTF-8 above). Hold back an incomplete trailing
                        // UTF-8 sequence so multibyte characters split across reads
                        // are not decoded as garbage.
                        let mut joined = Vec::with_capacity(carry.len() + chunk.len());
                        joined.extend_from_slice(&carry);
                        joined.extend_from_slice(chunk);
                        let valid = utf8_complete_len(&joined);
                        carry.clear();
                        carry.extend_from_slice(&joined[valid..]);
                        joined[..valid].to_vec()
                    } else {
                        chunk.to_vec()
                    };
                    // Keep the terminal out of Win32 input mode, otherwise keys
                    // arrive as CSI reports instead of plain VT bytes.
                    let data = strip_client_modes(&data, &mut mode_carry);
                    if !data.is_empty() {
                        // A cancelled console write must only end this thread,
                        // never panic the process.
                        match write_all(stdout, &data) {
                            Ok(true) => {}
                            _ => {
                                client_log("stdout write failed");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    client_log(&format!("out read error: {}", e));
                    break;
                }
            }
        }
        client_log("reader thread exiting");
        reader_done.store(true, Ordering::Relaxed);
    });

    let _ = reader.join();

    // Do NOT join the stdin thread: in raw mode it may be blocked in a console
    // read that never returns. Returning from main terminates the process.
    drop(stdin_thread);
    // Leave the terminal clean: disable mouse reporting the hosted TUI may have
    // enabled, so stray reports do not get typed into the shell after detach.
    // Only when VT output was actually enabled in the first place: otherwise the
    // sequence would be printed as literal text.
    if has_console && vt_out_ok {
        let _ = write_all(stdout, MOUSE_RESET);
        // The client held the local terminal on the alternate buffer for the
        // whole attach. Leaving it restores the parent shell's screen and
        // cursor, so the detach notice below lands on the normal buffer.
        let _ = write_all(stdout, LEAVE_ALT_SCREEN);
        let _ = write_all(stdout, SHOW_CURSOR_RESET_SGR);
    }
    // Plain write instead of println!: a console write racing with teardown must
    // not panic the client with "failed printing to stdout".
    let _ = write_all(stdout, format!("\r\ndetached ({})\r\n", CLIENT_BUILD).as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        filter_motion, image_paste_path, parse_paste_delay, parse_paste_trigger,
        read_paste_frame, split_paste_triggers, strip_client_modes, strip_paste_triggers,
        BracketedPaste, DetachDetector, PasteStep, Win32Detach, BRACKETED_PASTE_START,
        MAX_BRACKETED_PASTE, PASTE_TRIGGER,
    };
    use std::io::Cursor;
    use std::time::Duration;

    #[test]
    fn f12_detaches() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"\x1b[24~");
        assert!(detach);
        assert!(out.is_empty());
    }

    #[test]
    fn f12_after_text_forwards_the_prefix() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"abc\x1b[24~");
        assert!(detach);
        assert_eq!(&out[..], b"abc");
    }

    #[test]
    fn f12_split_across_reads() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"ab\x1b[2");
        assert!(!detach);
        assert_eq!(&out[..], b"ab");
        let (out, detach) = d.feed(b"4~");
        assert!(detach);
        assert!(out.is_empty());
    }

    #[test]
    fn lone_escape_is_forwarded_immediately() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"\x1b");
        assert!(!detach);
        assert_eq!(&out[..], b"\x1b");
        // The next key must arrive on its own, not as Alt+<key>.
        let (out, detach) = d.feed(b"x");
        assert!(!detach);
        assert_eq!(&out[..], b"x");
    }

    #[test]
    fn ctrl_bracket_right_still_detaches() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"xy\x1d");
        assert!(detach);
        assert_eq!(&out[..], b"xy");
    }

    #[test]
    fn other_escape_sequences_are_forwarded() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"\x1b[A\x1b[24");
        assert!(!detach);
        // The trailing partial F12 prefix is held for the next read.
        assert_eq!(&out[..], b"\x1b[A");
        let (out, detach) = d.feed(b"~");
        assert!(detach);
        assert!(out.is_empty());
    }

    #[test]
    fn modified_f12_is_forwarded() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"\x1b[24;2~");
        assert!(!detach);
        assert_eq!(&out[..], b"\x1b[24;2~");
    }

    #[test]
    fn plain_text_is_forwarded_unchanged() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"cd ~/x\r");
        assert!(!detach);
        assert_eq!(&out[..], b"cd ~/x\r");
    }

    #[test]
    fn hover_reports_are_filtered() {
        let mut p = Vec::new();
        let out = filter_motion(b"a\x1b[<35;1;28Mb", &mut p);
        assert_eq!(&out[..], b"ab");
    }

    #[test]
    fn clicks_wheel_and_drag_pass() {
        let mut p = Vec::new();
        let out = filter_motion(b"\x1b[<0;1;28M\x1b[<64;1;28M\x1b[<32;1;28M", &mut p);
        assert_eq!(&out[..], b"\x1b[<0;1;28M\x1b[<64;1;28M\x1b[<32;1;28M");
    }

    #[test]
    fn split_hover_report_is_dropped() {
        let mut p = Vec::new();
        assert!(filter_motion(b"\x1b[<35;1", &mut p).is_empty());
        assert!(filter_motion(b";28M", &mut p).is_empty());
        assert!(p.is_empty());
    }

    #[test]
    fn x10_hover_is_filtered() {
        let mut p = Vec::new();
        assert!(filter_motion(b"\x1b[M\x43\x21\x3c", &mut p).is_empty());
    }

    #[test]
    fn lone_escape_is_forwarded() {
        let mut p = Vec::new();
        assert_eq!(&filter_motion(b"\x1b", &mut p)[..], b"\x1b");
    }

    #[test]
    fn win32_input_mode_request_is_dropped() {
        let mut carry = Vec::new();
        assert_eq!(
            &strip_client_modes(b"a\x1b[?9001hb", &mut carry)[..],
            b"ab"
        );
        assert!(carry.is_empty());
    }

    #[test]
    fn win32_input_mode_sequence_split_across_reads() {
        let mut carry = Vec::new();
        assert_eq!(&strip_client_modes(b"a\x1b[?90", &mut carry)[..], b"a");
        assert_eq!(&strip_client_modes(b"01hb", &mut carry)[..], b"b");
        assert!(carry.is_empty());
    }

    #[test]
    fn win32_input_mode_sequence_split_at_final_byte() {
        let mut carry = Vec::new();
        assert_eq!(&strip_client_modes(b"\x1b[?9001", &mut carry)[..], b"");
        assert_eq!(&strip_client_modes(b"l", &mut carry)[..], b"");
        assert!(carry.is_empty());
    }

    #[test]
    fn other_private_modes_survive_stripping() {
        let mut carry = Vec::new();
        let out = strip_client_modes(b"\x1b[?1003h\x1b[?9001l\x1b[?25l", &mut carry);
        assert_eq!(&out[..], b"\x1b[?1003h\x1b[?25l");
    }

    #[test]
    fn trailing_escape_is_held_for_the_next_read() {
        let mut carry = Vec::new();
        assert_eq!(&strip_client_modes(b"x\x1b", &mut carry)[..], b"x");
        assert_eq!(&carry[..], b"\x1b");
        // The held ESC lets the filter see the sequence as a whole even when
        // the read boundary falls right after it. Alternate-screen switches are
        // forwarded, not stripped.
        assert_eq!(
            &strip_client_modes(b"[?1049hy", &mut carry)[..],
            b"\x1b[?1049hy"
        );
        assert!(carry.is_empty());
    }

    #[test]
    fn alt_screen_switches_are_forwarded() {
        let mut carry = Vec::new();
        let data = b"a\x1b[?1049hb\x1b[?1049lc\x1b[?1047h\x1b[?47l";
        let out = strip_client_modes(data, &mut carry);
        assert_eq!(&out[..], data);
        assert!(carry.is_empty());
    }

    #[test]
    fn mixed_sequences_are_forwarded_unchanged() {
        let mut carry = Vec::new();
        let data = b"\x1b[?1049;1000h\x1b[?1002;47l";
        let out = strip_client_modes(data, &mut carry);
        assert_eq!(&out[..], data);
    }

    #[test]
    fn alt_screen_sequence_split_across_reads_is_forwarded() {
        let seq = b"x\x1b[?1049hy";
        for split in 0..=seq.len() {
            let mut carry = Vec::new();
            let mut out = strip_client_modes(&seq[..split], &mut carry);
            out.extend_from_slice(&strip_client_modes(&seq[split..], &mut carry));
            assert_eq!(&out[..], seq, "split at byte {}", split);
        }
    }

    #[test]
    fn non_private_sequences_are_untouched() {
        let mut carry = Vec::new();
        let data = b"\x1b[1;2H\x1b[?1;2c\x1b[1049h";
        let out = strip_client_modes(data, &mut carry);
        assert_eq!(&out[..], data);
        assert!(carry.is_empty());
    }

    /// `CSI Vk;Sc;Uc;Kd;Cs;Rc _` report for one key event.
    fn win32_report(vk: u32, uc: u32, cs: u32, kd: u32) -> Vec<u8> {
        format!("\x1b[{};0;{};{};{};1_", vk, uc, kd, cs).into_bytes()
    }

    #[test]
    fn win32_ctrl_bracket_detaches() {
        let mut d = Win32Detach::new();
        assert!(d.feed(&win32_report(221, 0x1d, 8, 1)));
    }

    #[test]
    fn win32_f12_detaches() {
        let mut d = Win32Detach::new();
        assert!(d.feed(&win32_report(0x7b, 0, 0, 1)));
    }

    #[test]
    fn win32_keyup_and_ordinary_keys_do_not_detach() {
        let mut d = Win32Detach::new();
        assert!(!d.feed(&win32_report(221, 0x1d, 8, 0))); // key-up
        assert!(!d.feed(&win32_report(0x7b, 0, 0, 0))); // F12 key-up
        for ch in "@@detach".chars() {
            assert!(!d.feed(&win32_report(0, ch as u32, 0, 1)));
        }
    }

    #[test]
    fn win32_report_split_across_reads_is_recognised() {
        let mut d = Win32Detach::new();
        let seq = win32_report(221, 0x1d, 8, 1);
        let (head, tail) = seq.split_at(5);
        assert!(!d.feed(head));
        assert!(d.feed(tail));
    }

    #[test]
    fn paste_trigger_splits_promptly() {
        // No trigger: the whole input is one forwardable segment.
        assert_eq!(
            split_paste_triggers(b"abc", PASTE_TRIGGER),
            vec![&b"abc"[..]]
        );
        // One trigger separates the prefix from the suffix.
        assert_eq!(
            split_paste_triggers(b"ab\x1ccd", PASTE_TRIGGER),
            vec![&b"ab"[..], &b"cd"[..]]
        );
        // Several triggers in one read, including back-to-back ones.
        assert_eq!(
            split_paste_triggers(b"\x1c\x1c", PASTE_TRIGGER),
            vec![&b""[..], &b""[..], &b""[..]]
        );
        // A trigger at the very end leaves an empty segment after it.
        assert_eq!(
            split_paste_triggers(b"x\x1c", PASTE_TRIGGER),
            vec![&b"x"[..], &b""[..]]
        );
    }

    #[test]
    fn detach_prefix_drops_paste_triggers() {
        assert_eq!(strip_paste_triggers(b"a\x1cb\x1c", PASTE_TRIGGER), b"ab");
        assert_eq!(strip_paste_triggers(b"abc", PASTE_TRIGGER), b"abc");
    }

    #[test]
    fn paste_trigger_is_not_forwarded_after_detach_detection() {
        // Ctrl+' before an F12 is dropped with the rest of the prefix; detach
        // still wins.
        let mut d = DetachDetector::new();
        let (forward, detach) = d.feed(b"a\x1cb\x1b[24~");
        assert!(detach);
        assert_eq!(strip_paste_triggers(&forward, PASTE_TRIGGER), b"ab");
    }

    #[test]
    fn paste_trigger_override_is_parsed() {
        assert_eq!(parse_paste_trigger("0x1c"), Some(0x1c));
        assert_eq!(parse_paste_trigger("0X1C"), Some(0x1c));
        assert_eq!(parse_paste_trigger("28"), Some(0x1c));
        assert_eq!(parse_paste_trigger(" 0x07 "), Some(0x07));
        assert_eq!(parse_paste_trigger("0x100"), None);
        assert_eq!(parse_paste_trigger("nope"), None);
    }

    #[test]
    fn read_paste_frame_handles_one_frame_and_eof() {
        let mut input = Cursor::new(b"\x00\x00\x00\x05BMabc".to_vec());
        let frame = read_paste_frame(&mut input).unwrap().expect("one frame");
        assert_eq!(frame, b"BMabc");
        assert!(read_paste_frame(&mut input).unwrap().is_none());
    }

    #[test]
    fn read_paste_frame_rejects_truncated_and_oversized_frames() {
        // Announces five bytes but only three follow.
        let mut input = Cursor::new(b"\x00\x00\x00\x05BMa".to_vec());
        assert!(read_paste_frame(&mut input).is_err());

        // Above the 64 MiB cap, rejected before allocating.
        let mut input = Cursor::new((65u32 * 1024 * 1024).to_be_bytes().to_vec());
        assert!(read_paste_frame(&mut input).is_err());
    }

    const OTTY_IMAGE_PATH: &str =
        "/var/folders/v2/l3yrrlmx7y9cmq6msl3gjs0r0000gn/T/otty-paste/image-1793094876888.png";

    #[test]
    fn otty_image_path_is_recognised() {
        let paste = image_paste_path(OTTY_IMAGE_PATH.as_bytes()).expect("path");
        assert_eq!(paste.path, OTTY_IMAGE_PATH);
        assert!(paste.trailing.is_empty());
    }

    #[test]
    fn otty_image_path_keeps_trailing_newline() {
        let input = format!("{}\r\n", OTTY_IMAGE_PATH);
        let paste = image_paste_path(input.as_bytes()).expect("path");
        assert_eq!(paste.path, OTTY_IMAGE_PATH);
        assert_eq!(paste.trailing, b"\r\n");
    }

    #[test]
    fn quoted_otty_paths_are_recognised_only_in_pairs() {
        let single = format!("'{}'", OTTY_IMAGE_PATH);
        let paste = image_paste_path(single.as_bytes()).expect("single quotes");
        assert_eq!(paste.path, OTTY_IMAGE_PATH);
        assert!(paste.trailing.is_empty());

        let double = format!("\"{}\"", OTTY_IMAGE_PATH);
        let paste = image_paste_path(double.as_bytes()).expect("double quotes");
        assert_eq!(paste.path, OTTY_IMAGE_PATH);

        for unbalanced in [
            format!("'{}", OTTY_IMAGE_PATH),
            format!("{}'", OTTY_IMAGE_PATH),
            format!("\"{}", OTTY_IMAGE_PATH),
            format!("{}\"", OTTY_IMAGE_PATH),
        ] {
            assert!(
                image_paste_path(unbalanced.as_bytes()).is_none(),
                "{unbalanced}"
            );
        }
    }

    #[test]
    fn bracketed_paste_path_keeps_bytes_after_the_wrapper() {
        let input = format!("\x1b[200~{}\x1b[201~\r\n", OTTY_IMAGE_PATH);
        let paste = image_paste_path(input.as_bytes()).expect("bracketed");
        assert_eq!(paste.path, OTTY_IMAGE_PATH);
        assert_eq!(paste.trailing, b"\r\n");

        // Without the closing marker the read is not a complete paste.
        let open = format!("\x1b[200~{}", OTTY_IMAGE_PATH);
        assert!(image_paste_path(open.as_bytes()).is_none());
    }

    #[test]
    fn bracketed_paste_split_across_reads_is_reassembled() {
        let mut paste = BracketedPaste::default();
        assert!(matches!(paste.feed(b"\x1b[200~"), PasteStep::Buffered));

        let mut second = OTTY_IMAGE_PATH.as_bytes().to_vec();
        second.extend_from_slice(b"\x1b[201~");
        let PasteStep::Complete(buf) = paste.feed(&second) else {
            panic!("paste should complete in the second read");
        };
        let path = image_paste_path(&buf).expect("path");
        assert_eq!(path.path, OTTY_IMAGE_PATH);
        assert!(path.trailing.is_empty());
    }

    #[test]
    fn complete_bracketed_paste_in_one_read_passes_through() {
        let mut paste = BracketedPaste::default();
        let input = format!("\x1b[200~{}\x1b[201~", OTTY_IMAGE_PATH);
        assert!(matches!(
            paste.feed(input.as_bytes()),
            PasteStep::PassThrough
        ));
        assert!(image_paste_path(input.as_bytes()).is_some());
    }

    #[test]
    fn ordinary_input_passes_through() {
        let mut paste = BracketedPaste::default();
        assert!(matches!(paste.feed(b"a"), PasteStep::PassThrough));
        assert!(matches!(paste.feed(b"\x1b[20"), PasteStep::PassThrough));
        assert!(matches!(paste.feed(b"\x1b[201~"), PasteStep::PassThrough));
    }

    #[test]
    fn oversized_bracketed_paste_is_flushed_unchanged() {
        let mut paste = BracketedPaste::default();
        assert!(matches!(paste.feed(b"\x1b[200~"), PasteStep::Buffered));

        let mut rest = vec![b'a'; MAX_BRACKETED_PASTE];
        rest.extend_from_slice(b"tail");
        let PasteStep::Complete(buf) = paste.feed(&rest) else {
            panic!("oversized paste should be flushed");
        };
        assert_eq!(buf.len(), BRACKETED_PASTE_START.len() + rest.len());
        assert!(buf.starts_with(BRACKETED_PASTE_START));
        assert!(buf.ends_with(b"tail"));
        // The state is cleared, so the next read is treated normally.
        assert!(matches!(paste.feed(b"x"), PasteStep::PassThrough));

        // Exactly at the limit is still held back; one byte more flushes it.
        let mut paste = BracketedPaste::default();
        assert!(matches!(paste.feed(b"\x1b[200~"), PasteStep::Buffered));
        let exact = vec![b'a'; MAX_BRACKETED_PASTE - BRACKETED_PASTE_START.len()];
        assert!(matches!(paste.feed(&exact), PasteStep::Buffered));
        assert!(matches!(paste.feed(b"a"), PasteStep::Complete(_)));
    }

    #[test]
    fn bytes_after_end_marker_stay_in_the_completed_paste() {
        let mut paste = BracketedPaste::default();
        assert!(matches!(paste.feed(b"\x1b[200~"), PasteStep::Buffered));

        let second = format!("{}\x1b[201~\r\n", OTTY_IMAGE_PATH);
        let PasteStep::Complete(buf) = paste.feed(second.as_bytes()) else {
            panic!("paste should complete");
        };
        let path = image_paste_path(&buf).expect("path");
        assert_eq!(path.path, OTTY_IMAGE_PATH);
        assert_eq!(path.trailing, b"\r\n");
    }

    #[test]
    fn completed_paste_resets_for_the_next_one() {
        let mut paste = BracketedPaste::default();
        let start = b"\x1b[200~";
        let body = format!("{}\x1b[201~", OTTY_IMAGE_PATH);
        for _ in 0..2 {
            assert!(matches!(paste.feed(start), PasteStep::Buffered));
            assert!(matches!(
                paste.feed(body.as_bytes()),
                PasteStep::Complete(_)
            ));
        }
    }

    #[test]
    fn uppercase_image_extension_is_recognised() {
        let input = OTTY_IMAGE_PATH.replace(".png", ".PNG");
        assert!(image_paste_path(input.as_bytes()).is_some());
    }

    #[test]
    fn non_single_image_paths_are_rejected() {
        // Two paths in one read are ordinary text, not one paste.
        let two = format!("{} {}", OTTY_IMAGE_PATH, OTTY_IMAGE_PATH);
        assert!(image_paste_path(two.as_bytes()).is_none());

        assert!(image_paste_path(b"otty-paste/image-1.png").is_none());
        assert!(image_paste_path(b"/tmp/image-1.png").is_none());
        assert!(image_paste_path(b"/tmp/otty-paste/image-1.txt").is_none());
        assert!(image_paste_path(b"/tmp/otty-paste/image-\n1.png").is_none());
        assert!(image_paste_path(b"/tmp/otty-paste/image-1.png\xff").is_none());
    }

    #[test]
    fn paste_delay_override_is_parsed() {
        assert_eq!(parse_paste_delay("400"), Some(Duration::from_millis(400)));
        assert_eq!(parse_paste_delay(" 1 "), Some(Duration::from_millis(1)));
        assert_eq!(
            parse_paste_delay("10000"),
            Some(Duration::from_millis(10_000))
        );
        assert_eq!(parse_paste_delay("0"), None);
        assert_eq!(parse_paste_delay("10001"), None);
        assert_eq!(parse_paste_delay("nope"), None);
    }
}

#[cfg(test)]
mod cli_tests {
    use super::{parse, Cmd};

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_invocation_targets_the_last_session() {
        assert_eq!(
            parse(&[]).unwrap(),
            Cmd::Attach {
                name: None,
                wsl: false
            }
        );
    }

    #[test]
    fn name_alone_attaches_or_creates() {
        assert_eq!(
            parse(&args(&["t1"])).unwrap(),
            Cmd::Attach {
                name: Some("t1".into()),
                wsl: false
            }
        );
    }

    #[test]
    fn short_and_long_aliases_agree() {
        for alias in ["ls", "l", "list"] {
            assert_eq!(parse(&args(&[alias])).unwrap(), Cmd::List);
        }
        for alias in ["a", "attach", "c", "continue"] {
            assert_eq!(
                parse(&args(&[alias, "t1"])).unwrap(),
                Cmd::Attach {
                    name: Some("t1".into()),
                    wsl: false
                }
            );
        }
        for alias in ["d", "detach"] {
            assert_eq!(
                parse(&args(&[alias])).unwrap(),
                Cmd::Detach { name: None }
            );
        }
        for alias in ["k", "kill", "rm", "stop"] {
            assert_eq!(
                parse(&args(&[alias, "t1"])).unwrap(),
                Cmd::Stop {
                    name: Some("t1".into())
                }
            );
        }
    }

    #[test]
    fn start_creates_and_needs_a_name() {
        assert_eq!(
            parse(&args(&["start", "t1"])).unwrap(),
            Cmd::Start {
                name: "t1".into(),
                wsl: false
            }
        );
        assert!(parse(&args(&["start"])).is_err());
    }

    #[test]
    fn wsl_flag_reaches_creation_paths() {
        assert_eq!(
            parse(&args(&["t1", "--wsl"])).unwrap(),
            Cmd::Attach {
                name: Some("t1".into()),
                wsl: true
            }
        );
        assert_eq!(
            parse(&args(&["new", "t2", "--wsl"])).unwrap(),
            Cmd::Start {
                name: "t2".into(),
                wsl: true
            }
        );
    }

    #[test]
    fn help_is_recognised() {
        assert_eq!(parse(&args(&["-h"])).unwrap(), Cmd::Help);
        assert_eq!(parse(&args(&["--help"])).unwrap(), Cmd::Help);
        assert_eq!(parse(&args(&["help"])).unwrap(), Cmd::Help);
    }

    #[test]
    fn version_is_recognised() {
        assert_eq!(parse(&args(&["-V"])).unwrap(), Cmd::Version);
        assert_eq!(parse(&args(&["--version"])).unwrap(), Cmd::Version);
    }

    #[test]
    fn unknown_option_is_rejected() {
        let err = parse(&args(&["--bogus"])).unwrap_err();
        assert!(err.contains("unknown option"), "unexpected error: {}", err);
        assert!(parse(&args(&["t1", "--bogus"])).is_err());
    }

    #[test]
    fn help_still_works_as_a_session_name_argument() {
        assert_eq!(
            parse(&args(&["a", "help"])).unwrap(),
            Cmd::Attach {
                name: Some("help".into()),
                wsl: false
            }
        );
    }

    #[test]
    fn clipboard_image_is_parsed_before_session_names() {
        assert_eq!(
            parse(&args(&["clipboard-image"])).unwrap(),
            Cmd::ClipboardImage { stream: false }
        );
        assert_eq!(
            parse(&args(&["clipboard-image", "--stream"])).unwrap(),
            Cmd::ClipboardImage { stream: true }
        );
    }

    #[test]
    fn clipboard_image_rejects_other_arguments() {
        assert!(parse(&args(&["clipboard-image", "--wsl"])).is_err());
        assert!(parse(&args(&["clipboard-image", "name"])).is_err());
    }
}
