//! Daemon: the long-running smd process.
//!
//! Maintains a session table and serves control-pipe requests (START / CONTINUE / LIST / STOP).

use crate::protocol::{
    control_pipe, read_exact_pipe, write_all_pipe, Op, Request, Response, SessionInfo,
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
};
use crate::session::{Session, SessionState};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, ConnectNamedPipe, DisconnectNamedPipe,
    PIPE_TYPE_BYTE, PIPE_READMODE_BYTE, PIPE_WAIT,
    PIPE_UNLIMITED_INSTANCES, NMPWAIT_WAIT_FOREVER,
};

/// Shared session table: session name -> Session.
type SessionTable = Arc<Mutex<HashMap<String, Session>>>;

/// Session name used most recently, so bare `sm` can reattach without an argument.
type LastSession = Arc<Mutex<Option<String>>>;

pub struct Daemon {
    sessions: SessionTable,
    last: LastSession,
    control_pipe: HANDLE,
}

impl Daemon {
    pub fn new() -> Result<Self> {
        let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(
            std::ffi::OsStr::new(control_pipe()),
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
            last: Arc::new(Mutex::new(None)),
            control_pipe: h,
        })
    }

    /// Main loop: accept control-pipe connections and serve requests.
    ///
    /// Connections are handled serially: each request is short-lived
    /// (START/CONTINUE/LIST/STOP), so no per-connection thread is needed.
    pub fn run(&mut self) -> Result<()> {
        println!("smd listening on {}", control_pipe());

        loop {
            let connected = unsafe { crate::protocol::is_pipe_connected(&ConnectNamedPipe(self.control_pipe, None)) };
            if !connected {
                // A failed ConnectNamedPipe (e.g. ERROR_NO_DATA) leaves this
                // instance unusable, so disconnect before retrying. Back off to
                // avoid spinning at 100% CPU on a persistent error.
                unsafe {
                    let _ = DisconnectNamedPipe(self.control_pipe);
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }

            let _ = handle_control_connection(
                self.control_pipe,
                self.sessions.clone(),
                self.last.clone(),
            );
            unsafe { DisconnectNamedPipe(self.control_pipe) }.ok();
        }
    }
}

/// Handle one control-pipe connection: read one request, send one response, disconnect.
fn handle_control_connection(pipe: HANDLE, sessions: SessionTable, last: LastSession) -> Result<()> {
    // Read length prefix (4 bytes LE).
    let mut len_buf = [0u8; 4];
    if let Err(e) = read_exact_pipe(pipe, &mut len_buf) {
        // The client may already be gone; a failed error report must not mask
        // the original failure.
        let _ = write_response(pipe, &Response::err(format!("failed to read request length: {:#}", e)));
        return Err(e).context("failed to read request length");
    }

    let req_len = u32::from_le_bytes(len_buf) as usize;
    if req_len > MAX_REQUEST_BYTES {
        // Reject before allocating: a bogus prefix must not let a local process
        // make smd reserve gigabytes.
        let msg = format!(
            "request too large: {} bytes (limit {} bytes)",
            req_len, MAX_REQUEST_BYTES
        );
        let _ = write_response(pipe, &Response::err(&msg));
        anyhow::bail!("{}", msg);
    }

    let mut req_buf = vec![0u8; req_len];
    if let Err(e) = read_exact_pipe(pipe, &mut req_buf) {
        let _ = write_response(pipe, &Response::err(format!("failed to read request body: {:#}", e)));
        return Err(e).context("failed to read request body");
    }

    let req: Request = match serde_json::from_slice(&req_buf) {
        Ok(req) => req,
        Err(e) => {
            let _ = write_response(pipe, &Response::err(format!("failed to parse request JSON: {}", e)));
            return Err(e).context("failed to parse request JSON");
        }
    };

    let response = dispatch(&req, &sessions, &last);
    write_response(pipe, &response)?;
    Ok(())
}

/// Serialize and write one response, then flush it before the caller disconnects.
fn write_response(pipe: HANDLE, response: &Response) -> Result<()> {
    let payload = serde_json::to_vec(response)?;
    if payload.len() > MAX_RESPONSE_BYTES {
        anyhow::bail!(
            "response too large: {} bytes (limit {} bytes)",
            payload.len(),
            MAX_RESPONSE_BYTES
        );
    }

    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&payload);

    write_all_pipe(pipe, &buf)?;

    // Ensure the response is delivered before the caller disconnects the pipe;
    // otherwise DisconnectNamedPipe may discard buffered data the client hasn't read yet.
    unsafe {
        FlushFileBuffers(pipe)
            .map_err(|e| anyhow::anyhow!("FlushFileBuffers failed: {}", e))?;
    }

    Ok(())
}

/// Dispatch a request to the appropriate session operation.
fn dispatch(req: &Request, sessions: &SessionTable, last: &LastSession) -> Response {
    match req.op {
        Op::Start => handle_start(&req.name, req.wsl, req.cols, req.rows, sessions, last),
        Op::Continue => handle_continue(&req.name, req.cols, req.rows, sessions, last),
        Op::List => handle_list(sessions),
        Op::Stop => handle_stop(&req.name, sessions, last),
        Op::Resize => handle_resize(&req.name, req.cols, req.rows, sessions),
        Op::Detach => handle_detach(&req.name, sessions),
        Op::Last => handle_last(last),
        Op::PasteImage => handle_paste_image(),
    }
}

