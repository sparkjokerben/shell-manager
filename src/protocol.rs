use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::OnceLock;
use windows::Win32::Foundation::HANDLE;

/// Environment variable that overrides the instance namespace.
const INSTANCE_ENV: &str = "SM_INSTANCE";

static INSTANCE_NAME: OnceLock<String> = OnceLock::new();
static CONTROL_PIPE_CACHE: OnceLock<String> = OnceLock::new();
static DATA_PIPE_PREFIX_CACHE: OnceLock<String> = OnceLock::new();

/// Pipe names for one instance. `prod` keeps the historical namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipePaths {
    pub control: String,
    /// Data pipe prefix: `<prefix>\<session_id>\out` / `<prefix>\<session_id>\in`.
    pub data: String,
}

/// Normalize and validate an `SM_INSTANCE` value.
///
/// Returns the lowercased, trimmed name, or `None` when the value is not a
/// legal namespace component. Callers must ignore `None` rather than falling
/// back to the production namespace, so a typo can never reconnect a dev build
/// to the production daemon.
pub fn valid_instance(raw: &str) -> Option<String> {
    let name = raw.trim().to_ascii_lowercase();
    if name.is_empty() || name.len() > 32 {
        return None;
    }
    if name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        Some(name)
    } else {
        None
    }
}

/// Resolve the instance name from the environment and executable path.
///
/// Pure so the precedence rules can be unit-tested; `instance()` supplies the
/// real environment and `current_exe()`.
pub fn resolve_instance(env: Option<&str>, exe: Option<&Path>) -> String {
    if let Some(name) = env.and_then(valid_instance) {
        return name;
    }
    match exe {
        Some(exe) if is_under_target(exe) => "dev".to_string(),
        _ => "prod".to_string(),
    }
}

/// Whether `exe` is inside `<CARGO_MANIFEST_DIR>/target`.
///
/// Comparison is case-insensitive and treats `/` and `\` alike, since Windows
/// paths can arrive in either form.
fn is_under_target(exe: &Path) -> bool {
    let target = normalize_path(env!("CARGO_MANIFEST_DIR")) + r"\target";
    let exe = normalize_path(exe.to_string_lossy().as_ref());
    exe.starts_with(&(target + r"\"))
}

fn normalize_path(path: &str) -> String {
    path.replace('/', r"\").to_ascii_lowercase()
}

/// Namespace of this process, resolved once.
///
/// The installed production daemon and builds run straight out of `target/`
/// must never share pipes, the single-instance mutex or logs, so every process
/// resolves an instance name first:
///
/// 1. `SM_INSTANCE`, trimmed and lowercased, if it is 1-32 chars of
///    `[A-Za-z0-9_-]`.
/// 2. `dev` when the executable lives under `<CARGO_MANIFEST_DIR>/target`.
/// 3. `prod` otherwise.
///
/// `prod` keeps the historical names, so an already-running production daemon
/// is untouched by an upgrade.
pub fn instance() -> &'static str {
    INSTANCE_NAME.get_or_init(|| {
        let raw = std::env::var(INSTANCE_ENV).ok();
        if let Some(value) = raw.as_deref() {
            if valid_instance(value).is_none() {
                eprintln!(
                    "warning: ignoring invalid {}={:?} (expected 1-32 chars of [A-Za-z0-9_-])",
                    INSTANCE_ENV, value
                );
            }
        }
        resolve_instance(raw.as_deref(), std::env::current_exe().ok().as_deref())
    })
}

/// True when this process uses the production namespace.
pub fn is_prod() -> bool {
    instance() == "prod"
}

/// IPC control pipe path shared between the sm client and smd.
pub fn control_pipe() -> &'static str {
    CONTROL_PIPE_CACHE.get_or_init(|| pipe_paths_for(instance()).control)
}

/// Data pipe prefix: `<prefix>\<session_id>`.
pub fn data_pipe_prefix() -> &'static str {
    DATA_PIPE_PREFIX_CACHE.get_or_init(|| pipe_paths_for(instance()).data)
}

