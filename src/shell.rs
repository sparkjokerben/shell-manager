//! Picking a PowerShell binary that the daemon can actually start.
//!
//! The Microsoft Store build of PowerShell 7 is an MSIX package: its
//! `WindowsApps\pwsh.exe` execution alias can only be activated from an
//! interactive session. A daemon running in session 0 - for instance one
//! started from an SSH window - gets ERROR_ACCESS_DENIED from CreateProcessW.
//! Prefer an unpackaged install (MSI or zip) at a known location, and fall back
//! to Windows PowerShell 5.1, which always starts.

use std::path::{Path, PathBuf};

/// Resolve the command for a default (non-WSL) session.
pub fn default_shell() -> String {
    pick(&candidates(), |p| p.is_file())
        .to_string_lossy()
        .into_owned()
}

/// Candidate shells in preference order; the last one is the unconditional
/// fallback and is therefore never filtered out by `pick`.
fn candidates() -> Vec<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let mut out = vec![program_files.join(r"PowerShell\7\pwsh.exe")];

    // Per-user MSI or zip install.
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join(r"Microsoft\PowerShell\7\pwsh.exe"));
    }

    if let Some(p) = path_pwsh() {
        out.push(p);
    }

    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    out.push(root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
    out
}

/// `pwsh.exe` from PATH, skipping the Store execution alias.
///
/// The alias lives under `...\Microsoft\WindowsApps`; a real install dropped on
/// PATH (zip deployment) is fine.
fn path_pwsh() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("pwsh.exe");
        if candidate.is_file() && !is_store_alias(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_store_alias(p: &Path) -> bool {
    p.to_string_lossy()
        .to_ascii_lowercase()
        .contains(r"\microsoft\windowsapps\")
}

fn pick<F: Fn(&Path) -> bool>(candidates: &[PathBuf], exists: F) -> PathBuf {
    candidates
        .iter()
        .find(|p| exists(p))
        .or_else(|| candidates.last())
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_first_existing_candidate() {
        let candidates = vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")];
        assert_eq!(pick(&candidates, |p| p.starts_with("b")), PathBuf::from("b"));
    }

    #[test]
    fn falls_back_to_last_candidate() {
        let candidates = vec![PathBuf::from("a"), PathBuf::from("b")];
        assert_eq!(pick(&candidates, |_| false), PathBuf::from("b"));
    }

    #[test]
    fn store_alias_is_detected() {
        assert!(is_store_alias(Path::new(
            r"C:\Users\me\AppData\Local\Microsoft\WindowsApps\pwsh.exe"
        )));
        assert!(is_store_alias(Path::new(
            r"c:\users\me\appdata\local\microsoft\windowsapps\pwsh.exe"
        )));
        assert!(!is_store_alias(Path::new(r"C:\Program Files\PowerShell\7\pwsh.exe")));
    }

    #[test]
    fn windows_powershell_is_the_last_resort() {
        let candidates = candidates();
        let last = candidates.last().unwrap();
        assert_eq!(
            last.file_name().unwrap().to_string_lossy().to_lowercase(),
            "powershell.exe"
        );
    }
}
