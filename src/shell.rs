//! Choosing the PowerShell to run a default (non-WSL) session with.
//!
//! Preference is an unpackaged install, then whatever `pwsh.exe` resolves to
//! (which on this machine is the Microsoft Store build), then Windows
//! PowerShell 5.1. The daemon tries the candidates in order and keeps the first
//! one that actually starts: a Store alias is an MSIX app, so starting it needs
//! an interactive session and fails with ERROR_ACCESS_DENIED in session 0.

use std::path::{Path, PathBuf};

/// Shell commands to try, in order, for a default session.
pub fn shell_candidates() -> Vec<String> {
    let all = candidates();
    let mut found: Vec<PathBuf> = all.iter().filter(|p| p.is_file()).cloned().collect();
    if found.is_empty() {
        // None of the preferred paths exists; still offer the last resort so the
        // caller gets CreateProcessW's error rather than an empty list.
        if let Some(last) = all.last() {
            found.push(last.clone());
        }
    }

    let mut out: Vec<String> = Vec::new();
    for path in found {
        let s = path.to_string_lossy().into_owned();
        if !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Best guess for logging: the first candidate that exists.
pub fn default_shell() -> String {
    shell_candidates().into_iter().next().unwrap_or_default()
}

fn candidates() -> Vec<PathBuf> {
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    let mut out = vec![program_files.join(r"PowerShell\7\pwsh.exe")];

    let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    if let Some(local) = &local_app_data {
        // Per-user MSI or zip install.
        out.push(local.join(r"Microsoft\PowerShell\7\pwsh.exe"));
    }

    if let Some(p) = path_pwsh() {
        out.push(p);
    }

    if let Some(local) = &local_app_data {
        // Microsoft Store build: works when the daemon runs in an interactive
        // session, denied when it runs in session 0.
        out.push(local.join(r"Microsoft\WindowsApps\pwsh.exe"));
    }

    let root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    out.push(root.join(r"System32\WindowsPowerShell\v1.0\powershell.exe"));
    out
}

fn path_pwsh() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("pwsh.exe"))
        .find(|candidate| candidate.is_file())
}

/// First existing candidate, or the last one when none exists.
#[cfg(test)]
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
    fn candidates_are_ordered_and_end_with_windows_powershell() {
        let candidates = candidates();
        let last = candidates.last().unwrap();
        assert_eq!(
            last.file_name().unwrap().to_string_lossy().to_lowercase(),
            "powershell.exe"
        );
        assert_eq!(
            candidates[0].file_name().unwrap().to_string_lossy().to_lowercase(),
            "pwsh.exe"
        );
    }

    #[test]
    fn shell_candidates_are_deduplicated() {
        let list = shell_candidates();
        assert!(!list.is_empty());
        let mut seen = std::collections::HashSet::new();
        for c in &list {
            assert!(seen.insert(c.clone()), "duplicate candidate: {}", c);
        }
    }
}
