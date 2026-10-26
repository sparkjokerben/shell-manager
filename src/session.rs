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

        // In thread (client -> PTY).
        let in_tid_slot = in_tid.clone();
        let stop_in_thread = stop_in.clone();
        thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *in_tid_slot.lock().unwrap() = Some(tid);
            run_in_thread(in_h, pty_in, state_in, stop_in_thread, alive_in);
        });

        // Out thread (PTY -> client).
        thread::spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *out_tid.lock().unwrap() = Some(tid);
            run_out_thread(out_h, pty_out, state_out, stop_out, alive_out);
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

        // Read client input until the client disconnects.
        loop {
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

        'forward: loop {
            if stop.load(Ordering::Relaxed) {
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
