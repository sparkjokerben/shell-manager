//! Session: a single PTY-backed shell session managed by the daemon.
//!
//! The client talks to a session through **two unidirectional named pipes**:
//! - `out`: daemon writes PTY output, client reads it.
//! - `in`:  client writes keyboard input, daemon reads it.
//!
//! Using separate handles per direction avoids Windows serializing a blocking
//! `ReadFile` against a `WriteFile` on the same synchronous (non-overlapped)
//! pipe handle.

use crate::pty::Pty;
use anyhow::Result;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread;
use std::time::Duration;
use uuid::Uuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, ConnectNamedPipe, DisconnectNamedPipe,
    PIPE_TYPE_BYTE, PIPE_READMODE_BYTE, PIPE_WAIT,
    PIPE_UNLIMITED_INSTANCES, NMPWAIT_WAIT_FOREVER,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};

const DATA_PIPE_PREFIX: &str = r"\\.\pipe\shell-daemon\data";

/// State of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Created,   // PTY + process spawned, no client attached yet
    Attached,  // a client is attached
    Detached,  // client detached; session still alive
    Stopped,   // session terminated
}

/// Shared handle to a single shell session.
pub struct Session {
    pub name: String,
    pub session_id: String,
    pub pty: Arc<Mutex<Pty>>,
    pub state: Arc<Mutex<SessionState>>,
    /// Set to stop both directional threads.
    pub stop_flag: Arc<AtomicBool>,
    /// True while a client is believed to be attached.
    pub client_connected: Arc<AtomicBool>,
    /// Set by an out-of-band detach request; both forwarder threads watch it.
    pub detach_requested: Arc<AtomicBool>,
    /// Win32 thread ids of the out/in threads (for cancelling blocking I/O).
    pub out_tid: Arc<Mutex<Option<u32>>>,
    pub in_tid: Arc<Mutex<Option<u32>>>,
}

impl Session {
    pub fn new(name: &str, cols: u16, rows: u16, cmd: &str) -> Result<Self> {
        let session_id = Uuid::new_v4().to_string();
        let mut pty = Pty::new(cols, rows)?;
        pty.spawn(cmd)?;

        Ok(Session {
            name: name.to_string(),
            session_id,
            pty: Arc::new(Mutex::new(pty)),
            state: Arc::new(Mutex::new(SessionState::Created)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            client_connected: Arc::new(AtomicBool::new(false)),
            detach_requested: Arc::new(AtomicBool::new(false)),
            out_tid: Arc::new(Mutex::new(None)),
            in_tid: Arc::new(Mutex::new(None)),
        })
    }

    /// (out_pipe_path, in_pipe_path) for this session.
    pub fn data_pipe_paths(&self) -> (String, String) {
        (
            format!("{}\\{}\\out", DATA_PIPE_PREFIX, self.session_id),
            format!("{}\\{}\\in", DATA_PIPE_PREFIX, self.session_id),
        )
    }

    /// Start the two directional listener/forwarder threads.
    pub fn start_data_pipe_listener(&self) -> Result<()> {
        let (out_path, in_path) = self.data_pipe_paths();

        // out: daemon -> client (server writes).
        let out_h = create_pipe(&out_path, PIPE_ACCESS_OUTBOUND)?;
        // in: client -> daemon (server reads).
        let in_h = create_pipe(&in_path, PIPE_ACCESS_INBOUND)?;

        let pty_out = self.pty.clone();
        let pty_in = self.pty.clone();
        let state_out = self.state.clone();
        let state_in = self.state.clone();
        let stop_out = self.stop_flag.clone();
        let stop_in = self.stop_flag.clone();
        let alive_out = self.client_connected.clone();
        let alive_in = self.client_connected.clone();
        let out_tid = self.out_tid.clone();
        let in_tid = self.in_tid.clone();
        let detach_out = self.detach_requested.clone();
        let detach_in = self.detach_requested.clone();

        // In thread (client -> PTY).
        let in_tid_slot = in_tid.clone();
        let stop_in_thread = stop_in.clone();
        thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *in_tid_slot.lock().unwrap() = Some(tid);
            run_in_thread(in_h, pty_in, state_in, stop_in_thread, alive_in, detach_in);
        });

