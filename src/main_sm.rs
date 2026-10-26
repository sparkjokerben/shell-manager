use std::os::windows::ffi::OsStrExt;
use std::ffi::OsStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use anyhow::Result;
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
options: --wsl  create the session with wsl.exe instead of PowerShell";

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
    Help,
    Version,
}

fn parse(args: &[String]) -> std::result::Result<Cmd, String> {
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
            println!("{}", env!("CARGO_PKG_VERSION"));
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
            attach_session(&name, &resp)
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
    }
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
    protocol::pipe_request(protocol::CONTROL_PIPE, &req)
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
        return attach_session(name, &resp);
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
                return attach_session(name, &retried);
            }
            eprintln!("attach failed: {}", retried.error);
            std::process::exit(1);
        }
        eprintln!("start failed: {}", started.error);
        std::process::exit(1);
    }
    attach_session(name, &started)
}

fn attach_session(name: &str, resp: &protocol::Response) -> Result<()> {
    let (pipe_out, pipe_in, mouse) = extract_pipes(&resp.data);
    if let Err(e) = attach(name, &pipe_out, &pipe_in, mouse) {
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
        let _ = protocol::pipe_request(protocol::CONTROL_PIPE, &req);
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
fn open_pipe(name: &str, access: u32) -> Result<HANDLE> {
    const ERROR_PIPE_BUSY: u32 = 0x8007_00E7;
    let w = wide(name);

    for _ in 0..40 {
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
                    500,
                );
            },
            Err(e) => anyhow::bail!("failed to connect to data pipe {}: {}", name, e),
        }
    }

    anyhow::bail!("timed out connecting to data pipe {}", name)
}

/// Bumped whenever the client's input handling changes, so a stale attach is
/// easy to spot in the banner.
const CLIENT_BUILD: &str = "client 2026-10-26g";

/// Appends a diagnostic line to %TEMP%\sm-client.log.
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
        let path = std::env::temp_dir().join("sm-client.log");
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

/// Detach triggers. The printable sequence works from any keyboard and is not
/// consumed by ssh (unlike "~."); F12 and Ctrl+] are kept as extras.
/// Keys that detach the client. Ctrl+] is the control byte GS; F12 is the VT
/// sequence. A Win32-input-mode report for either is handled separately.
const DETACH_SEQS: &[&[u8]] = &[b"\x1b[24~", b"\x1d"];

/// Disable terminal mouse reporting. A TUI that exits without restoring its
/// mouse mode otherwise leaves the terminal sending SGR mouse reports, which
/// leak into the shell as typed garbage.
const MOUSE_RESET: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

/// DECRST 9001: turn "Win32 input mode" off (Windows Terminal extension).
///
/// PowerShell 7 enables it via `CSI ? 9001 h`. A terminal that honours it stops
/// sending plain VT keystrokes and instead encodes every key as
/// `CSI Vk;Sc;Uc;Kd;Cs;Rc _`, which matches none of the detach sequences and
/// leaves the mouse filter nothing to recognise. The client therefore hides the
/// request from the terminal and switches the mode off on every attach.
const WIN32_INPUT_OFF: &[u8] = b"\x1b[?9001l";

/// Sequence prefix the hosted app uses to enable Win32 input mode.
const WIN32_INPUT_SEQ: &[u8] = b"\x1b[?9001";

