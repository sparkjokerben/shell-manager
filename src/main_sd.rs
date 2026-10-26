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
            let (pipe_out, pipe_in) = extract_pipes(&resp.data);
            attach(&name, &pipe_out, &pipe_in)
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
            let (pipe_out, pipe_in) = extract_pipes(&resp.data);
            attach(&name, &pipe_out, &pipe_in)
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

fn extract_pipes(data: &Option<serde_json::Value>) -> (String, String) {
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
    (out, inp)
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

const DETACH_KEY: u8 = 0x1d;

/// Position of the Ctrl+] detach key in a read, if present.
fn detach_key_pos(data: &[u8]) -> Option<usize> {
    data.iter().position(|&b| b == DETACH_KEY)
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

/// Detects the "~." detach sequence at the start of a line, like ssh.
///
/// A "~" typed at line start is held back until the next byte arrives, so a
/// sequence split across two reads is still recognized.
struct DetachDetector {
    held_tilde: bool,
    line_start: bool,
}

impl DetachDetector {
    fn new() -> Self {
        Self { held_tilde: false, line_start: true }
    }

    /// Returns the bytes to forward and whether detach was requested.
    fn feed(&mut self, data: &[u8]) -> (Vec<u8>, bool) {
        let mut out = Vec::with_capacity(data.len() + 1);
        for &b in data {
            if self.held_tilde {
                self.held_tilde = false;
                if b == b'.' {
                    return (out, true);
                }
                out.push(b'~');
                self.line_start = false;
            }
            if self.line_start && b == b'~' {
                self.held_tilde = true;
                continue;
            }
            out.push(b);
            self.line_start = b == b'\r' || b == b'\n';
        }
        (out, false)
    }
}

fn attach(name: &str, pipe_out: &str, pipe_in: &str) -> Result<()> {
    let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
    let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE)? };
    let mut orig_in_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let mut orig_out_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let has_console = unsafe {
        GetConsoleMode(stdin, &mut orig_in_mode).is_ok()
            && GetConsoleMode(stdout, &mut orig_out_mode).is_ok()
    };

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
    println!("\r\nattached (detach: Ctrl+] or ~. at line start)");

    // Two unidirectional handles: out is read-only, in is write-only.
    // Byte-type pipes use byte read mode by default, so no SetNamedPipeHandleState
    // is needed (it would require extra access rights on a read-only handle).
    let h_out = open_pipe(pipe_out, GENERIC_READ.0)?;
    let h_in = open_pipe(pipe_in, GENERIC_WRITE.0)?;

    // Put the local console into raw mode so input passes through byte-for-byte.
    if has_console {
        // Raw input, plus VT input so arrows / function keys / mouse arrive as VT
        // sequences instead of being dropped by the console input translator.
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
        // Processed output + VT rendering. DISABLE_NEWLINE_AUTO_RETURN makes the
        // console treat LF like a real terminal (no implicit carriage return).
        let raw_out = orig_out_mode.0
            | ENABLE_PROCESSED_OUTPUT.0
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING.0
            | DISABLE_NEWLINE_AUTO_RETURN.0;
        unsafe {
            if SetConsoleMode(stdin, CONSOLE_MODE(raw_in)).is_err() {
                // VT input unsupported on this console: fall back to plain raw mode.
                let fallback = (orig_in_mode.0 | ENABLE_EXTENDED_FLAGS.0)
                    & !(ENABLE_LINE_INPUT.0
                        | ENABLE_ECHO_INPUT.0
                        | ENABLE_PROCESSED_INPUT.0
                        | ENABLE_WINDOW_INPUT.0
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
                    let data = &buf[..read as usize];
                    if let Some(pos) = detach_key_pos(data) {
                        // Forward whatever preceded the key on this read, then
                        // tear the connection down.
                        if pos > 0 {
                            let _ = write_all(h_in, &data[..pos]);
                        }
                        stop_reader(&stdin_reader_tid, &stdin_done);
                        break;
                    }
                    let (forward, detach) = detector.feed(data);
                    if detach {
                        // Forward whatever preceded the escape on this read
                        // before tearing the connection down (ssh-like).
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
    // Plain write instead of println!: a console write racing with teardown must
    // not panic the client with "failed printing to stdout".
    let _ = write_all(stdout, b"\r\ndetached\r\n");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{detach_key_pos, DetachDetector};

    #[test]
    fn detach_key_absent() {
        assert_eq!(detach_key_pos(b"hello\r\n"), None);
    }

    #[test]
    fn detach_key_at_start() {
        assert_eq!(detach_key_pos(b"\x1drest"), Some(0));
    }

    #[test]
    fn detach_key_after_bytes() {
        assert_eq!(detach_key_pos(b"abcd\x1d"), Some(4));
    }

    #[test]
    fn detach_sequence_split_across_reads() {
        let mut d = DetachDetector::new();
        assert_eq!(d.feed(b"~"), (Vec::new(), false));
        assert_eq!(d.feed(b"."), (Vec::new(), true));
    }

    #[test]
    fn tilde_inside_a_line_is_forwarded_unchanged() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"cd ~/x\r");
        assert!(!detach);
        assert_eq!(&out[..], &b"cd ~/x\r"[..]);
    }

    #[test]
    fn tilde_at_line_start_followed_by_other_char() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"~a");
        assert!(!detach);
        assert_eq!(&out[..], &b"~a"[..]);
    }

    #[test]
    fn detach_only_at_line_start() {
        let mut d = DetachDetector::new();
        let (out, detach) = d.feed(b"x~.");
        assert!(!detach);
        assert_eq!(&out[..], &b"x~."[..]);
    }
}
