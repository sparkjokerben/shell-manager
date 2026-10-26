use std::os::windows::ffi::OsStrExt;
use std::ffi::OsStr;
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
        eprintln!("usage: sd <start|continue|list|stop> <name> [--wsl]");
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
fn open_pipe(name: &str, access: u32) -> Result<HANDLE> {
    let w = wide(name);
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
        Ok(h) => Ok(h),
        Err(_) => anyhow::bail!("failed to connect to data pipe {}", name),
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
    println!("\r\nattached (detach: type ~. at the start of a line)");

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

    // The main thread will block reading the out pipe. Remember its id so the
    // stdin thread can cancel that read when the user detaches with "~.".
    let main_tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };

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
                    let (forward, detach) = detector.feed(data);
                    if detach {
                        // Unblock the main thread's ReadFile on the out pipe.
                        // Retry in a loop: the main thread may not have posted its
                        // read yet when the first cancellation is issued.
                        unsafe {
                            use windows::Win32::System::IO::CancelSynchronousIo;
                            use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
                            while let Ok(ht) = OpenThread(THREAD_TERMINATE, false, main_tid) {
                                let _ = CancelSynchronousIo(ht);
                                let _ = windows::Win32::Foundation::CloseHandle(ht);
                                std::thread::sleep(std::time::Duration::from_millis(20));
                            }
                        }
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

    // Main thread: out pipe -> stdout.
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
                if !data.is_empty() && !write_all(stdout, &data)? {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    // Do NOT join the stdin thread: in raw mode it may be blocked in a console
    // read that never returns. Returning from main terminates the process.
    drop(stdin_thread);
    println!("\r\ndetached");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::DetachDetector;

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
