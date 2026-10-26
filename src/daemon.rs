//! Daemon: the long-running shell-daemon process.
//!
//! Maintains a session table and serves control-pipe requests (START / CONTINUE / LIST / STOP).

use crate::protocol::{Op, Request, Response, SessionInfo};
use crate::session::{Session, SessionState};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, ConnectNamedPipe, DisconnectNamedPipe,
    PIPE_TYPE_BYTE, PIPE_READMODE_BYTE, PIPE_WAIT,
    PIPE_UNLIMITED_INSTANCES, NMPWAIT_WAIT_FOREVER,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};

/// Shared session table: session name -> Session.
type SessionTable = Arc<Mutex<HashMap<String, Session>>>;

pub struct Daemon {
    sessions: SessionTable,
    control_pipe: HANDLE,
}

const CONTROL_PIPE: &str = r"\\.\pipe\shell-daemon\control";

impl Daemon {
    pub fn new() -> Result<Self> {
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(
            std::ffi::OsStr::new(CONTROL_PIPE),
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
            anyhow::bail!("CreateNamedPipe (control) failed");
        }

        Ok(Daemon {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            control_pipe: h,
        })
    }

    /// Main loop: accept control-pipe connections and serve requests.
    ///
    /// Connections are handled serially: each request is short-lived
    /// (START/CONTINUE/LIST/STOP), so no per-connection thread is needed.
    pub fn run(&mut self) -> Result<()> {
        println!("shell-daemon listening on {}", CONTROL_PIPE);

        loop {
            let connected = unsafe { crate::protocol::is_pipe_connected(&ConnectNamedPipe(self.control_pipe, None)) };
            if !connected {
                continue;
            }

            let _ = handle_control_connection(self.control_pipe, self.sessions.clone());
            unsafe { DisconnectNamedPipe(self.control_pipe) }.ok();
        }
    }
}

/// Handle one control-pipe connection: read one request, send one response, disconnect.
fn handle_control_connection(pipe: HANDLE, sessions: SessionTable) -> Result<()> {
    // Read length prefix (4 bytes LE).
    let mut len_buf = [0u8; 4];
    let mut read: u32 = 0;
    unsafe {
        ReadFile(
            pipe,
            Some(&mut len_buf),
            Some(&mut read),
            None,
        )
        .map_err(|e| anyhow::anyhow!("ReadFile failed: {}", e))?;
    }
    if read < 4 {
        anyhow::bail!("short read on request length");
    }
    let req_len = u32::from_le_bytes(len_buf) as usize;

    let mut req_buf = vec![0u8; req_len];
    unsafe {
        ReadFile(
            pipe,
            Some(&mut req_buf),
            Some(&mut read),
            None,
        )
        .map_err(|e| anyhow::anyhow!("ReadFile failed: {}", e))?;
    }

    let req: Request = serde_json::from_slice(&req_buf)
        .context("failed to parse request JSON")?;

    let response = dispatch(&req, &sessions);

    let payload = serde_json::to_vec(&response)?;
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&payload);

    let mut written: u32 = 0;
    unsafe {
        WriteFile(
            pipe,
            Some(&buf),
            Some(&mut written),
            None,
        )
        .map_err(|e| anyhow::anyhow!("WriteFile failed: {}", e))?;
    }

    // Ensure the response is delivered before the caller disconnects the pipe;
    // otherwise DisconnectNamedPipe may discard buffered data the client hasn't read yet.
    unsafe {
        windows::Win32::Storage::FileSystem::FlushFileBuffers(pipe)
            .map_err(|e| anyhow::anyhow!("FlushFileBuffers failed: {}", e))?;
    }

    Ok(())
}

/// Dispatch a request to the appropriate session operation.
fn dispatch(req: &Request, sessions: &SessionTable) -> Response {
    match req.op {
        Op::Start => handle_start(&req.name, req.wsl, req.cols, req.rows, sessions),
        Op::Continue => handle_continue(&req.name, req.cols, req.rows, sessions),
        Op::List => handle_list(sessions),
        Op::Stop => handle_stop(&req.name, sessions),
        Op::Resize => handle_resize(&req.name, req.cols, req.rows, sessions),
    }
}

