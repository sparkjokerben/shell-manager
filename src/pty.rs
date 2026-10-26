//! ConPTY pseudo-terminal wrapper for Windows.
//!
//! Uses CreatePseudoConsole + CreateProcessW with
//! PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE to host a shell child process
//! whose stdin/stdout/stderr are attached to a pseudo console.

use anyhow::{Context, Result};
use std::ffi::c_void;
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{TOKEN_DUPLICATE, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::ReadFile;
use windows::Win32::System::Console::{
    ClosePseudoConsole, CreatePseudoConsole, GetConsoleMode, GetStdHandle, SetStdHandle,
    CONSOLE_MODE, COORD, HPCON, STD_ERROR_HANDLE, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcessToken, CREATE_UNICODE_ENVIRONMENT, EXTENDED_STARTUPINFO_PRESENT,
    PROCESS_INFORMATION, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESHOWWINDOW, STARTUPINFOEXW,
};
use windows::Win32::System::Threading::{
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute, LPPROC_THREAD_ATTRIBUTE_LIST,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;

/// Terminal identity advertised to the hosted child.
///
/// ConPTY is a real VT terminal, but a process started inside it inherits only
/// the registry environment, where `TERM` is normally absent. Unicode-aware
/// programs use `TERM` on Windows to decide whether the terminal can render
/// Unicode; without it they replace symbols such as `○`, `□` and `☒` with
/// ASCII fallbacks (`()`, `[]`, `[x]`). Claude Code's TUI checks exactly this,
/// so declaring the pseudo console as an xterm-256color terminal is both
/// accurate and what keeps those programs on the Unicode path.
const TERM_ENTRY: &str = "TERM=xterm-256color";

/// Builds the user's own environment block (from the registry) for the child,
/// with the terminal identity added.
///
/// Otherwise the shell inherits whatever environment the daemon happened to be
/// started with - e.g. NO_COLOR and TERM from a parent terminal - which changes
/// how tools such as oh-my-posh render.
fn user_environment() -> Option<(Vec<u16>, HANDLE)> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_QUERY | TOKEN_DUPLICATE,
            &mut token,
        )
        .ok()?;

        let mut block: *mut c_void = ptr::null_mut();
        match CreateEnvironmentBlock(&mut block, token, false) {
            Ok(()) if !block.is_null() => {
                let raw = copy_environment_block(block as *const u16);
                let _ = DestroyEnvironmentBlock(block);
                Some((ensure_terminal_env(&raw), token))
            }
            _ => {
                let _ = CloseHandle(token);
                None
            }
        }
    }
}

/// Copy a UTF-16 environment block up to and including its double-NUL end.
///
/// The length is not returned by `CreateEnvironmentBlock`; the block is
/// terminated by an empty string followed by the block terminator.
unsafe fn copy_environment_block(block: *const u16) -> Vec<u16> {
    const LIMIT: usize = 1 << 20;
    let mut len = 0usize;
    while len + 1 < LIMIT {
        if *block.add(len) == 0 && *block.add(len + 1) == 0 {
            return std::slice::from_raw_parts(block, len + 2).to_vec();
        }
        len += 1;
    }
    Vec::new()
}

/// Return `block` with exactly one `TERM` entry, set to the pseudo console's
/// terminal type.
///
/// Environment variable names are case-insensitive on Windows, so an existing
/// `term=...` is replaced rather than duplicated. Other entries and the final
/// double NUL stay intact.
fn ensure_terminal_env(block: &[u16]) -> Vec<u16> {
    let mut entries: Vec<&[u16]> = Vec::new();
    let mut start = 0usize;
    for i in 0..block.len() {
        if block[i] != 0 {
            continue;
        }
        if i == start {
            break; // the empty string that terminates the block
        }
        entries.push(&block[start..=i]);
        start = i + 1;
    }

    let term: Vec<u16> = TERM_ENTRY.encode_utf16().chain(Some(0)).collect();
    let mut out = Vec::with_capacity(block.len() + term.len());
    let mut replaced = false;
    for entry in entries {
        if is_env_key(entry, "TERM") {
            if !replaced {
                out.extend_from_slice(&term);
                replaced = true;
            }
            continue;
        }
        out.extend_from_slice(entry);
    }
    if !replaced {
        out.extend_from_slice(&term);
    }
    out.push(0);
    out
}

