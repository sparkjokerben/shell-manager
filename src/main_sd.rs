use std::os::windows::ffi::OsStrExt;
use std::ffi::OsStr;
use anyhow::Result;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::FILE_SHARE_MODE;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, OPEN_EXISTING, SECURITY_SQOS_PRESENT, SECURITY_IDENTIFICATION,
    ReadFile, WriteFile,
};
use windows::Win32::System::Console::{
    GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    GetConsoleMode, SetConsoleMode,
    ENABLE_LINE_INPUT, ENABLE_ECHO_INPUT, ENABLE_PROCESSED_INPUT, ENABLE_WINDOW_INPUT, ENABLE_MOUSE_INPUT,
    ENABLE_PROCESSED_OUTPUT, ENABLE_WRAP_AT_EOL_OUTPUT, CONSOLE_MODE,
};
use windows::Win32::System::Pipes::{PIPE_READMODE_BYTE, PIPE_WAIT, NAMED_PIPE_MODE, SetNamedPipeHandleState};
use shell_daemon::protocol::{self, Request, Op};
use windows::core::PCWSTR;

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
            let pipe_name = resp
                .data
                .and_then(|d| d.get("pipe_name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            attach(&pipe_name)
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
            let pipe_name = resp
                .data
                .and_then(|d| d.get("pipe_name").and_then(|v| v.as_str()).map(|s| s.to_string()))
                .unwrap_or_default();
            attach(&pipe_name)
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

fn attach(pipe_name: &str) -> Result<()> {
    let wide: Vec<u16> = OsStrExt::encode_wide(OsStr::new(pipe_name)).chain(Some(0)).collect();
    let h = match unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    } {
        Ok(h) => h,
        Err(_) => {
            anyhow::bail!("failed to connect to data pipe (is the session attached?)");
        }
    };
    unsafe {
        let mut mode: NAMED_PIPE_MODE = PIPE_READMODE_BYTE | PIPE_WAIT;
        SetNamedPipeHandleState(h, Some(&mut mode), None, None)?;
    }

    // Put the local console into raw mode so input passes through byte-for-byte.
    let stdin = unsafe { GetStdHandle(STD_INPUT_HANDLE)? };
    let stdout = unsafe { GetStdHandle(STD_OUTPUT_HANDLE)? };
    let mut orig_in_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    let mut orig_out_mode: CONSOLE_MODE = CONSOLE_MODE(0);
    unsafe {
        GetConsoleMode(stdin, &mut orig_in_mode)?;
        GetConsoleMode(stdout, &mut orig_out_mode)?;
    }
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

    struct Guard {
        stdin: HANDLE,
        stdout: HANDLE,
        in_mode: CONSOLE_MODE,
        out_mode: CONSOLE_MODE,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                let _ = SetConsoleMode(self.stdin, self.in_mode);
                let _ = SetConsoleMode(self.stdout, self.out_mode);
            }
        }
    }
    let _guard = Guard {
        stdin,
        stdout,
        in_mode: orig_in_mode,
        out_mode: orig_out_mode,
    };

    println!("\r\nattached to {} (escape: ~.)", pipe_name);

    let h_stdin_thread = h.clone();
    let stdout_handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE)? };

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
                        break;
                    }
                    let mut pos = 0;
                    while pos < data.len() {
                        let mut written: u32 = 0;
                        let r2 = unsafe {
                            WriteFile(
                                h_stdin_thread,
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

    // Main thread: pipe -> stdout.
    let mut buf = [0u8; 4096];
    loop {
        let mut read: u32 = 0;
        let r = unsafe {
            ReadFile(
                h,
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
                            stdout_handle,
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

    let _ = stdin_thread.join();
    unsafe {
        let _ = CloseHandle(h);
    }
    println!("\r\ndetached");
    Ok(())
}
