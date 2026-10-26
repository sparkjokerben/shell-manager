use shell_manager::pty::Pty;
use std::time::{Duration, Instant};

/// ConPTY must forward the sequences TUI apps rely on: 24-bit color and
/// synchronized output. The payload also proves the child is really attached to
/// the pseudo console rather than inheriting the host's redirected stdio.
#[test]
fn conpty_forwards_truecolor_and_sync_output() {
    let file = std::env::temp_dir().join(format!("sd-conpty-test-{}.txt", std::process::id()));
    let payload: &[u8] =
        b"\x1b[38;2;12;34;56mTRUECOLOR\x1b[0m\r\n\x1b[?2026hSYNC-FRAME\x1b[?2026l\r\n";
    std::fs::write(&file, payload).unwrap();

    let mut pty = Pty::new(80, 25).unwrap();
    pty.spawn(&format!("cmd.exe /c type \"{}\"", file.display())).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut out: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    let mut quiet_since = Instant::now();
    while Instant::now() < deadline {
        match pty.available() {
            Ok(n) if n > 0 => {
                let r = pty.read_output(&mut buf).unwrap() as usize;
                out.extend_from_slice(&buf[..r]);
                quiet_since = Instant::now();
            }
            Ok(_) => {
                if !pty.is_running()
                    && Instant::now().duration_since(quiet_since) > Duration::from_millis(300)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
    let _ = std::fs::remove_file(&file);

    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("\u{1b}[38;2;12;34;56m"),
        "truecolor SGR did not survive ConPTY: {:?}",
        text
    );
    assert!(
        text.contains("TRUECOLOR"),
        "child output did not reach the pseudo console: {:?}",
        text
    );
    assert!(
        text.contains("\u{1b}[?2026h"),
        "synchronized output mode did not survive ConPTY: {:?}",
        text
    );
}
