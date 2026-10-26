//! Small append-only daemon log in %TEMP%.
//!
//! The daemon normally runs without a console, so this is the only way to
//! answer questions like "which shell did it start" or "why did a start fail".
//! Logging must never take the daemon down, hence the swallowed I/O errors.

use std::io::Write;

const MAX_BYTES: u64 = 1024 * 1024;

pub fn info(msg: &str) {
    let path = std::env::temp_dir().join("smd.log");

    // Keep the file bounded: start over once it grows past the cap.
    if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }

    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{}", msg);
    }
}
