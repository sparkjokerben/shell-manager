use shell_daemon::daemon::Daemon;

/// Ensure only one daemon instance runs.
///
/// Creates a named mutex; if it already exists another instance is running.
/// The returned handle must stay alive for the process lifetime.
fn acquire_single_instance() -> anyhow::Result<windows::Win32::Foundation::HANDLE> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GetLastError, BOOL};
    use windows::Win32::System::Threading::CreateMutexW;

    let name: Vec<u16> = std::ffi::OsStr::new("Local\\shell-daemon-single-instance")
        .encode_wide()
        .chain(Some(0))
        .collect();

    let handle = unsafe {
        CreateMutexW(None, BOOL(0), PCWSTR::from_raw(name.as_ptr()))?
    };

    // CreateMutexW sets ERROR_ALREADY_EXISTS (183 -> HRESULT 0x800700B7) when the
    // mutex already existed, i.e. another daemon is running.
    let already_exists = unsafe { GetLastError() }
        .err()
        .map(|e| (e.code().0 as u32) == 0x8007_00B7)
        .unwrap_or(false);
    if already_exists {
        anyhow::bail!("another shell-daemon instance is already running");
    }

    Ok(handle)
}

fn main() -> anyhow::Result<()> {
    let _mutex = match acquire_single_instance() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("shell-daemon: {}", e);
            return Ok(());
        }
    };

    ctrlc::set_handler(move || {
        println!("\nshell-daemon shutting down...");
        std::process::exit(0);
    })
    .expect("error setting Ctrl-C handler");

    let mut daemon = Daemon::new()?;
    println!("shell-daemon started. Press Ctrl-C to stop.");
    daemon.run()?;
    Ok(())
}
