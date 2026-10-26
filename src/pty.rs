//! ConPTY pseudo-terminal wrapper for Windows.
//!
//! Uses CreatePseudoConsole + CreateProcessW with
//! PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE to host a shell child process
//! whose stdin/stdout/stderr are attached to a pseudo console.

use anyhow::{Context, Result};
use std::mem;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::ffi::c_void;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE,
};
use windows::Win32::System::Console::{
    CreatePseudoConsole, ClosePseudoConsole, COORD, HPCON,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    EXTENDED_STARTUPINFO_PRESENT, STARTUPINFOEXW,
    PROCESS_INFORMATION,
    PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, STARTF_USESHOWWINDOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::Win32::System::Threading::{
    InitializeProcThreadAttributeList, UpdateProcThreadAttribute,
    LPPROC_THREAD_ATTRIBUTE_LIST,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};

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
        let mut output_read: HANDLE = unsafe { mem::zeroed() };
        let mut output_write: HANDLE = unsafe { mem::zeroed() };
        unsafe {
            CreatePipe(&mut output_read, &mut output_write, None, 0)
                .context("CreatePipe (output) failed")?;
        }

        let size = COORD { X: cols as i16, Y: rows as i16 };
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
            InitializeProcThreadAttributeList(
                attr_list_ptr,
                1,
                0,
                &mut attr_list_size,
            )
            .ok();

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
        si.lpAttributeList = self.attr_list_buf.as_ref().map(|_| attr_list_ptr).unwrap_or(attr_list_ptr);

        // Keep the buffer alive for the duration of CreateProcessW.
        self.attr_list_buf = Some(buf);

        let mut pi: PROCESS_INFORMATION = unsafe { mem::zeroed() };

        let flags = EXTENDED_STARTUPINFO_PRESENT;

        unsafe {
            windows::Win32::System::Threading::CreateProcessW(
                None,
                windows::core::PWSTR::from_raw(cmd_buf.as_mut_ptr()),
                None,
                None,
                false, // bInheritHandles - ConPTY handles the connection
                flags,
                None, // lpEnvironment - inherit parent's
                None, // lpCurrentDirectory - inherit parent's
                &mut si as *mut STARTUPINFOEXW as *mut _,
                &mut pi,
            )
            .context("CreateProcessW failed")?;
        }

        self.process = Some(pi);
        Ok(())
    }

    /// Write input bytes to the pseudo console (sent to child's stdin).
    pub fn write_input(&self, buf: &[u8]) -> Result<u32> {
        let mut written: u32 = 0;
        unsafe {
            WriteFile(
                self.input_write,
                Some(buf),
                Some(&mut written),
                None,
            )
            .context("WriteFile to PTY input failed")?;
        }
        Ok(written)
    }

    /// Read output bytes from the pseudo console (from child's stdout/stderr).
    pub fn read_output(&self, buf: &mut [u8]) -> Result<u32> {
        let mut read: u32 = 0;
        unsafe {
            ReadFile(
                self.output_read,
                Some(buf),
                Some(&mut read),
                None,
            )
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
                COORD { X: cols as i16, Y: rows as i16 },
            )
            .context("ResizePseudoConsole failed")?;
        }
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