/// Whether a UTF-16 environment entry defines `key` (ASCII, case-insensitive).
fn is_env_key(entry: &[u16], key: &str) -> bool {
    for (i, expected) in key.bytes().enumerate() {
        let Some(&actual) = entry.get(i) else {
            return false;
        };
        if actual > 0x7f || (actual as u8).to_ascii_lowercase() != expected.to_ascii_lowercase() {
            return false;
        }
    }
    entry.get(key.len()) == Some(&(b'=' as u16))
}

/// A pseudo-terminal hosting a shell process.
///
/// The PTY owns:
/// - `hpc`: the pseudo console handle
/// - `input_write`: parent writes here to send input to the child
/// - `output_read`: parent reads here to receive output from the child
/// - `process`: the child process information (if spawned)
pub struct Pty {
    pub hpc: HPCON,
    pub input_write: HANDLE,
    pub output_read: HANDLE,
    pub process: Option<PROCESS_INFORMATION>,
    pub attr_list_buf: Option<Vec<u8>>,
    /// Current pseudo console size in columns and rows.
    pub cols: u16,
    pub rows: u16,
}

impl Pty {
    /// Create a new pseudo console with the given buffer size (columns x rows).
    pub fn new(cols: u16, rows: u16) -> Result<Self> {
        // Input pipe: parent writes to input_write, PTY reads from input_read.
        let mut input_read: HANDLE = unsafe { mem::zeroed() };
        let mut input_write: HANDLE = unsafe { mem::zeroed() };
        unsafe {
            CreatePipe(&mut input_read, &mut input_write, None, 0)
                .context("CreatePipe (input) failed")?;
        }

        // Output pipe: PTY writes to output_write, parent reads from output_read.
        // A modest buffer is enough because the daemon keeps draining even while
        // no client is attached.
        let mut output_read: HANDLE = unsafe { mem::zeroed() };
        let mut output_write: HANDLE = unsafe { mem::zeroed() };
        unsafe {
            CreatePipe(&mut output_read, &mut output_write, None, 64 * 1024)
                .context("CreatePipe (output) failed")?;
        }

        let size = COORD {
            X: cols as i16,
            Y: rows as i16,
        };
        let hpc = unsafe {
            CreatePseudoConsole(size, input_read, output_write, 0)
                .context("CreatePseudoConsole failed")?
        };

        // CreatePseudoConsole duplicates the passed handles internally,
        // so we close the originals we handed over.
        unsafe {
            CloseHandle(input_read).context("CloseHandle(input_read) failed")?;
            CloseHandle(output_write).context("CloseHandle(output_write) failed")?;
        }

        Ok(Pty {
            hpc,
            input_write,
            output_read,
            process: None,
            attr_list_buf: None,
            cols,
            rows,
        })
    }