        // Out thread (PTY -> client).
        thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *out_tid.lock().unwrap() = Some(tid);
            run_out_thread(out_h, pty_out, state_out, stop_out, alive_out, detach_out);
        });

        Ok(())
    }

    /// Resize the underlying pseudo console to match the client terminal.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.lock().unwrap().resize(cols, rows)
    }

    /// Whether the hosted shell process is still running.
    pub fn is_alive(&self) -> bool {
        self.pty.lock().unwrap().is_running()
    }

    /// Stop the session: signal and cancel both directional threads.
    pub fn stop(&self) -> Result<()> {
        *self.state.lock().unwrap() = SessionState::Stopped;
        self.stop_flag.store(true, Ordering::Relaxed);
        // Copy the thread ids, then cancel without holding any tid lock
        // (CancelSynchronousIo can block until the target I/O aborts).
        let ot = *self.out_tid.lock().unwrap();
        let it = *self.in_tid.lock().unwrap();
        cancel_thread_io(ot);
        cancel_thread_io(it);
        Ok(())
    }

    /// Disconnect the attached client without stopping the session.
    ///
    /// The actual cancellation runs on a helper thread: CancelSynchronousIo can
    /// block, and callers may hold the session table lock.
    pub fn detach_client(&self) {
        if !self.client_connected.load(Ordering::Relaxed) {
            return;
        }

        self.detach_requested.store(true, Ordering::Relaxed);

        let in_tid = self.in_tid.clone();
        let out_tid = self.out_tid.clone();
        let connected = self.client_connected.clone();
        let requested = self.detach_requested.clone();

        thread::spawn(move || {
            // Retry until the client is gone: a single cancel can miss when the
            // target thread is between blocking calls. A fresh attach clears the
            // request flag, which also stops this loop.
            for _ in 0..50 {
                if !connected.load(Ordering::Relaxed) || !requested.load(Ordering::Relaxed) {
                    break;
                }
                let it = *in_tid.lock().unwrap();
                let ot = *out_tid.lock().unwrap();
                cancel_thread_io(it);
                cancel_thread_io(ot);
                thread::sleep(Duration::from_millis(20));
            }
        });
    }
}

fn create_pipe(path: &str, access: windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES) -> Result<HANDLE> {
    let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(
        std::ffi::OsStr::new(path),
    )
    .chain(Some(0))
    .collect();

    let h = unsafe {
        CreateNamedPipeW(
            windows::core::PCWSTR::from_raw(wide.as_ptr()),
            access,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            NMPWAIT_WAIT_FOREVER,
            None,
        )
    };
    if h.is_invalid() {
        anyhow::bail!("CreateNamedPipe failed for {}", path);
    }
    Ok(h)
}

/// Cancel a blocking synchronous I/O call in the given thread.
fn cancel_thread_io(tid: Option<u32>) {
    if let Some(tid) = tid {
        unsafe {
            use windows::Win32::System::IO::CancelSynchronousIo;
            use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
            if let Ok(ht) = OpenThread(THREAD_TERMINATE, false, tid) {
                let _ = CancelSynchronousIo(ht);
                let _ = CloseHandle(ht);
            }
        }
    }
}

/// Accept one client, forward input to the PTY, then loop for the next client.
fn run_in_thread(
    server: HANDLE,
    pty: Arc<Mutex<Pty>>,
    state: Arc<Mutex<SessionState>>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    detach: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let connected = unsafe {
            crate::protocol::is_pipe_connected(&ConnectNamedPipe(server, None))
        };
        if !connected {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }

        set_attached(&state);
        alive.store(true, Ordering::Relaxed);
        detach.store(false, Ordering::Relaxed);

        // Read client input until the client disconnects or a detach is asked for.
        loop {
            if stop.load(Ordering::Relaxed) || detach.load(Ordering::Relaxed) {
                break;
            }
            let mut read: u32 = 0;
            let result = unsafe {
                ReadFile(server, Some(&mut buf), Some(&mut read), None)
            };
            match result {
                Ok(()) if read == 0 => break,
                Ok(()) => {
                    let _ = pty.lock().unwrap().write_input(&buf[..read as usize]);
                }
                Err(_) => break,
            }
        }

        alive.store(false, Ordering::Relaxed);
        set_detached(&state);

        unsafe {
            let _ = DisconnectNamedPipe(server);
        }
    }

    unsafe {
        let _ = CloseHandle(server);
    }
}

