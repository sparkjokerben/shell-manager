use shell_manager::daemon::Daemon;
use shell_manager::protocol;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentProcessId};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;

/// `Some(handle)` when this process created the mutex, `None` when one already
/// existed (another daemon owns it). The handle must stay alive for the process
/// lifetime.
fn try_mutex(name: &str) -> anyhow::Result<Option<HANDLE>> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, BOOL};

    let wide: Vec<u16> = std::ffi::OsStr::new(name)
        .encode_wide()
        .chain(Some(0))
        .collect();
    let handle = unsafe { CreateMutexW(None, BOOL(0), PCWSTR::from_raw(wide.as_ptr()))? };

    // CreateMutexW sets ERROR_ALREADY_EXISTS when the mutex already existed.
    let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
    if err == 183 {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(handle))
}

/// Ensure only one daemon instance runs.
///
/// `Global\` also covers a daemon started in another session - an SSH window
/// lives in session 0 while the desktop is session 1 - and a `Local\` name
/// (per-session) let a second instance coexist and steal client requests.
fn acquire_single_instance() -> anyhow::Result<HANDLE> {
    let (global, local) = protocol::mutex_names_for(protocol::instance());

    match try_mutex(&global) {
        Ok(Some(h)) => return Ok(h),
        Ok(None) => anyhow::bail!("another smd instance is already running"),
        // Creating a Global\ kernel object needs SeCreateGlobalPrivilege, which
        // an unelevated logon session may lack; fall back to the per-session name.
        Err(_) => {}
    }

    match try_mutex(&local)? {
        Some(h) => Ok(h),
        None => anyhow::bail!("another smd instance is already running"),
    }
}

/// Windows session the daemon runs in: 0 for services/SSH, 1+ for desktops.
fn current_session() -> u32 {
    let mut session = 0u32;
    unsafe {
        let _ = ProcessIdToSessionId(GetCurrentProcessId(), &mut session);
    }
    session
}

fn main() -> anyhow::Result<()> {
    let _mutex = match acquire_single_instance() {
        Ok(h) => h,
        Err(e) => {
            eprintln!("smd: {}", e);
            return Ok(());
        }
    };

    let shell = shell_manager::shell::default_shell();
    shell_manager::log::info(&format!(
        "daemon start instance={} pid={} session={} shell={}",
        protocol::instance(),
        std::process::id(),
        current_session(),
        shell
    ));

    ctrlc::set_handler(move || {
        println!("\nsmd shutting down...");
        std::process::exit(0);
    })
    .expect("error setting Ctrl-C handler");

    let mut daemon = Daemon::new()?;
    println!(
        "smd started (instance {}, session {}, shell {}).",
        protocol::instance(),
        current_session(),
        shell
    );
    daemon.run()?;
    Ok(())
}