    /// Spawn a shell command inside this pseudo console.
    pub fn spawn(&mut self, cmd: &str) -> Result<()> {
        // Build the attribute list with PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE.
        let mut attr_list_size: usize = 0;
        unsafe {
            InitializeProcThreadAttributeList(
                LPPROC_THREAD_ATTRIBUTE_LIST(ptr::null_mut()),
                1,
                0,
                &mut attr_list_size,
            )
            .ok();
        }
        if attr_list_size == 0 {
            anyhow::bail!("InitializeProcThreadAttributeList returned zero size");
        }

        let mut buf = vec![0u8; attr_list_size];
        let attr_list_ptr = LPPROC_THREAD_ATTRIBUTE_LIST(buf.as_mut_ptr() as *mut c_void);

        unsafe {
            InitializeProcThreadAttributeList(attr_list_ptr, 1, 0, &mut attr_list_size).ok();

            // PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE takes the HPCON value itself
            // (not a pointer to it); pass the handle value as lpValue.
            let hpc_val: *const c_void = self.hpc.0 as *const c_void;
            let result = UpdateProcThreadAttribute(
                attr_list_ptr,
                0,
                PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
                Some(hpc_val),
                mem::size_of::<HPCON>(),
                None,
                None,
            );
            if result.is_err() {
                anyhow::bail!("UpdateProcThreadAttribute (PSEUDOCONSOLE) failed");
            }
        }

        // Build the command line as a mutable wide string.
        let wide: Vec<u16> = OsStrExt::encode_wide(std::ffi::OsStr::new(cmd))
            .chain(Some(0))
            .collect();
        let mut cmd_buf = wide;

        let mut si: STARTUPINFOEXW = unsafe { mem::zeroed() };
        si.StartupInfo.cb = mem::size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESHOWWINDOW;
        si.StartupInfo.wShowWindow = SW_HIDE.0 as u16;
        si.lpAttributeList = self
            .attr_list_buf
            .as_ref()
            .map(|_| attr_list_ptr)
            .unwrap_or(attr_list_ptr);

        // Keep the buffer alive for the duration of CreateProcessW.
        self.attr_list_buf = Some(buf);

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let flags = EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT;

        // Windows copies the parent's standard handles into the child's process
        // parameters, and a redirected file/pipe handle wins over the pseudo
        // console: the shell would then read/write the daemon's own stdio instead
        // of the PTY. Hide those handles for the duration of CreateProcessW.
        let _stdio = StdioHider::install();

        let user_env = user_environment();
        // CreateProcessW copies the block, so the Vec only has to outlive the
        // call; `user_env` stays in scope until after it returns.
        let env_ptr = user_env
            .as_ref()
            .map(|(block, _)| block.as_ptr() as *const c_void);

        let result = unsafe {
            windows::Win32::System::Threading::CreateProcessW(
                None,
                windows::core::PWSTR::from_raw(cmd_buf.as_mut_ptr()),
                None,
                None,
                false, // bInheritHandles - ConPTY handles the connection
                flags,
                env_ptr,
                None, // lpCurrentDirectory - inherit parent's
                &mut si as *mut STARTUPINFOEXW as *mut _,
                &mut pi,
            )
        };

        if let Some((_block, token)) = user_env {
            unsafe {
                let _ = CloseHandle(token);
            }
        }
        result.map_err(|e| {
            let code = e.code().0 as u32;
            // ERROR_ACCESS_DENIED, in Win32 and NTSTATUS form: an MSIX execution
            // alias (Microsoft Store pwsh) only activates in an interactive
            // session, so a background daemon cannot start it.
            let hint = if code == 0x8007_0005 || code == 0xC007_0005 {
                " (access denied; a Microsoft Store app alias only starts in an interactive session)"
            } else {
                ""
            };
            anyhow::anyhow!("CreateProcessW failed: 0x{:08X}{}", code, hint)
        })?;

        self.process = Some(pi);
        Ok(())
    }

    /// Read output bytes from the pseudo console (from child's stdout/stderr).
    pub fn read_output(&self, buf: &mut [u8]) -> Result<u32> {
        let mut read: u32 = 0;
        unsafe {
            ReadFile(self.output_read, Some(buf), Some(&mut read), None)
                .context("ReadFile from PTY output failed")?;
        }
        Ok(read)
    }

    /// Number of bytes currently available to read without blocking.
    ///
    /// Uses PeekNamedPipe on the PTY output pipe. Returns `Err` when the
    /// pipe is broken (child exited).
    pub fn available(&self) -> Result<u32> {
        let mut avail: u32 = 0;
        unsafe {
            windows::Win32::System::Pipes::PeekNamedPipe(
                self.output_read,
                None,
                0,
                None,
                Some(&mut avail),
                None,
            )
            .context("PeekNamedPipe on PTY output failed")?;
        }
        Ok(avail)
    }

    /// Resize the pseudo console buffer.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<()> {
        unsafe {
            windows::Win32::System::Console::ResizePseudoConsole(
                self.hpc,
                COORD {
                    X: cols as i16,
                    Y: rows as i16,
                },
            )
            .context("ResizePseudoConsole failed")?;
        }
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    /// Whether the child shell process is still running.
    pub fn is_running(&self) -> bool {
        match &self.process {
            Some(pi) => unsafe {
                windows::Win32::System::Threading::WaitForSingleObject(pi.hProcess, 0)
                    == windows::Win32::Foundation::WAIT_TIMEOUT
            },
            None => false,
        }
    }

    /// Child process ID.
    pub fn process_id(&self) -> Option<u32> {
        self.process.as_ref().map(|pi| pi.dwProcessId)
    }
}