/// Pipe names for `instance`, without any global state.
pub fn pipe_paths_for(instance: &str) -> PipePaths {
    if instance == "prod" {
        PipePaths {
            control: r"\\.\pipe\shell-manager\control".to_string(),
            data: r"\\.\pipe\shell-manager\data".to_string(),
        }
    } else {
        PipePaths {
            control: format!(r"\\.\pipe\shell-manager-{}\control", instance),
            data: format!(r"\\.\pipe\shell-manager-{}\data", instance),
        }
    }
}

/// `(global, local)` single-instance mutex names for `instance`.
pub fn mutex_names_for(instance: &str) -> (String, String) {
    if instance == "prod" {
        (
            r"Global\smd-single-instance".to_string(),
            r"Local\smd-single-instance".to_string(),
        )
    } else {
        (
            format!(r"Global\smd-single-instance-{}", instance),
            format!(r"Local\smd-single-instance-{}", instance),
        )
    }
}

/// Daemon log file name for `instance`, placed in `%TEMP%`.
pub fn daemon_log_file_for(instance: &str) -> String {
    if instance == "prod" {
        "smd.log".to_string()
    } else {
        format!("smd-{}.log", instance)
    }
}

/// Client log file name for `instance`, placed in `%TEMP%`.
pub fn client_log_file_for(instance: &str) -> String {
    if instance == "prod" {
        "sm-client.log".to_string()
    } else {
        format!("sm-client-{}.log", instance)
    }
}

/// Web bridge log file name for `instance`, placed in `%TEMP%`.
pub fn smw_log_file_for(instance: &str) -> String {
    if instance == "prod" {
        "smw.log".to_string()
    } else {
        format!("smw-{}.log", instance)
    }
}

/// Daemon log file name for this process.
pub fn daemon_log_file() -> String {
    daemon_log_file_for(instance())
}

/// Web bridge log file name for this process.
pub fn smw_log_file() -> String {
    smw_log_file_for(instance())
}

/// Client log file name for this process.
pub fn client_log_file() -> String {
    client_log_file_for(instance())
}

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
    /// Put the image staged by the client's paste watcher on the clipboard.
    PasteImage,
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

