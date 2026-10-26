use anyhow::Context;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::HANDLE;

/// IPC control pipe path shared between the sm client and smd.
pub const CONTROL_PIPE: &str = r"\\.\pipe\shell-manager\control";

/// Data pipe prefix: \\.\pipe\shell-manager\data\<session_id>
pub const DATA_PIPE_PREFIX: &str = r"\\.\pipe\shell-manager\data";

/// Upper bound on a control-pipe request body (1 MiB).
///
/// The daemon allocates a buffer of the announced length, so a bogus or
/// truncated length prefix must be rejected before allocating.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;

/// Upper bound on a control-pipe response body (16 MiB).
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Op {
    Start,
    Continue,
    List,
    Stop,
    Resize,
    Detach,
    /// Name of the session used most recently, for bare `sm` invocations.
    Last,
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

/// Read exactly `buf.len()` bytes from a pipe.
///
/// Pipes in byte mode may return fewer bytes than requested even when blocking,
/// so a single `ReadFile` is not enough. A zero-byte read means the peer closed
/// the pipe and is reported as an error rather than silently truncating.
pub fn read_exact_pipe(pipe: HANDLE, buf: &mut [u8]) -> anyhow::Result<()> {
    use windows::Win32::Storage::FileSystem::ReadFile;

    let mut pos = 0;
    while pos < buf.len() {
        let mut read: u32 = 0;
        unsafe { ReadFile(pipe, Some(&mut buf[pos..]), Some(&mut read), None) }
            .context("ReadFile failed")?;
        if read == 0 {
            anyhow::bail!(
                "pipe closed while reading ({} of {} bytes read)",
                pos,
                buf.len()
            );
        }
        pos += read as usize;
    }
    Ok(())
}

/// Write the whole buffer to a pipe, looping over short writes.
///
/// A `WriteFile` that reports zero bytes written is treated as an error so a
/// broken peer fails loudly instead of spinning.
pub fn write_all_pipe(pipe: HANDLE, data: &[u8]) -> anyhow::Result<()> {
    use windows::Win32::Storage::FileSystem::WriteFile;

    let mut pos = 0;
    while pos < data.len() {
        let mut written: u32 = 0;
        unsafe { WriteFile(pipe, Some(&data[pos..]), Some(&mut written), None) }
            .context("WriteFile failed")?;
        if written == 0 {
            anyhow::bail!(
                "pipe write made no progress ({} of {} bytes written)",
                pos,
                data.len()
            );
        }
        pos += written as usize;
    }
    Ok(())
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
    };
    use windows::Win32::System::Pipes::{
        PIPE_READMODE_BYTE, PIPE_WAIT, NAMED_PIPE_MODE, SetNamedPipeHandleState, WaitNamedPipeW,
    };
    use windows::core::PCWSTR;

    let wide: Vec<u16> = std::ffi::OsStr::new(pipe).encode_wide().chain(Some(0)).collect();
    // The daemon serves one connection at a time: between a response and the next
    // ConnectNamedPipe the instance is briefly unavailable, and a client that
    // sends two requests back to back (attach-or-create) would otherwise lose the
    // second one with ERROR_PIPE_BUSY.
    let mut attempts = 0;
    let h = loop {
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
        match h {
            Ok(h) => break h,
            Err(e) => {
                let code = e.code().0 as u32;
                // ERROR_PIPE_BUSY / ERROR_FILE_NOT_FOUND (the daemon is between
                // DisconnectNamedPipe and ConnectNamedPipe, or still starting).
                let retryable = code == 0x8007_00E7 || code == 0x8007_0002;
                if !retryable || attempts >= 40 {
                    anyhow::bail!("failed to connect to control pipe: {} (is smd running?)", e);
                }
                attempts += 1;
                if code == 0x8007_00E7 {
                    unsafe {
                        let _ = WaitNamedPipeW(PCWSTR::from_raw(wide.as_ptr()), 250);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    };

    // Set pipe to byte mode
    unsafe {
        let mut mode: NAMED_PIPE_MODE = PIPE_READMODE_BYTE | PIPE_WAIT;
        SetNamedPipeHandleState(h, Some(&mut mode), None, None)?;
    }

    let payload = serde_json::to_vec(req)?;
    if payload.len() > MAX_REQUEST_BYTES {
        anyhow::bail!(
            "request too large: {} bytes (limit {} bytes)",
            payload.len(),
            MAX_REQUEST_BYTES
        );
    }
    let payload_len = payload.len() as u32;

    // Write length prefix (4 bytes LE) then payload.
    let mut buf = Vec::with_capacity(4 + payload_len as usize);
    buf.extend_from_slice(&payload_len.to_le_bytes());
    buf.extend_from_slice(&payload);
    write_all_pipe(h, &buf)?;

    // Read length prefix (4 bytes).
    let mut len_buf = [0u8; 4];
    read_exact_pipe(h, &mut len_buf)?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;
    if resp_len > MAX_RESPONSE_BYTES {
        anyhow::bail!(
            "response too large: {} bytes (limit {} bytes)",
            resp_len,
            MAX_RESPONSE_BYTES
        );
    }

    let mut resp_buf = vec![0u8; resp_len];
    read_exact_pipe(h, &mut resp_buf)?;

    unsafe {
        CloseHandle(h).context("CloseHandle failed")?;
    }

    let resp: Response = serde_json::from_slice(&resp_buf)?;
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Storage::FileSystem::WriteFile;
    use windows::Win32::System::Pipes::CreatePipe;

    fn anonymous_pipe() -> (HANDLE, HANDLE) {
        let mut read_end = HANDLE::default();
        let mut write_end = HANDLE::default();
        unsafe { CreatePipe(&mut read_end, &mut write_end, None, 0) }.expect("CreatePipe");
        (read_end, write_end)
    }

    fn write_chunk(pipe: HANDLE, data: &[u8]) {
        let mut written = 0u32;
        unsafe { WriteFile(pipe, Some(data), Some(&mut written), None) }.expect("WriteFile");
        assert_eq!(written as usize, data.len());
    }

    /// Reads may return fewer bytes than requested even in blocking mode; the
    /// helper must keep reading until the buffer is full.
    #[test]
    fn read_exact_pipe_loops_over_short_reads() {
        let (read_end, write_end) = anonymous_pipe();

        // Prime the pipe with a short first chunk, then feed the rest later so
        // the first ReadFile is guaranteed to be a short read.
        write_chunk(write_end, b"ab");
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            write_chunk(write_end, b"cdefghi");
        });

        let mut buf = [0u8; 9];
        read_exact_pipe(read_end, &mut buf).expect("read_exact_pipe");
        assert_eq!(&buf, b"abcdefghi");

        writer.join().unwrap();
        unsafe {
            let _ = CloseHandle(write_end);
            let _ = CloseHandle(read_end);
        }
    }

    /// A peer that closes without sending all promised bytes must be an error,
    /// not a truncated success.
    #[test]
    fn read_exact_pipe_errors_when_pipe_closes_early() {
        let (read_end, write_end) = anonymous_pipe();
        write_chunk(write_end, b"ab");
        unsafe {
            let _ = CloseHandle(write_end);
        }

        let mut buf = [0u8; 4];
        assert!(read_exact_pipe(read_end, &mut buf).is_err());

        unsafe {
            let _ = CloseHandle(read_end);
        }
    }
}