fn handle_start(
    name: &str,
    wsl: Option<bool>,
    cols: Option<u16>,
    rows: Option<u16>,
    sessions: &SessionTable,
) -> Response {
    // A session whose shell exited is unusable; drop it before checking the name.
    let stale = {
        let table = sessions.lock().unwrap();
        table.get(name).map(|s| !s.is_alive()).unwrap_or(false)
    };
    if stale {
        let removed = { sessions.lock().unwrap().remove(name) };
        drop(removed);
    }

    let mut table = sessions.lock().unwrap();
    if table.contains_key(name) {
        return Response::err(format!("session '{}' already exists", name));
    }

    let cmd = if wsl.unwrap_or(false) {
        "wsl.exe".to_string()
    } else {
        // 默认 PowerShell 7 (pwsh.exe)，不使用 PowerShell 5.1 (powershell.exe)
        "pwsh.exe".to_string()
    };

    let (c, r) = (cols.unwrap_or(120), rows.unwrap_or(40));

    match Session::new(name, c, r, &cmd) {
        Ok(session) => {
            let session_id = session.session_id.clone();
            let (pipe_out, pipe_in) = session.data_pipe_paths();
            if let Err(e) = session.start_data_pipe_listener() {
                return Response::err(format!("failed to start data pipe: {}", e));
            }
            table.insert(name.to_string(), session);

            let data = serde_json::json!({
                "session_id": session_id,
                "pipe_out": pipe_out,
                "pipe_in": pipe_in,
            });
            Response::ok(Some(data))
        }
        Err(e) => Response::err(format!("failed to create session: {}", e)),
    }
}

fn handle_continue(
    name: &str,
    cols: Option<u16>,
    rows: Option<u16>,
    sessions: &SessionTable,
) -> Response {
    let mut table = sessions.lock().unwrap();
    let alive = match table.get(name) {
        Some(s) => s.is_alive(),
        None => return Response::err(format!("session '{}' not found", name)),
    };

    // Attaching to a dead shell would only produce an immediate disconnect.
    if !alive {
        drop(table);
        let removed = { sessions.lock().unwrap().remove(name) };
        drop(removed);
        return Response::err(format!("session '{}' has exited", name));
    }

    let session = match table.get_mut(name) {
        Some(s) => s,
        None => return Response::err(format!("session '{}' not found", name)),
    };

    // Match the existing shell to the client's terminal size.
    if let (Some(c), Some(r)) = (cols, rows) {
        if let Err(e) = session.resize(c, r) {
            eprintln!("[daemon] resize failed: {}", e);
        }
    }

    // The directional listener threads were started at session creation and
    // loop back to accept the next client, so nothing else is needed here.
    let session_id = session.session_id.clone();
    let (pipe_out, pipe_in) = session.data_pipe_paths();

    let data = serde_json::json!({
        "session_id": session_id,
        "pipe_out": pipe_out,
        "pipe_in": pipe_in,
    });
    Response::ok(Some(data))
}

fn handle_resize(
    name: &str,
    cols: Option<u16>,
    rows: Option<u16>,
    sessions: &SessionTable,
) -> Response {
    let table = sessions.lock().unwrap();
    let session = match table.get(name) {
        Some(s) => s,
        None => return Response::err(format!("session '{}' not found", name)),
    };
    match (cols, rows) {
        (Some(c), Some(r)) => match session.resize(c.max(1), r.max(1)) {
            Ok(()) => Response::ok(None),
            Err(e) => Response::err(format!("resize failed: {}", e)),
        },
        _ => Response::ok(None),
    }
}

fn handle_list(sessions: &SessionTable) -> Response {
    // Drop sessions whose shell has exited so the listing reflects reality.
    let dead: Vec<String> = {
        let table = sessions.lock().unwrap();
        table
            .iter()
            .filter(|(_, s)| !s.is_alive())
            .map(|(n, _)| n.clone())
            .collect()
    };
    if !dead.is_empty() {
        let removed: Vec<Session> = {
            let mut table = sessions.lock().unwrap();
            dead.iter().filter_map(|n| table.remove(n)).collect()
        };
        drop(removed);
    }

    let table = sessions.lock().unwrap();
    let infos: Vec<SessionInfo> = table
        .iter()
        .map(|(name, session)| {
            let attached = matches!(
                *session.state.lock().unwrap(),
                SessionState::Attached
            );
            SessionInfo {
                name: name.clone(),
                session_id: session.session_id.clone(),
                attached,
            }
        })
        .collect();

    Response::ok(Some(serde_json::to_value(infos).unwrap()))
}

fn handle_stop(name: &str, sessions: &SessionTable) -> Response {
    // Release the table lock before stopping/dropping the session.
    let removed = {
        let mut table = sessions.lock().unwrap();
        table.remove(name)
    };
    if let Some(session) = removed {
        if let Err(e) = session.stop() {
            return Response::err(format!("failed to stop session: {}", e));
        }
        drop(session);
        Response::ok(None)
    } else {
        Response::err(format!("session '{}' not found", name))
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.control_pipe);
        }
    }
}