/// Temporarily clears standard handles that point at files or pipes.
///
/// Console handles are process-local and cannot leak into a child process, so
/// they are left untouched; only redirected handles need hiding.
struct StdioHider {
    saved: Vec<(STD_HANDLE, HANDLE)>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl StdioHider {
    fn install() -> Self {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut saved = Vec::new();

        for id in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            unsafe {
                let h = match GetStdHandle(id) {
                    Ok(h) => h,
                    Err(_) => continue,
                };
                let mut mode = CONSOLE_MODE(0);
                if GetConsoleMode(h, &mut mode).is_ok() {
                    continue;
                }
                saved.push((id, h));
                let _ = SetStdHandle(id, HANDLE::default());
            }
        }

        Self { saved, _lock: lock }
    }
}

impl Drop for StdioHider {
    fn drop(&mut self) {
        unsafe {
            for (id, h) in self.saved.drain(..) {
                let _ = SetStdHandle(id, h);
            }
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        if let Some(pi) = self.process.take() {
            unsafe {
                // Terminate the shell before closing the pseudo console:
                // ClosePseudoConsole can block until all attached processes exit.
                let _ = windows::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                let _ = windows::Win32::System::Threading::WaitForSingleObject(pi.hProcess, 2000);
                let _ = CloseHandle(pi.hProcess);
                let _ = CloseHandle(pi.hThread);
            }
        }
        unsafe {
            let _ = CloseHandle(self.input_write);
            let _ = CloseHandle(self.output_read);
        }
        unsafe {
            let _ = ClosePseudoConsole(self.hpc);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a UTF-16 environment block from `NAME=value` entries.
    fn block(entries: &[&str]) -> Vec<u16> {
        let mut raw = Vec::new();
        for entry in entries {
            raw.extend(entry.encode_utf16());
            raw.push(0);
        }
        raw.push(0);
        raw
    }

    fn entries(block: &[u16]) -> Vec<String> {
        let mut out = Vec::new();
        let mut current = String::new();
        for &unit in block {
            if unit == 0 {
                if current.is_empty() {
                    break;
                }
                out.push(std::mem::take(&mut current));
            } else {
                current.push(char::from_u32(unit as u32).unwrap_or('\u{fffd}'));
            }
        }
        out
    }

    #[test]
    fn term_is_added_when_the_registry_environment_lacks_it() {
        let out = ensure_terminal_env(&block(&["Path=C:\\Windows", "USERNAME=ben"]));
        let values = entries(&out);

        assert_eq!(values, vec!["Path=C:\\Windows", "USERNAME=ben", TERM_ENTRY]);
        assert_eq!(
            &out[out.len() - 2..],
            &[0, 0],
            "block must end with double NUL"
        );
    }

    #[test]
    fn an_existing_term_is_replaced_not_duplicated() {
        let out = ensure_terminal_env(&block(&[
            "Path=C:\\Windows",
            "term=dumb",
            "TERM=vt100",
            "USERNAME=ben",
        ]));
        let values = entries(&out);

        assert_eq!(
            values,
            vec!["Path=C:\\Windows", TERM_ENTRY, "USERNAME=ben"],
            "only one TERM entry should remain"
        );
    }

    #[test]
    fn is_env_key_matches_ascii_names_case_insensitively() {
        let term = block(&["term=x"]);
        let entry = &term[..term.len() - 1];
        assert!(is_env_key(entry, "TERM"));
        assert!(is_env_key(entry, "term"));
        assert!(!is_env_key(entry, "TERMINAL"));
        assert!(!is_env_key(entry, "TERM_PREFIX"));
    }

    #[test]
    fn an_empty_block_still_gets_the_terminal_identity() {
        let out = ensure_terminal_env(&[0]);
        assert_eq!(entries(&out), vec![TERM_ENTRY]);
        assert_eq!(&out[out.len() - 2..], &[0, 0]);
    }

    #[test]
    fn values_containing_equals_are_preserved() {
        let out = ensure_terminal_env(&block(&["=C:=C:\\work", "A=b=c"]));
        assert_eq!(entries(&out), vec!["=C:=C:\\work", "A=b=c", TERM_ENTRY]);
    }
}
