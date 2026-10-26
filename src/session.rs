//! Session: a single PTY-backed shell session managed by the daemon.

use crate::pty::Pty;
use anyhow::Result;
use std::sync::{
    Arc, Mutex,
    mpsc::{channel, Sender},
};
use std::thread;
use std::time::Duration;
use uuid::Uuid;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
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
    Attached,  // a client is attached to the data pipe
    Detached,  // client detached; session still alive
    Stopped,   // session terminated
}

/// Shared handle to a single shell session.
pub struct Session {
    pub name: String,
    pub session_id: String,
    pub pty: Arc<Mutex<Pty>>,
    pub state: Arc<Mutex<SessionState>>,
    /// Data pipe server handle (listening for a client to attach).
    pub data_pipe_server: Arc<Mutex<Option<HANDLE>>>,
    /// Connected client data pipe handle (when attached).
    pub data_pipe_client: Arc<Mutex<Option<HANDLE>>>,
    /// Sender to signal the accept thread to stop.
    pub stop_tx: Mutex<Option<Sender<()>>>,
    /// Win32 thread id of the accept thread, used to cancel its blocking ConnectNamedPipe.
    pub accept_tid: Arc<Mutex<Option<u32>>>,
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
            data_pipe_server: Arc::new(Mutex::new(None)),
            data_pipe_client: Arc::new(Mutex::new(None)),
            stop_tx: Mutex::new(None),
            accept_tid: Arc::new(Mutex::new(None)),
        })
    }

    /// Data pipe path for this session.
    pub fn data_pipe_path(&self) -> String {
        format!("{}\\{}", DATA_PIPE_PREFIX, self.session_id)
    }

    /// Start listening on the data pipe for a client to attach.
    pub fn start_data_pipe_listener(&self) -> Result<()> {
        let path = self.data_pipe_path();
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(
            std::ffi::OsStr::new(&path),
        )
        .chain(Some(0))
        .collect();

        let h = unsafe {
            CreateNamedPipeW(
                windows::core::PCWSTR::from_raw(wide.as_ptr()),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                4096,
                4096,
                NMPWAIT_WAIT_FOREVER,
                None,
            )
        };
        if h.is_invalid() {
            anyhow::bail!("CreateNamedPipe (data) failed for {}", path);
        }

        let state = self.state.clone();
        let data_pipe_server = self.data_pipe_server.clone();
        let pty = self.pty.clone();
        let data_pipe_client = self.data_pipe_client.clone();
        let accept_tid = self.accept_tid.clone();
        let (tx, rx) = channel();
        *self.stop_tx.lock().unwrap() = Some(tx);

        let session_name = self.name.clone();
        let session_id = self.session_id.clone();

        thread::spawn(move || {
            // Publish this thread's id so stop() can cancel the blocking ConnectNamedPipe.
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *accept_tid.lock().unwrap() = Some(tid);

            let _ = accept_data_pipe(
                h,
                rx,
                state,
                data_pipe_server,
                pty,
                data_pipe_client,
                session_name,
                session_id,
            );

            // The accept thread owns the server handle; close it when it exits.
            unsafe {
                let _ = CloseHandle(h);
            }
        });

        Ok(())
    }

    /// Mark the session as detached (client disconnected).
    pub fn mark_detached(&self) {
        *self.state.lock().unwrap() = SessionState::Detached;
        if let Some(client) = self.data_pipe_client.lock().unwrap().take() {
            unsafe {
                let _ = DisconnectNamedPipe(client);
                let _ = CloseHandle(client);
            }
        }
    }

    /// Stop the session: detach client, stop listener, mark stopped.
    pub fn stop(&self) -> Result<()> {
        self.mark_detached();

        // Signal the accept thread to stop.
        if let Some(tx) = self.stop_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }

        // Wake the accept thread out of its blocking ConnectNamedPipe.
        // Do NOT call DisconnectNamedPipe here: it would deadlock against the
        // pending ConnectNamedPipe in the accept thread.
        let tid = *self.accept_tid.lock().unwrap();
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

        *self.state.lock().unwrap() = SessionState::Stopped;
        Ok(())
    }
}

