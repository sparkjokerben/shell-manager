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
    ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_WINDOW_INPUT, ENABLE_MOUSE_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_WRAP_AT_EOL_OUTPUT, CONSOLE_MODE,
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
            let req = Request { op: Op::Start, name, wsl: Some(wsl) };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if !resp.ok {
                eprintln!("start failed: {}", resp.error);
                std::process::exit(1);
            }
            let (pipe_out, pipe_in) = extract_pipes(&resp.data);
            attach(&pipe_out, &pipe_in)
        }
        "continue" => {
            if args.len() < 3 {
                eprintln!("usage: sd continue <name>");
                std::process::exit(1);
            }
            let name = args[2].clone();
            let req = Request { op: Op::Continue, name, wsl: None };
            let resp = protocol::pipe_request(protocol::CONTROL_PIPE, &req)?;
            if !resp.ok {
                eprintln!("continue failed: {}", resp.error);
                std::process::exit(1);
            }
            let (pipe_out, pipe_in) = extract_pipes(&resp.data);
            attach(&pipe_out, &pipe_in)
        }
        "list" => {
            let req = Request { op: Op::List, name: String::new(), wsl: None };
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
            let req = Request { op: Op::Stop, name, wsl: None };
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

fn attach(pipe_out: &str, pipe_in: &str) -> Result<()> {
    // Two unidirectional handles: out is read-only, in is write-only.
    // Byte-type pipes use byte read mode by default, so no SetNamedPipeHandleState
    // is needed (it would require extra access rights on a read-only handle).
    let h_out = open_pipe(pipe_out, GENERIC_READ.0)?;
    let h_in = open_pipe(pipe_in, GENERIC_WRITE.0)?;

    // Put the local console into raw mode so input passes through byte-for-byte.
    // If stdin/stdout are not real consoles (e.g. piped), skip raw mode but keep forwarding.
    let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
    let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE)? };
    let mut orig_in_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let mut orig_out_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let has_console = unsafe {
        GetConsoleMode(stdin, &mut orig_in_mode).is_ok()
            && GetConsoleMode(stdout, &mut orig_out_mode).is_ok()
    };
    if has_console {
        let raw_in = orig_in_mode.0
            & !(ENABLE_LINE_INPUT.0
                | ENABLE_ECHO_INPUT.0
                | ENABLE_PROCESSED_INPUT.0
                | ENABLE_WINDOW_INPUT.0
                | ENABLE_MOUSE_INPUT.0);
        let raw_out = orig_out_mode.0 & !(ENABLE_PROCESSED_OUTPUT.0 | ENABLE_WRAP_AT_EOL_OUTPUT.0);
        unsafe {
            SetConsoleMode(stdin, CONSOLE_MODE(raw_in))?;
            SetConsoleMode(stdout, CONSOLE_MODE(raw_out))?;
        }
    } else {
        eprintln!("[sd] no console detected; forwarding without raw mode");
    }

    struct Guard {
        active: bool,
        stdin: HANDLE,
        stdout: HANDLE,
        in_mode: CONSOLE_MODE,
        out_mode: CONSOLE_MODE,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.active {
                unsafe {
                    let _ = SetConsoleMode(self.stdin, self.in_mode);
                    let _ = SetConsoleMode(self.stdout, self.out_mode);
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
    };

    println!("\r\nattached to {} (escape: ~.)", pipe_out);

    // The main thread will block reading the out pipe. Remember its id so the
    // stdin thread can cancel that read when the user detaches with "~.".
    let main_tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };

    let stdin_thread = std::thread::spawn(move || -> Result<()> {
        let stdin_handle = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
        let mut buf = [0u8; 4096];
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
                    // Detach sequence: "~." optionally followed by CR/LF.
                    if data == b"~.\r" || data == b"~.\n" || data == b"~." {
                        // Unblock the main thread's ReadFile on the out pipe.
                        unsafe {
                            use windows::Win32::System::IO::CancelSynchronousIo;
                            use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
                            if let Ok(ht) = OpenThread(THREAD_TERMINATE, false, main_tid) {
                                let _ = CancelSynchronousIo(ht);
                                let _ = windows::Win32::Foundation::CloseHandle(ht);
                            }
                        }
                        break;
                    }
                    let mut pos = 0;
                    while pos < data.len() {
                        let mut written: u32 = 0;
                        let r2 = unsafe {
                            WriteFile(
                                h_in,
                                Some(&data[pos..]),
                                Some(&mut written),
                                None,
                            )
                        };
                        match r2 {
                            Ok(()) if written > 0 => pos += written as usize,
                            Ok(()) => break,
                            Err(_) => return Ok(()),
                        }
                    }
                }
                Err(_) => break,
            }
        }
        Ok(())
    });

    // Main thread: out pipe -> stdout.
    let mut buf = [0u8; 4096];
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
                let mut pos = 0;
                while pos < read as usize {
                    let mut written: u32 = 0;
                    let r2 = unsafe {
                        WriteFile(
                            stdout,
                            Some(&buf[pos..read as usize]),
                            Some(&mut written),
                            None,
                        )
                    };
                    match r2 {
                        Ok(()) if written > 0 => pos += written as usize,
                        Ok(()) => break,
                        Err(_) => break,
                    }
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
