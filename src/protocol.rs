use anyhow::Context;
use serde::{Deserialize, Serialize};

/// IPC control pipe path shared between sd client and shell-daemon.
pub const CONTROL_PIPE: &str = r"\\.\pipe\shell-daemon\control";

/// Data pipe prefix: \\.\pipe\shell-daemon\data\<session_id>
pub const DATA_PIPE_PREFIX: &str = r"\\.\pipe\shell-daemon\data";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Start,
    Continue,
    List,
    Stop,
    Resize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub op: Op,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wsl: Option<bool>,
    /// Client terminal width/height (columns, rows), if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub session_id: String,
    pub attached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(data: Option<serde_json::Value>) -> Self {
        Self { ok: true, error: String::new(), data }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: msg.into(), data: None }
    }
}

/// True when a `ConnectNamedPipe` result means the client is connected.
///
/// `ConnectNamedPipe` reports `ERROR_PIPE_CONNECTED` (Win32 535,
/// HRESULT 0x80070217) when a client connected before the server called it.
/// That is a successful connection, not a failure.
pub fn is_pipe_connected(result: &windows::core::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(e) => (e.code().0 as u32) == 0x8007_0217u32,
    }
}

/// Send a JSON request over a named pipe and read a JSON response.
///
/// Wire format: 4-byte little-endian length prefix, then UTF-8 JSON payload.
pub fn pipe_request(pipe: &str, req: &Request) -> anyhow::Result<Response> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::FILE_SHARE_MODE;
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, OPEN_EXISTING, SECURITY_SQOS_PRESENT, SECURITY_IDENTIFICATION,
        ReadFile, WriteFile,
    };
    use windows::Win32::System::Pipes::{
        PIPE_READMODE_BYTE, PIPE_WAIT, NAMED_PIPE_MODE, SetNamedPipeHandleState,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = std::ffi::OsStr::new(pipe).encode_wide().chain(Some(0)).collect();
    let h = unsafe {
        CreateFileW(
            PCWSTR::from_raw(wide.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
            None,
        )
    };
    let h = match h {
        Ok(h) => h,
        Err(_) => {
            anyhow::bail!("failed to connect to control pipe (is shell-daemon running?)");
        }
    };

    // Set pipe to byte mode
    unsafe {
        let mut mode: NAMED_PIPE_MODE = PIPE_READMODE_BYTE | PIPE_WAIT;
        SetNamedPipeHandleState(h, Some(&mut mode), None, None)?;
    }

    let payload = serde_json::to_vec(req)?;
    let payload_len = payload.len() as u32;

    // Write length prefix (4 bytes LE) then payload
    let mut buf = Vec::with_capacity(4 + payload_len as usize);
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(&payload);

    let mut written: u32 = 0;
    unsafe {
        WriteFile(
            h,
            Some(&buf),
            Some(&mut written),
            None,
        )
        .context("WriteFile failed")?;
    }

    // Read length prefix (4 bytes)
    let mut len_buf = [0u8; 4];
    let mut read: u32 = 0;
    unsafe {
        ReadFile(
            h,
            Some(&mut len_buf),
            Some(&mut read),
            None,
        )
        .context("ReadFile length failed")?;
    }
    if read < 4 {
        anyhow::bail!("short read on response length");
    }
    let resp_len = u32::from_le_bytes(len_buf) as usize;

    let mut resp_buf = vec![0u8; resp_len];
    let mut read: u32 = 0;
    unsafe {
        ReadFile(
            h,
            Some(&mut resp_buf),
            Some(&mut read),
            None,
        )
        .context("ReadFile payload failed")?;
    }

    unsafe {
        CloseHandle(h).context("CloseHandle failed")?;
    }

    let resp: Response = serde_json::from_slice(&resp_buf)?;
    Ok(resp)
}