fn accept_data_pipe(
    server: HANDLE,
    stop_rx: std::sync::mpsc::Receiver<()>,
    state: Arc<Mutex<SessionState>>,
    data_pipe_server: Arc<Mutex<Option<HANDLE>>>,
    pty: Arc<Mutex<Pty>>,
    data_pipe_client: Arc<Mutex<Option<HANDLE>>>,
    _session_name: String,
    _session_id: String,
) {
    *data_pipe_server.lock().unwrap() = Some(server);

    loop {
        // Check if we should stop.
        if stop_rx.try_recv().is_ok() {
            break;
        }

        eprintln!("[session {}] waiting for client on data pipe", _session_name);
        let connected = unsafe {
            crate::protocol::is_pipe_connected(&ConnectNamedPipe(server, None))
        };
        if !connected {
            // Pipe disconnected or error; check stop again briefly.
            if stop_rx.recv_timeout(Duration::from_millis(100)).is_ok() {
                break;
            }
            continue;
        }

        // Client attached.
        *state.lock().unwrap() = SessionState::Attached;
        *data_pipe_client.lock().unwrap() = Some(server);

        let client_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let pty_out = pty.clone();
        let pty_in = pty.clone();
        let state_clone = state.clone();
        let client_clone = data_pipe_client.clone();
        let client = server;

        // Forward PTY output -> data pipe.
        let alive_out = client_alive.clone();
        let out_thread = thread::spawn(move || {
            forward_pty_to_pipe(&pty_out, client, &alive_out);
        });

        // Forward data pipe input -> PTY.
        let state_clone2 = state_clone.clone();
        let alive_in = client_alive.clone();
        let in_thread = thread::spawn(move || {
            forward_pipe_to_pty(&pty_in, client, &alive_in);
        });

        // Wait for either direction to finish.
        let _ = out_thread.join();
        let _ = in_thread.join();

        // Client detached.
        *state_clone2.lock().unwrap() = SessionState::Detached;
        *client_clone.lock().unwrap() = None;
        unsafe {
            let _ = DisconnectNamedPipe(client);
            // Note: we don't CloseHandle(server) here; it will be reused for next attach.
        }
    }
}

/// Forward PTY output to the named pipe.
///
/// Uses PeekNamedPipe so the thread can notice a disconnected client
/// (`alive` becomes false) instead of blocking forever in ReadFile.
fn forward_pty_to_pipe(
    pty: &Arc<Mutex<Pty>>,
    pipe: HANDLE,
    alive: &Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let mut buf = [0u8; 4096];
    loop {
        let avail = {
            let p = pty.lock().unwrap();
            match p.available() {
                Ok(n) => n,
                // Broken pipe: child process exited.
                Err(_) => break,
            }
        };

        if avail > 0 {
            let n = match pty.lock().unwrap().read_output(&mut buf) {
                Ok(0) => break,
                Ok(n) => n as usize,
                Err(_) => break,
            };
            let mut pos: usize = 0;
            while pos < n {
                let mut written: u32 = 0;
                match unsafe {
                    WriteFile(
                        pipe,
                        Some(&buf[pos..n]),
                        Some(&mut written),
                        None,
                    )
                } {
                    Ok(()) if written > 0 => pos += written as usize,
                    Ok(()) => break,
                    // Client gone.
                    Err(_) => {
                        alive.store(false, Ordering::Relaxed);
                        return;
                    }
                }
            }
        } else {
            // No pending output; stop once the client is gone.
            if !alive.load(Ordering::Relaxed) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

/// Forward named pipe input to the PTY.
fn forward_pipe_to_pty(
    pty: &Arc<Mutex<Pty>>,
    pipe: HANDLE,
    alive: &Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    let mut buf = [0u8; 4096];
    loop {
        let mut read: u32 = 0;
        let result = unsafe {
            ReadFile(
                pipe,
                Some(&mut buf),
                Some(&mut read),
                None,
            )
        };
        match result {
            Ok(()) if read == 0 => break, // EOF: client disconnected
            Ok(()) => {
                let _ = pty.lock().unwrap().write_input(&buf[..read as usize]);
            }
            Err(_) => break,
        }
    }
    // Signal the output thread that the client is gone.
    alive.store(false, Ordering::Relaxed);
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