/// Publish the client-staged image on the clipboard.
fn handle_paste_image() -> Response {
    match crate::clipboard::install_latest_image() {
        Ok(()) => {
            crate::log::info("paste image installed into clipboard");
            Response::ok(None)
        }
        Err(e) => {
            let msg = format!("failed to install clipboard image: {:#}", e);
            crate::log::info(&msg);
            Response::err(msg)
        }
    }
}

/// Name of the most recently used session, for bare `sm`.
fn handle_last(last: &LastSession) -> Response {
    match last.lock().unwrap().clone() {
        Some(name) => Response::ok(Some(serde_json::json!({ "name": name }))),
        None => Response::err("no sessions yet"),
    }
}

fn handle_start(
    name: &str,
    wsl: Option<bool>,
    cols: Option<u16>,
    rows: Option<u16>,
    sessions: &SessionTable,
    last: &LastSession,
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

    let (c, r) = (cols.unwrap_or(120), rows.unwrap_or(40));

    // A default session walks the candidate shells until one starts. The first
    // candidate that exists is not necessarily the first one that works: a
    // Microsoft Store pwsh is an MSIX alias, which only activates in an
    // interactive session.
    let shells: Vec<String> = if wsl.unwrap_or(false) {
        vec!["wsl.exe".to_string()]
    } else {
        crate::shell::shell_candidates()
    };

    let mut started: Option<(Session, String)> = None;
    let mut last_err = None;
    for shell in &shells {
        match Session::new(name, c, r, shell) {
            Ok(session) => {
                started = Some((session, shell.clone()));
                break;
            }
            Err(e) => {
                crate::log::info(&format!(
                    "start name={} shell={} failed: {:#}",
                    name, shell, e
                ));
                last_err = Some(e);
            }
        }
    }

    match started {
        Some((session, shell)) => {
            crate::log::info(&format!(
                "start name={} shell={} size={}x{}",
                name, shell, c, r
            ));
            let session_id = session.session_id.clone();
            let (pipe_out, pipe_in) = session.data_pipe_paths();
            if let Err(e) = session.start_data_pipe_listener() {
                return Response::err(format!("failed to start data pipe: {}", e));
            }
            table.insert(name.to_string(), session);
            *last.lock().unwrap() = Some(name.to_string());

            let data = serde_json::json!({
                "session_id": session_id,
                "pipe_out": pipe_out,
                "pipe_in": pipe_in,
                "mouse": 0u8,
            });
            Response::ok(Some(data))
        }
        None => Response::err(format!(
            "failed to create session: {}",
            last_err
                .map(|e| format!("{:#}", e))
                .unwrap_or_else(|| "no shell candidates".to_string())
        )),
    }
}

fn handle_continue(
    name: &str,
    cols: Option<u16>,
    rows: Option<u16>,
    sessions: &SessionTable,
    last: &LastSession,
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
    let mouse = session.mouse_state();
    *last.lock().unwrap() = Some(name.to_string());

    let data = serde_json::json!({
        "session_id": session_id,
        "pipe_out": pipe_out,
        "pipe_in": pipe_in,
        "mouse": mouse,
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

/// Disconnect the attached client without stopping the session.
fn handle_detach(name: &str, sessions: &SessionTable) -> Response {
    {
        let table = sessions.lock().unwrap();
        match table.get(name) {
            None => return Response::err(format!("session '{}' not found", name)),
            Some(session) => {
                if session.is_alive() {
                    session.detach_client();
                    return Response::ok(None);
                }
            }
        }
    }

    // Mirror CONTINUE: a dead shell cannot be reattached, so reap it.
    let removed = { sessions.lock().unwrap().remove(name) };
    drop(removed);
    Response::err(format!("session '{}' has exited", name))
}

fn handle_stop(name: &str, sessions: &SessionTable, last: &LastSession) -> Response {
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
        let mut last = last.lock().unwrap();
        if last.as_deref() == Some(name) {
            *last = None;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Pipes::CreatePipe;

    /// An announced request length above the cap must be rejected before any
    /// body buffer is allocated, and must not block waiting for the body.
    #[test]
    fn oversized_request_length_is_rejected() {
        let mut read_end = HANDLE::default();
        let mut write_end = HANDLE::default();
        unsafe { CreatePipe(&mut read_end, &mut write_end, None, 0) }.expect("CreatePipe");

        let len = (MAX_REQUEST_BYTES as u32 + 1).to_le_bytes();
        let mut written = 0u32;
        unsafe { WriteFile(write_end, Some(&len), Some(&mut written), None) }.expect("WriteFile");
        assert_eq!(written, 4);

        let sessions: SessionTable = Arc::new(Mutex::new(HashMap::new()));
        let last: LastSession = Arc::new(Mutex::new(None));

        let err = handle_control_connection(read_end, sessions, last)
            .expect_err("oversized request length must be rejected");
        assert!(
            err.to_string().contains("request too large"),
            "unexpected error: {:#}",
            err
        );

        unsafe {
            let _ = CloseHandle(write_end);
            let _ = CloseHandle(read_end);
        }
    }
}