/// Accept one client, forward PTY output to it, then loop for the next client.
fn run_out_thread(
    server: HANDLE,
    pty: Arc<Mutex<Pty>>,
    state: Arc<Mutex<SessionState>>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    detach: Arc<AtomicBool>,
) {
    let mut buf = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let connected = unsafe {
            crate::protocol::is_pipe_connected(&ConnectNamedPipe(server, None))
        };
        if !connected {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }

        set_attached(&state);
        alive.store(true, Ordering::Relaxed);
        detach.store(false, Ordering::Relaxed);

        'forward: loop {
            // Checked on every iteration, including while output is flowing: a
            // detach or client loss must disconnect even under constant load.
            if stop.load(Ordering::Relaxed)
                || detach.load(Ordering::Relaxed)
                || !alive.load(Ordering::Relaxed)
            {
                break 'forward;
            }

            let avail = {
                let p = pty.lock().unwrap();
                match p.available() {
                    Ok(n) => n,
                    // Broken pipe: child process exited.
                    Err(_) => break 'forward,
                }
            };

            if avail > 0 {
                let n = match pty.lock().unwrap().read_output(&mut buf) {
                    Ok(0) => break 'forward,
                    Ok(n) => n as usize,
                    Err(_) => break 'forward,
                };

                let mut pos: usize = 0;
                while pos < n {
                    let mut written: u32 = 0;
                    let r = unsafe {
                        WriteFile(server, Some(&buf[pos..n]), Some(&mut written), None)
                    };
                    match r {
                        Ok(()) if written > 0 => pos += written as usize,
                        Ok(()) => break,
                        Err(_) => break 'forward,
                    }
                }
            } else {
                // No pending output. Stop if the client left or the shell exited.
                if !alive.load(Ordering::Relaxed) {
                    break 'forward;
                }
                if !pty.lock().unwrap().is_running() {
                    break 'forward;
                }
                thread::sleep(Duration::from_millis(2));
            }
        }

        alive.store(false, Ordering::Relaxed);
        set_detached(&state);

        unsafe {
            let _ = DisconnectNamedPipe(server);
        }
    }

    unsafe {
        let _ = CloseHandle(server);
    }
}

fn set_attached(state: &Arc<Mutex<SessionState>>) {
    *state.lock().unwrap() = SessionState::Attached;
}

fn set_detached(state: &Arc<Mutex<SessionState>>) {
    let mut s = state.lock().unwrap();
    // Do not overwrite a terminal Stopped state.
    if *s != SessionState::Stopped {
        *s = SessionState::Detached;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::PeekNamedPipe;

    fn open(path: &str, access: u32) -> HANDLE {
        let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let r = unsafe {
                CreateFileW(
                    PCWSTR::from_raw(wide.as_ptr()),
                    access,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            match r {
                Ok(h) => return h,
                Err(e) => {
                    if Instant::now() > deadline {
                        panic!("open {} failed: {}", path, e);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Ok(n) with n == 0 when nothing is available; Err when the pipe is closed.
    fn read_some(h: HANDLE, buf: &mut [u8]) -> Result<usize, u32> {
        let mut avail: u32 = 0;
        unsafe {
            PeekNamedPipe(h, None, 0, None, Some(&mut avail), None)
                .map_err(|e| e.code().0 as u32)?;
        }
        if avail == 0 {
            return Ok(0);
        }
        let want = (avail as usize).min(buf.len());
        let mut read: u32 = 0;
        unsafe {
            ReadFile(h, Some(&mut buf[..want]), Some(&mut read), None)
                .map_err(|e| e.code().0 as u32)?;
        }
        Ok(read as usize)
    }

    /// Detaching while output is flowing must disconnect the client promptly and
    /// leave the shell alive and reattachable.
    #[test]
    fn detach_under_load_keeps_shell() {
        let session = Session::new("unit-detach", 80, 25, "cmd.exe").unwrap();
        session.start_data_pipe_listener().unwrap();
        let (out_path, in_path) = session.data_pipe_paths();

        let h_out = open(&out_path, GENERIC_READ.0);
        let h_in = open(&in_path, GENERIC_WRITE.0);

        // Keep the shell busy producing output.
        let cmd = b"for /L %i in (1,1,1000000) do @echo LINE %i\r\n";
        let mut written: u32 = 0;
        unsafe { WriteFile(h_in, Some(cmd), Some(&mut written), None).unwrap() };

        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut total = 0usize;
        while Instant::now() < deadline && total < 4096 {
            match read_some(h_out, &mut buf) {
                Ok(n) => total += n,
                Err(_) => panic!("out pipe broke before detach"),
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(total >= 4096, "expected output before detach");

        session.detach_client();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < deadline {
            match read_some(h_out, &mut buf) {
                Ok(_) => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(closed, "detach did not disconnect the client");
        assert!(session.is_alive(), "shell exited during detach");

        // A real client process exits here, closing both of its handles.
        unsafe {
            let _ = CloseHandle(h_out);
            let _ = CloseHandle(h_in);
        }

        let h_out2 = open(&out_path, GENERIC_READ.0);
        let h_in2 = open(&in_path, GENERIC_WRITE.0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = 0usize;
        while Instant::now() < deadline && got == 0 {
            match read_some(h_out2, &mut buf) {
                Ok(n) => got += n,
                Err(_) => break,
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(got > 0, "no output after reattach");

        unsafe {
            let _ = CloseHandle(h_out2);
            let _ = CloseHandle(h_in2);
        }
        session.stop().unwrap();
    }
}