/// Drop `CSI ? 9001 h|l` from the PTY output so the terminal keeps encoding
/// input as ordinary VT bytes. A sequence split across reads is held in `carry`
/// until its final byte arrives.
fn strip_win32_input_mode(data: &[u8], carry: &mut Vec<u8>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(carry.len() + data.len());
    buf.extend_from_slice(carry);
    buf.extend_from_slice(data);
    carry.clear();

    let mut out = Vec::with_capacity(buf.len());
    let mut i = 0;
    while i < buf.len() {
        let rest = &buf[i..];
        if rest.starts_with(WIN32_INPUT_SEQ) {
            if rest.len() == WIN32_INPUT_SEQ.len() {
                // Final byte not seen yet.
                carry.extend_from_slice(rest);
                break;
            }
            if matches!(rest[WIN32_INPUT_SEQ.len()], b'h' | b'l') {
                i += WIN32_INPUT_SEQ.len() + 1;
                continue;
            }
        } else if rest.len() >= 2
            && rest.len() < WIN32_INPUT_SEQ.len()
            && WIN32_INPUT_SEQ.starts_with(rest)
        {
            // Possible prefix of the sequence at the end of this chunk.
            carry.extend_from_slice(rest);
            break;
        }
        out.push(buf[i]);
        i += 1;
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

fn attach(name: &str, pipe_out: &str, pipe_in: &str, mouse: u8) -> Result<()> {
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

    client_log(&format!(
        "attach build={} pid={} has_console={} in_mode={:#x} out_mode={:#x} mouse={} stdin={} stdout={}",
        CLIENT_BUILD,
        std::process::id(),
        has_console,
        orig_in_mode.0,
        orig_out_mode.0,
        mouse,
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

    // Nothing else tells the hosted app that the screen should be redrawn, so
    // make it repaint once the daemon has accepted this client.
    {
        let repaint_name = name.to_string();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            force_repaint(&repaint_name);
        });
    }

    // Put the local console into raw mode so input passes through byte-for-byte.
    let mut raw_in_mode = CONSOLE_MODE(0);
    if has_console {
        // Raw input plus VT input, so arrows and function keys arrive as VT
        // sequences. Mouse input is enabled only when the hosted app had it on
        // before this client attached; the reader thread switches it on and off
        // as the app toggles its DEC modes. Hover-only motion reports are
        // filtered out below so they do not get typed into a plain shell prompt.
        // ENABLE_EXTENDED_FLAGS is required for QUICK_EDIT_MODE to take effect.
        let raw_in = (orig_in_mode.0
            | ENABLE_EXTENDED_FLAGS.0
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0)
            & !(ENABLE_LINE_INPUT.0
                | ENABLE_ECHO_INPUT.0
                | ENABLE_PROCESSED_INPUT.0
                | ENABLE_WINDOW_INPUT.0
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
        let vt_out_ok = unsafe {
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
            // would show up as literal garbage on screen. Start from a clean
            // terminal state, then restore the mouse modes the hosted app had
            // enabled before this client attached.
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
                last = cur;
                let req = Request {
                    op: Op::Resize,
                    name: resize_name.clone(),
                    wsl: None,
                    cols: cur.0,
                    rows: cur.1,
                };
                // Failures are non-fatal: the session may be gone already.
                let _ = protocol::pipe_request(protocol::CONTROL_PIPE, &req);
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

    let stdin_thread = std::thread::spawn(move || -> Result<()> {
        let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
        let mut buf = [0u8; 4096];
        let mut detector = DetachDetector::new();
        let mut motion_pending: Vec<u8> = Vec::new();
        let mut win32 = Win32Detach::new();
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
                        // tearing the connection down.
                        if !forward.is_empty() {
                            let _ = write_all(h_in, &forward);
                        }
                        // Unblock the reader thread's ReadFile on the out pipe.
                        stop_reader(&stdin_reader_tid, &stdin_done);
                        break;
                    }
                    if !forward.is_empty() && !write_all(h_in, &forward)? {
                        client_log("stdin forward failed");
                        return Ok(());
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
        // Holds a split `CSI ? 9001 h|l` between reads; see strip_win32_input_mode.
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
                    let data = strip_win32_input_mode(&data, &mut mode_carry);
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
    if has_console {
        let _ = write_all(stdout, MOUSE_RESET);
    }
    // Plain write instead of println!: a console write racing with teardown must
    // not panic the client with "failed printing to stdout".
    let _ = write_all(stdout, format!("\r\ndetached ({})\r\n", CLIENT_BUILD).as_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{filter_motion, strip_win32_input_mode, DetachDetector, Win32Detach};

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
            &strip_win32_input_mode(b"a\x1b[?9001hb", &mut carry)[..],
            b"ab"
        );
        assert!(carry.is_empty());
    }

    #[test]
    fn win32_input_mode_sequence_split_across_reads() {
        let mut carry = Vec::new();
        assert_eq!(&strip_win32_input_mode(b"a\x1b[?90", &mut carry)[..], b"a");
        assert_eq!(&strip_win32_input_mode(b"01hb", &mut carry)[..], b"b");
        assert!(carry.is_empty());
    }

    #[test]
    fn win32_input_mode_sequence_split_at_final_byte() {
        let mut carry = Vec::new();
        assert_eq!(&strip_win32_input_mode(b"\x1b[?9001", &mut carry)[..], b"");
        assert_eq!(&strip_win32_input_mode(b"l", &mut carry)[..], b"");
        assert!(carry.is_empty());
    }

    #[test]
    fn other_private_modes_survive_stripping() {
        let mut carry = Vec::new();
        let out = strip_win32_input_mode(b"\x1b[?1003h\x1b[?9001l\x1b[?25l", &mut carry);
        assert_eq!(&out[..], b"\x1b[?1003h\x1b[?25l");
    }

    #[test]
    fn trailing_escape_survives_stripping() {
        let mut carry = Vec::new();
        assert_eq!(&strip_win32_input_mode(b"x\x1b", &mut carry)[..], b"x\x1b");
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
}
