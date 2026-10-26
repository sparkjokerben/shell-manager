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
use shell_daemon::protocol::{self, Request, Op};
use shell_daemon::session::{for_each_mouse_mode, mouse_bit};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: sd <start|continue|detach|list|stop> <name> [--wsl]");
        std::process::exit(1);
    }
    match args[1].as_str() {
        "start" => {
            if args.len() < 3 {
                eprintln!("usage: sd start <name> [--wsl]");
                std::process::exit(1);
            }
            let name = args[2].clone();
            let wsl = args.iter().any(|a| a == "--wsl");
            let (cols, rows) = current_size();
            let req = Request { op: Op::Start, name: name.clone(), wsl: Some(wsl), cols, rows };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if !resp.ok {
                eprintln!("start failed: {}", resp.error);
                std::process::exit(1);
            }
            let (pipe_out, pipe_in, mouse) = extract_pipes(&resp.data);
            attach(&name, &pipe_out, &pipe_in, mouse)
        }
        "continue" => {
            if args.len() < 3 {
                eprintln!("usage: sd continue <name>");
                std::process::exit(1);
            }
            let name = args[2].clone();
            let (cols, rows) = current_size();
            let req = Request { op: Op::Continue, name: name.clone(), wsl: None, cols, rows };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if !resp.ok {
                eprintln!("continue failed: {}", resp.error);
                std::process::exit(1);
            }
            let (pipe_out, pipe_in, mouse) = extract_pipes(&resp.data);
            attach(&name, &pipe_out, &pipe_in, mouse)
        }
        "detach" => {
            if args.len() < 3 {
                eprintln!("usage: sd detach <name>");
                std::process::exit(1);
            }
            let name = args[2].clone();
            let req = Request { op: Op::Detach, name, wsl: None, cols: None, rows: None };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if resp.ok {
                println!("detached");
            } else {
                eprintln!("detach failed: {}", resp.error);
                std::process::exit(1);
            }
            Ok(())
        }
        "list" => {
            let req = Request { op: Op::List, name: String::new(), wsl: None, cols: None, rows: None };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if resp.ok {
                if let Some(d) = resp.data {
                    let arr = d.as_array().cloned().unwrap_or_default();
                    for info in arr {
                        let name = info.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let attached = info.get("attached").and_then(|v| v.as_bool()).unwrap_or(false);
                        println!("{} {}", if attached { "*" } else { " " }, name);
                    }
                }
            } else {
                eprintln!("list failed: {}", resp.error);
            }
            Ok(())
        }
        "stop" => {
            if args.len() < 3 {
                eprintln!("usage: sd stop <name>");
                std::process::exit(1);
            }
            let name = args[2].clone();
            let req = Request { op: Op::Stop, name, wsl: None, cols: None, rows: None };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if resp.ok {
                println!("stopped");
            } else {
                eprintln!("stop failed: {}", resp.error);
                std::process::exit(1);
            }
            Ok(())
        }
        _ => {
            eprintln!("unknown command: {}", args[1]);
            std::process::exit(1);
        }
    }
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

/// Detach triggers. The printable sequence works from any keyboard and is not
/// consumed by ssh (unlike "~."); F12 and Ctrl+] are kept as extras.
const DETACH_SEQS: &[&[u8]] = &[b"@@detach", b"\x1b[24~", b"\x1d"];

/// Disable terminal mouse reporting. A TUI that exits without restoring its
/// mouse mode otherwise leaves the terminal sending SGR mouse reports, which
/// leak into the shell as typed garbage.
const MOUSE_RESET: &[u8] = b"\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l";

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

/// Removes hover-only mouse motion reports (SGR and X10 encodings).
///
/// ConPTY asks the terminal for any-event mouse tracking whenever mouse input
/// is enabled, so simply moving the pointer at a plain shell prompt would type
/// escape sequences into it. Clicks, wheel and drag events still pass through
/// for the hosted TUI.
fn filter_motion(data: &[u8], pending: &mut Vec<u8>) -> Vec<u8> {
    let mut buf = std::mem::take(pending);
    buf.extend_from_slice(data);
    let mut out = Vec::with_capacity(buf.len());
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
        eprintln!("[sd] no console detected; forwarding without raw mode");
    }

    // Print before connecting: once connected, the daemon starts streaming PTY
    // output, and mixing console-wide prints with raw byte writes garbles the line.
    println!("\r\nattached (detach: type @@detach)");
    // Start from a clean terminal state, then restore the mouse modes the
    // hosted app had enabled before this client attached.
    if has_console {
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

    // Two unidirectional handles: out is read-only, in is write-only.
    // Byte-type pipes use byte read mode by default, so no SetNamedPipeHandleState
    // is needed (it would require extra access rights on a read-only handle).
    let h_out = open_pipe(pipe_out, GENERIC_READ.0)?;
    let h_in = open_pipe(pipe_in, GENERIC_WRITE.0)?;

    // Put the local console into raw mode so input passes through byte-for-byte.
    let mut raw_in_mode = CONSOLE_MODE(0);
    if has_console {
        // Raw input plus VT input, so arrows and function keys arrive as VT
        // sequences. Mouse input stays enabled because the hosted TUI needs it;
        // hover-only motion reports are filtered out below so they do not get
        // typed into a plain shell prompt.
        // ENABLE_EXTENDED_FLAGS is required for QUICK_EDIT_MODE to take effect.
        let raw_in = (orig_in_mode.0
            | ENABLE_EXTENDED_FLAGS.0
            | ENABLE_VIRTUAL_TERMINAL_INPUT.0
            | ENABLE_MOUSE_INPUT.0)
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
        unsafe {
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
            let _ = SetConsoleMode(stdout, CONSOLE_MODE(raw_out));
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
                Ok(()) if read == 0 => break,
                Ok(()) => {
                    let filtered = filter_motion(&buf[..read as usize], &mut motion_pending);
                    let data = &filtered[..];
                    if data.is_empty() {
                        continue;
                    }
                    let (forward, detach) = detector.feed(data);
                    if detach {
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
                        return Ok(());
                    }
                }
                Err(_) => break,
            }
        }
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
                Ok(()) if read == 0 => break,
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
                        let keep = scan.len().min(16);
                        mouse_carry.clear();
                        mouse_carry.extend_from_slice(&scan[scan.len() - keep..]);

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
                    if !data.is_empty() {
                        // A cancelled console write must only end this thread,
                        // never panic the process.
                        match write_all(stdout, &data) {
                            Ok(true) => {}
                            _ => break,
                        }
                    }
                }
                Err(_) => break,
            }
        }
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
    let _ = write_all(stdout, b"\r\ndetached\r\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{filter_motion, DetachDetector};

    #[test]
    fn magic_sequence_detaches() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"@@detach");
        assert!(detach);
        assert!(out.is_empty());
    }

    #[test]
    fn magic_sequence_after_text_forwards_prefix() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"abc@@detach");
        assert!(detach);
        assert_eq!(&out[..], b"abc");
    }

    #[test]
    fn magic_sequence_split_across_reads() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"ab@@det");
        assert!(!detach);
        assert_eq!(&out[..], b"ab");
        let (out, detach) = d.feed(b"ach");
        assert!(detach);
        assert!(out.is_empty());
    }

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
}