/// Closes a Win32 handle when dropped, so every error path in
/// [`pipe_request`] releases it.
struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Send a JSON request over a named pipe and read a JSON response.
///
/// Wire format: 4-byte little-endian length prefix, then UTF-8 JSON payload.
pub fn pipe_request(pipe: &str, req: &Request) -> anyhow::Result<Response> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
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

    let h = OwnedHandle(h);

    // Set pipe to byte mode
    unsafe {
        let mut mode: NAMED_PIPE_MODE = PIPE_READMODE_BYTE | PIPE_WAIT;
        SetNamedPipeHandleState(h.0, Some(&mut mode), None, None)?;
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
    write_all_pipe(h.0, &buf)?;

    // Read length prefix (4 bytes).
    let mut len_buf = [0u8; 4];
    read_exact_pipe(h.0, &mut len_buf)?;
    let resp_len = u32::from_le_bytes(len_buf) as usize;
    if resp_len > MAX_RESPONSE_BYTES {
        anyhow::bail!(
            "response too large: {} bytes (limit {} bytes)",
            resp_len,
            MAX_RESPONSE_BYTES
        );
    }

    let mut resp_buf = vec![0u8; resp_len];
    read_exact_pipe(h.0, &mut resp_buf)?;

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

    #[test]
    fn production_paths_are_unchanged() {
        let paths = pipe_paths_for("prod");
        assert_eq!(paths.control, r"\\.\pipe\shell-manager\control");
        assert_eq!(paths.data, r"\\.\pipe\shell-manager\data");
        assert_eq!(
            mutex_names_for("prod"),
            (
                r"Global\smd-single-instance".to_string(),
                r"Local\smd-single-instance".to_string(),
            )
        );
        assert_eq!(daemon_log_file_for("prod"), "smd.log");
        assert_eq!(client_log_file_for("prod"), "sm-client.log");
        assert_eq!(smw_log_file_for("prod"), "smw.log");
    }

    #[test]
    fn development_paths_use_the_dev_namespace() {
        let paths = pipe_paths_for("dev");
        assert_eq!(paths.control, r"\\.\pipe\shell-manager-dev\control");
        assert_eq!(paths.data, r"\\.\pipe\shell-manager-dev\data");
        assert_eq!(
            mutex_names_for("dev"),
            (
                r"Global\smd-single-instance-dev".to_string(),
                r"Local\smd-single-instance-dev".to_string(),
            )
        );
        assert_eq!(daemon_log_file_for("dev"), "smd-dev.log");
        assert_eq!(client_log_file_for("dev"), "sm-client-dev.log");
        assert_eq!(smw_log_file_for("dev"), "smw-dev.log");
    }

    #[test]
    fn custom_instance_paths_are_namespaced() {
        let paths = pipe_paths_for("my_test-1");
        assert_eq!(paths.control, r"\\.\pipe\shell-manager-my_test-1\control");
        assert_eq!(paths.data, r"\\.\pipe\shell-manager-my_test-1\data");
        assert_eq!(daemon_log_file_for("my_test-1"), "smd-my_test-1.log");
    }

    #[test]
    fn valid_instance_normalizes_case_and_whitespace() {
        assert_eq!(valid_instance("  DEV  "), Some("dev".into()));
        assert_eq!(valid_instance("a_b-9"), Some("a_b-9".into()));
        assert_eq!(valid_instance("x".repeat(32).as_str()), Some("x".repeat(32)));
    }

    #[test]
    fn invalid_instance_values_are_rejected() {
        assert_eq!(valid_instance(""), None);
        assert_eq!(valid_instance("   "), None);
        assert_eq!(valid_instance("x".repeat(33).as_str()), None);
        assert_eq!(valid_instance("bad name"), None);
        assert_eq!(valid_instance("bad\\name"), None);
        assert_eq!(valid_instance("bad/name"), None);
        assert_eq!(valid_instance("bad.name"), None);
        assert_eq!(valid_instance("生产"), None);
    }

    fn manifest_exe(relative: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
    }

    #[test]
    fn environment_override_wins() {
        let exe = manifest_exe(r"target\debug\sm.exe");
        assert_eq!(resolve_instance(Some("Staging"), Some(&exe)), "staging");
        let outside = manifest_exe(r"bin\sm.exe");
        assert_eq!(resolve_instance(Some("staging"), Some(&outside)), "staging");
    }

    #[test]
    fn invalid_environment_falls_through_to_path_rules() {
        let inside = manifest_exe(r"target\debug\sm.exe");
        let outside = manifest_exe(r"bin\sm.exe");
        assert_eq!(resolve_instance(Some("bad name"), Some(&inside)), "dev");
        assert_eq!(resolve_instance(Some("bad name"), Some(&outside)), "prod");
        assert_eq!(resolve_instance(None, Some(&outside)), "prod");
    }

    #[test]
    fn executables_under_target_resolve_to_dev() {
        let inside = manifest_exe(r"target\debug\sm.exe");
        assert_eq!(resolve_instance(None, Some(&inside)), "dev");

        // Separators and case do not matter on Windows.
        let mixed = manifest_exe(r"TARGET\Debug\sm.exe");
        assert_eq!(resolve_instance(None, Some(&mixed)), "dev");
        let slashes = manifest_exe(r"target/debug/sm.exe");
        assert_eq!(resolve_instance(None, Some(&slashes)), "dev");

        // A sibling directory whose name merely starts with "target" is not it.
        let sibling = manifest_exe(r"target-dev\sm.exe");
        assert_eq!(resolve_instance(None, Some(&sibling)), "prod");
    }

    #[test]
    fn executables_outside_target_resolve_to_prod() {
        let outside = manifest_exe(r"bin\sm.exe");
        assert_eq!(resolve_instance(None, Some(&outside)), "prod");
        assert_eq!(resolve_instance(None, None), "prod");
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
