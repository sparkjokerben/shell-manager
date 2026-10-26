//! Session: a single PTY-backed shell session managed by the daemon.
//!
//! The client talks to a session through **two unidirectional named pipes**:
//! - `out`: daemon writes PTY output, client reads it.
//! - `in`:  client writes keyboard input, daemon reads it.
//!
//! Using separate handles per direction avoids Windows serializing a blocking
//! `ReadFile` against a `WriteFile` on the same synchronous (non-overlapped)
//! pipe handle.

use crate::protocol::data_pipe_prefix;
use crate::pty::Pty;
use anyhow::Result;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;
use windows::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND,
};
use windows::Win32::System::Pipes::{
    CreateNamedPipeW, ConnectNamedPipe, DisconnectNamedPipe,
    PIPE_TYPE_BYTE, PIPE_READMODE_BYTE, PIPE_WAIT,
    PIPE_UNLIMITED_INSTANCES, NMPWAIT_WAIT_FOREVER,
};
use windows::Win32::Storage::FileSystem::{ReadFile, WriteFile};
use windows::Win32::System::Console::{COORD, ResizePseudoConsole};
use windows::Win32::System::Threading::GetCurrentProcess;

/// State of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Created,   // PTY + process spawned, no client attached yet
    Attached,  // a client is attached
    Detached,  // client detached; session still alive
    Stopped,   // session terminated
}

/// Shared handle to a single shell session.
pub struct Session {
    pub name: String,
    pub session_id: String,
    pub pty: Arc<Mutex<Pty>>,
    pub state: Arc<Mutex<SessionState>>,
    /// Set to stop both directional threads.
    pub stop_flag: Arc<AtomicBool>,
    /// True while a client is believed to be attached.
    pub client_connected: Arc<AtomicBool>,
    /// Set by an out-of-band detach request; both forwarder threads watch it.
    pub detach_requested: Arc<AtomicBool>,
    /// Set when a client connection ends; the next attach forces a repaint.
    pub needs_repaint: Arc<AtomicBool>,
    /// DEC mouse modes the hosted app has enabled (1000/1002/1003/1006).
    pub mouse_modes: Arc<AtomicU8>,
    /// Win32 thread ids of the out/in threads (for cancelling blocking I/O).
    pub out_tid: Arc<Mutex<Option<u32>>>,
    pub in_tid: Arc<Mutex<Option<u32>>>,
    /// Set when the corresponding listener thread has exited. Used by `stop`
    /// to keep cancelling until both threads are really gone.
    pub out_done: Arc<AtomicBool>,
    pub in_done: Arc<AtomicBool>,
    /// Connection generation per direction, bumped after `ConnectNamedPipe`
    /// succeeds. A pending detach helper stops once either counter moves, so a
    /// quick reattach is not hit by cancellations meant for the old client.
    pub out_gen: Arc<AtomicU32>,
    pub in_gen: Arc<AtomicU32>,
    /// Serialises publishing a new connection's generation against the detach
    /// helper's "re-check generations + cancel". A listener waits here between
    /// `ConnectNamedPipe` and its first I/O, so while it waits it has nothing
    /// for `CancelSynchronousIo` to abort; the helper therefore cannot cancel a
    /// client that attached after its last generation check.
    pub conn_gate: Arc<Mutex<()>>,
    /// Set once both directional listener threads have been spawned. `stop`
    /// uses it to avoid waiting for threads that never started.
    pub listeners_started: AtomicBool,
}

impl Session {
    pub fn new(name: &str, cols: u16, rows: u16, cmd: &str) -> Result<Self> {
        let session_id = Uuid::new_v4().to_string();
        let mut pty = Pty::new(cols, rows)?;
        pty.spawn(cmd)?;

        Ok(Session {
            name: name.to_string(),
            session_id,
            pty: Arc::new(Mutex::new(pty)),
            state: Arc::new(Mutex::new(SessionState::Created)),
            stop_flag: Arc::new(AtomicBool::new(false)),
            client_connected: Arc::new(AtomicBool::new(false)),
            detach_requested: Arc::new(AtomicBool::new(false)),
            needs_repaint: Arc::new(AtomicBool::new(false)),
            mouse_modes: Arc::new(AtomicU8::new(0)),
            out_tid: Arc::new(Mutex::new(None)),
            in_tid: Arc::new(Mutex::new(None)),
            out_done: Arc::new(AtomicBool::new(false)),
            in_done: Arc::new(AtomicBool::new(false)),
            out_gen: Arc::new(AtomicU32::new(0)),
            in_gen: Arc::new(AtomicU32::new(0)),
            conn_gate: Arc::new(Mutex::new(())),
            listeners_started: AtomicBool::new(false),
        })
    }

    /// (out_pipe_path, in_pipe_path) for this session.
    pub fn data_pipe_paths(&self) -> (String, String) {
        let prefix = data_pipe_prefix();
        (
            format!("{}\\{}\\out", prefix, self.session_id),
            format!("{}\\{}\\in", prefix, self.session_id),
        )
    }

    /// Start the two directional listener/forwarder threads.
    pub fn start_data_pipe_listener(&self) -> Result<()> {
        let (out_path, in_path) = self.data_pipe_paths();

        // out: daemon -> client (server writes).
        let out_h = create_pipe(&out_path, PIPE_ACCESS_OUTBOUND)?;
        // in: client -> daemon (server reads).
        let in_h = create_pipe(&in_path, PIPE_ACCESS_INBOUND)?;

        // The input thread writes through a private duplicate of the PTY input
        // handle instead of holding the pty mutex: `WriteFile` can block while
        // the console drains, and the out/drain threads need that mutex to read
        // output. The duplicate stays valid after the Pty is dropped and closes
        // its own handle.
        let input_write = {
            let p = self.pty.lock().unwrap();
            let mut dup = HANDLE::default();
            let result = unsafe {
                DuplicateHandle(
                    GetCurrentProcess(),
                    p.input_write,
                    GetCurrentProcess(),
                    &mut dup,
                    0,
                    false,
                    DUPLICATE_SAME_ACCESS,
                )
            };
            if let Err(e) = result {
                unsafe {
                    let _ = CloseHandle(out_h);
                    let _ = CloseHandle(in_h);
                }
                return Err(e.into());
            }
            dup
        };

        let pty_out = self.pty.clone();
        let state_out = self.state.clone();
        let state_in = self.state.clone();
        let stop_out = self.stop_flag.clone();
        let stop_in = self.stop_flag.clone();
        let alive_out = self.client_connected.clone();
        let alive_in = self.client_connected.clone();
        let out_tid = self.out_tid.clone();
        let in_tid = self.in_tid.clone();
        let detach_out = self.detach_requested.clone();
        let detach_in = self.detach_requested.clone();
        let repaint_out = self.needs_repaint.clone();
        let repaint_in = self.needs_repaint.clone();
        let mouse_out = self.mouse_modes.clone();
        let mouse_drain = self.mouse_modes.clone();
        let out_gen = self.out_gen.clone();
        let in_gen = self.in_gen.clone();
        let out_done = self.out_done.clone();
        let in_done = self.in_done.clone();
        let gate_in = self.conn_gate.clone();
        let gate_out = self.conn_gate.clone();

        // In thread (client -> PTY).
        let in_tid_slot = in_tid.clone();
        let stop_in_thread = stop_in.clone();
        let in_gen_thread = in_gen.clone();
        thread::spawn(move || {
            let _done = DoneOnDrop(in_done);
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *in_tid_slot.lock().unwrap() = Some(tid);
            run_in_thread(
                in_h, input_write, state_in, stop_in_thread, alive_in, detach_in, repaint_in,
                in_gen_thread, gate_in,
            );
        });

        // Out thread (PTY -> client).
        let out_gen_thread = out_gen.clone();
        thread::spawn(move || {
            let _done = DoneOnDrop(out_done);
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
            *out_tid.lock().unwrap() = Some(tid);
            run_out_thread(out_h, pty_out, state_out, stop_out, alive_out, detach_out, repaint_out, mouse_out, out_gen_thread, gate_out);
        });

        // Both directional threads exist now; `stop` may wait for them.
        self.listeners_started.store(true, Ordering::Release);

        // Drain thread (PTY -> discard while no client is attached).
        let drain_pty = self.pty.clone();
        let drain_stop = self.stop_flag.clone();
        let drain_connected = self.client_connected.clone();
        thread::spawn(move || run_drain_thread(drain_pty, drain_stop, drain_connected, mouse_drain));

        Ok(())
    }

    /// Resize the underlying pseudo console to match the client terminal.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        self.pty.lock().unwrap().resize(cols, rows)
    }

    /// Whether the hosted shell process is still running.
    pub fn is_alive(&self) -> bool {
        self.pty.lock().unwrap().is_running()
    }

    /// DEC mouse modes the hosted app currently has enabled.
    pub fn mouse_state(&self) -> u8 {
        self.mouse_modes.load(Ordering::Relaxed)
    }

    /// Stop the session: signal and cancel both directional threads.
    pub fn stop(&self) -> Result<()> {
        *self.state.lock().unwrap() = SessionState::Stopped;
        self.stop_flag.store(true, Ordering::Relaxed);

        // No directional thread exists if the listener was never started (the
        // session was dropped right after `new`, or `start_data_pipe_listener`
        // failed). Waiting for them would stall the caller for the full deadline;
        // daemon callers may hold the session table lock meanwhile.
        if !self.listeners_started.load(Ordering::Acquire) {
            return Ok(());
        }

        // One cancel can land in the window between the thread's stop check and
        // its blocking ConnectNamedPipe, leaving it blocked forever (and the
        // Pty leaked). Keep cancelling unfinished threads until both have exited
        // or ~1s has passed. The tid locks are released before cancelling:
        // CancelSynchronousIo may block until the target I/O aborts.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let out_done = self.out_done.load(Ordering::Acquire);
            let in_done = self.in_done.load(Ordering::Acquire);
            if out_done && in_done {
                break;
            }

            let ot = *self.out_tid.lock().unwrap();
            let it = *self.in_tid.lock().unwrap();
            if !out_done {
                cancel_thread_io(ot);
            }
            if !in_done {
                cancel_thread_io(it);
            }

            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }

    /// Disconnect the attached client without stopping the session.
    ///
    /// The actual cancellation runs on a helper thread: CancelSynchronousIo can
    /// block, and callers may hold the session table lock.
    pub fn detach_client(&self) {
        if !self.client_connected.load(Ordering::Relaxed) {
            return;
        }

        self.detach_requested.store(true, Ordering::Relaxed);

        let in_tid = self.in_tid.clone();
        let out_tid = self.out_tid.clone();
        let connected = self.client_connected.clone();
        let requested = self.detach_requested.clone();
        let in_gen = self.in_gen.clone();
        let out_gen = self.out_gen.clone();
        let gate = self.conn_gate.clone();
        // Snapshot the connection generations at request time. If either moves,
        // a new client has attached and its I/O must not be cancelled.
        let start_in_gen = in_gen.load(Ordering::SeqCst);
        let start_out_gen = out_gen.load(Ordering::SeqCst);

        thread::spawn(move || {
            // Retry until the client is gone: a single cancel can miss when the
            // target thread is between blocking calls. A fresh attach clears the
            // request flag and bumps a generation, which also stops this loop.
            for _ in 0..50 {
                if !connected.load(Ordering::Relaxed) || !requested.load(Ordering::Relaxed) {
                    break;
                }
                if in_gen.load(Ordering::SeqCst) != start_in_gen
                    || out_gen.load(Ordering::SeqCst) != start_out_gen
                {
                    break;
                }
                let it = *in_tid.lock().unwrap();
                let ot = *out_tid.lock().unwrap();
                // Hold the connection gate across the re-check and the blocking
                // cancels. A new client publishes its generation under this same
                // lock, so it cannot slip in between the check and the cancel;
                // while it waits it is not doing I/O, hence the cancel can only
                // hit the previous client. The next iteration sees the bumped
                // generation and stops before touching the new client.
                let gate_guard = gate.lock().unwrap();
                if !requested.load(Ordering::Relaxed)
                    || in_gen.load(Ordering::SeqCst) != start_in_gen
                    || out_gen.load(Ordering::SeqCst) != start_out_gen
                {
                    break;
                }
                cancel_thread_io(it);
                cancel_thread_io(ot);
                drop(gate_guard);
                thread::sleep(Duration::from_millis(20));
            }
        });
    }
}

/// Sets a flag when dropped, so a listener thread marks itself finished even on
/// an early return.
struct DoneOnDrop(Arc<AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Bit for one of the DEC private mouse modes we track.
pub fn mouse_bit(mode: u32) -> Option<u8> {
    match mode {
        1000 => Some(1),
        1002 => Some(2),
        1003 => Some(4),
        1006 => Some(8),
        _ => None,
    }
}

/// Calls `f(mode, enabled)` for every DEC private mouse mode toggled in `data`.
///
/// Handles multi-parameter sequences such as ESC [ ? 1003 ; 1006 h.
pub fn for_each_mouse_mode(data: &[u8], mut f: impl FnMut(u32, bool)) {
    let mut i = 0;
    while i + 4 < data.len() {
        if data[i] == 0x1b && data[i + 1] == b'[' && data[i + 2] == b'?' {
            let mut j = i + 3;
            let mut modes = [0u32; 8];
            let mut count = 0;
            loop {
                let start = j;
                let mut mode: u32 = 0;
                while j < data.len() && data[j].is_ascii_digit() {
                    mode = mode.saturating_mul(10).saturating_add((data[j] - b'0') as u32);
                    j += 1;
                }
                if j == start {
                    break;
                }
                if count < modes.len() {
                    modes[count] = mode;
                    count += 1;
                }
                if j < data.len() && data[j] == b';' {
                    j += 1;
                    continue;
                }
                break;
            }
            if count > 0 && j < data.len() && (data[j] == b'h' || data[j] == b'l') {
                let on = data[j] == b'h';
                for &mode in &modes[..count] {
                    f(mode, on);
                }
                i = j + 1;
                continue;
            }
        }
        i += 1;
    }
}

/// Applies the mouse mode toggles found in `data` to the session state.
fn apply_mouse_events(data: &[u8], state: &AtomicU8) {
    for_each_mouse_mode(data, |mode, on| {
        if let Some(bit) = mouse_bit(mode) {
            if on {
                state.fetch_or(bit, Ordering::Relaxed);
            } else {
                state.fetch_and(!bit, Ordering::Relaxed);
            }
        }
    });
}

/// Maximum number of trailing bytes kept between reads to complete a mouse
/// mode sequence split across a read boundary.
const MOUSE_CARRY_MAX: usize = 256;

/// Tail of `scan` that may still hold an incomplete escape sequence.
///
/// Keeps everything from the last ESC within the final `MOUSE_CARRY_MAX` bytes,
/// so the next read can complete a sequence that was split. The cap bounds
/// memory when an ESC is followed by data that never terminates.
pub fn mouse_carry_tail(scan: &[u8]) -> &[u8] {
    let window = &scan[scan.len().saturating_sub(MOUSE_CARRY_MAX)..];
    match window.iter().rposition(|&b| b == 0x1b) {
        Some(pos) => &window[pos..],
        None => &[][..],
    }
}

/// Scans one read (`data`) together with the previous read's tail (`carry`),
/// applies the mouse mode toggles found to `state`, and refreshes `carry` with
/// the bytes a sequence split across reads may still need.
///
/// Multi-parameter sequences such as `ESC[?1000;1002;1003;1006h` are recognised
/// no matter where the read boundary falls.
pub fn scan_mouse_modes(data: &[u8], carry: &mut Vec<u8>, state: &AtomicU8) {
    let mut scan = Vec::with_capacity(carry.len() + data.len());
    scan.extend_from_slice(carry);
    scan.extend_from_slice(data);

    apply_mouse_events(&scan, state);

    carry.clear();
    carry.extend_from_slice(mouse_carry_tail(&scan));
}

fn create_pipe(path: &str, access: windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES) -> Result<HANDLE> {
    let wide: Vec<u16> = std::os::windows::ffi::OsStrExt::encode_wide(
        std::ffi::OsStr::new(path),
    )
    .chain(Some(0))
    .collect();

    let h = unsafe {
        CreateNamedPipeW(
            windows::core::PCWSTR::from_raw(wide.as_ptr()),
            access,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            4096,
            4096,
            NMPWAIT_WAIT_FOREVER,
            None,
        )
    };
    if h.is_invalid() {
        anyhow::bail!("CreateNamedPipe failed for {}", path);
    }
    Ok(h)
}

/// Cancel a blocking synchronous I/O call in the given thread.
fn cancel_thread_io(tid: Option<u32>) {
    if let Some(tid) = tid {
        unsafe {
            use windows::Win32::System::IO::CancelSynchronousIo;
            use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
            if let Ok(ht) = OpenThread(THREAD_TERMINATE, false, tid) {
                let _ = CancelSynchronousIo(ht);
                let _ = CloseHandle(ht);
            }
        }
    }
}

/// Keep the PTY output flowing while no client is attached, so a detached
/// TUI does not block on a full output pipe. Discards the bytes: the
/// console buffer in ConPTY keeps the screen state for the reattach repaint.
fn run_drain_thread(
    pty: Arc<Mutex<Pty>>,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    mouse: Arc<AtomicU8>,
) {
    enum Status {
        Attached,
        Read(usize),
        Idle,
        Dead,
    }

    let mut buf = [0u8; 8192];
    let mut mouse_carry: Vec<u8> = Vec::new();
    let mut idle_ticks: u32 = 0;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // The attach flag is checked under the same pty lock the out thread
        // takes when it claims the stream, so the switch-over cannot tear.
        let status = {
            let p = pty.lock().unwrap();
            if connected.load(Ordering::Relaxed) {
                Status::Attached
            } else {
                match p.available() {
                    // Read under the same lock as the availability check: a
                    // blocking read with no data would hold the pty lock and
                    // stall is_alive / resize / stop.
                    Ok(n) if n > 0 => match p.read_output(&mut buf) {
                        Ok(n) if n > 0 => Status::Read(n as usize),
                        _ => Status::Dead,
                    },
                    Ok(_) => Status::Idle,
                    Err(_) => Status::Dead,
                }
            }
        };

        match status {
            Status::Read(n) => {
                idle_ticks = 0;
                // Remember which mouse modes the app has enabled, so a later
                // attach can restore them.
                scan_mouse_modes(&buf[..n], &mut mouse_carry, &mouse);
            }
            Status::Attached => {
                idle_ticks = 0;
                thread::sleep(Duration::from_millis(5));
            }
            Status::Idle => {
                idle_ticks = idle_ticks.wrapping_add(1);
                // Confirm the shell is still alive now and then, so the drain
                // thread cannot keep a dead session's Pty alive forever.
                if idle_ticks % 200 == 0 && !pty.lock().unwrap().is_running() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Status::Dead => break,
        }
    }
}

/// Accept one client, forward input to the PTY, then loop for the next client.
fn run_in_thread(
    server: HANDLE,
    input_write: HANDLE,
    state: Arc<Mutex<SessionState>>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    detach: Arc<AtomicBool>,
    needs_repaint: Arc<AtomicBool>,
    generation: Arc<AtomicU32>,
    gate: Arc<Mutex<()>>,
) {
    let mut buf = [0u8; 8192];
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let connected = unsafe {
            crate::protocol::is_pipe_connected(&ConnectNamedPipe(server, None))
        };
        if !connected {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // A failed ConnectNamedPipe can leave the instance unusable; recycle
            // it and back off instead of spinning at full speed.
            unsafe {
                let _ = DisconnectNamedPipe(server);
            }
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        // Publish the new connection under the gate, so a detach helper that is
        // between its generation check and its cancel cannot hit this client:
        // it either sees this generation in the re-check, or it holds the gate
        // while this thread waits here (with no I/O in flight).
        {
            let _gate = gate.lock().unwrap();
            generation.fetch_add(1, Ordering::SeqCst);
            // A new client owns this direction now; stop cancelling for the old
            // one.
            detach.store(false, Ordering::Relaxed);
        }

        set_attached(&state);
        alive.store(true, Ordering::Relaxed);

        // Read client input until the client disconnects or a detach is asked for.
        loop {
            if stop.load(Ordering::Relaxed) || detach.load(Ordering::Relaxed) {
                break;
            }
            let mut read: u32 = 0;
            let result = unsafe {
                ReadFile(server, Some(&mut buf), Some(&mut read), None)
            };
            match result {
                Ok(()) if read == 0 => break,
                Ok(()) => {
                    // Written through our own duplicate of the PTY input handle,
                    // without the pty lock: WriteFile can block while a large
                    // paste drains, and the out/drain threads need the lock to
                    // keep reading output. Short writes are completed by the
                    // helper.
                    let _ = crate::protocol::write_all_pipe(
                        input_write,
                        &buf[..read as usize],
                    );
                }
                Err(_) => break,
            }
        }

        alive.store(false, Ordering::Relaxed);
        set_detached(&state);
        needs_repaint.store(true, Ordering::Relaxed);

        unsafe {
            let _ = DisconnectNamedPipe(server);
        }
    }

    unsafe {
        let _ = CloseHandle(server);
        let _ = CloseHandle(input_write);
    }
}

/// Accept one client, forward PTY output to it, then loop for the next client.
fn run_out_thread(
    server: HANDLE,
    pty: Arc<Mutex<Pty>>,
    state: Arc<Mutex<SessionState>>,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    detach: Arc<AtomicBool>,
    needs_repaint: Arc<AtomicBool>,
    mouse: Arc<AtomicU8>,
    generation: Arc<AtomicU32>,
    gate: Arc<Mutex<()>>,
) {
    enum LoopState {
        Data(usize),
        Idle,
        Dead,
    }

    let mut buf = [0u8; 8192];
    let mut mouse_carry: Vec<u8> = Vec::new();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        let connected = unsafe {
            crate::protocol::is_pipe_connected(&ConnectNamedPipe(server, None))
        };
        if !connected {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            // A failed ConnectNamedPipe can leave the instance unusable; recycle
            // it and back off instead of spinning at full speed.
            unsafe {
                let _ = DisconnectNamedPipe(server);
            }
            thread::sleep(Duration::from_millis(20));
            continue;
        }

        // Publish the new connection under the gate, so a detach helper that is
        // between its generation check and its cancel cannot hit this client:
        // it either sees this generation in the re-check, or it holds the gate
        // while this thread waits here (with no I/O in flight).
        {
            let _gate = gate.lock().unwrap();
            generation.fetch_add(1, Ordering::SeqCst);
            // A new client owns this direction now; stop cancelling for the old
            // one.
            detach.store(false, Ordering::Relaxed);
        }

        // Claim the output stream under the pty lock so the drain thread cannot
        // read (and discard) bytes after this client has taken over.
        {
            let _p = pty.lock().unwrap();
            alive.store(true, Ordering::Relaxed);
        }

        set_attached(&state);

        // Toggling the size makes ConPTY re-emit the whole screen, restoring
        // TUIs that kept running while detached. Done without the pty lock: the
        // redraw can fill the output pipe and the forward loop below must stay
        // free to drain it.
        if needs_repaint.swap(false, Ordering::Relaxed) {
            let (hpc, c, r) = {
                let p = pty.lock().unwrap();
                (p.hpc, p.cols, p.rows)
            };
            if r > 1 {
                unsafe {
                    let _ = ResizePseudoConsole(hpc, COORD { X: c as i16, Y: (r - 1) as i16 });
                    // Let the hosted app observe the shrink before restoring.
                    // Sent back to back it sees only the final size and skips
                    // the redraw, leaving the bottom row stale (the console
                    // client's force_repaint waits for the same reason).
                    thread::sleep(Duration::from_millis(120));
                    // Restore with retries: a failed restore would leave the
                    // session one row short until the next resize request.
                    for _ in 0..3 {
                        if ResizePseudoConsole(hpc, COORD { X: c as i16, Y: r as i16 }).is_ok() {
                            break;
                        }
                    }
                }
            }
        }

        'forward: loop {
            // Checked on every iteration, including while output is flowing: a
            // detach or client loss must disconnect even under constant load.
            if stop.load(Ordering::Relaxed)
                || detach.load(Ordering::Relaxed)
                || !alive.load(Ordering::Relaxed)
            {
                break 'forward;
            }

            // Availability and the read happen under one lock: the drain thread
            // is a second reader, and a read issued after a stale "available"
            // sample would block while holding the pty lock.
            let state = {
                let p = pty.lock().unwrap();
                match p.available() {
                    Ok(n) if n > 0 => match p.read_output(&mut buf) {
                        Ok(n) if n > 0 => LoopState::Data(n as usize),
                        // Broken pipe: child process exited.
                        _ => LoopState::Dead,
                    },
                    Ok(_) => LoopState::Idle,
                    Err(_) => LoopState::Dead,
                }
            };

            match state {
                LoopState::Data(n) => {
                    // Track the app's mouse modes for the current and future
                    // attach cycles.
                    scan_mouse_modes(&buf[..n], &mut mouse_carry, &mouse);

                    let mut pos: usize = 0;
                    while pos < n {
                        let mut written: u32 = 0;
                        let r = unsafe {
                            WriteFile(server, Some(&buf[pos..n]), Some(&mut written), None)
                        };
                        match r {
                            Ok(()) if written > 0 => pos += written as usize,
                            Ok(()) => break,
                            Err(_) => break 'forward,
                        }
                    }
                }
                LoopState::Idle => {
                    // No pending output. Stop if the client left or the shell exited.
                    if !alive.load(Ordering::Relaxed) {
                        break 'forward;
                    }
                    if !pty.lock().unwrap().is_running() {
                        break 'forward;
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                LoopState::Dead => break 'forward,
            }
        }

        alive.store(false, Ordering::Relaxed);
        set_detached(&state);
        needs_repaint.store(true, Ordering::Relaxed);

        unsafe {
            let _ = DisconnectNamedPipe(server);
        }
    }

    unsafe {
        let _ = CloseHandle(server);
    }
}

fn set_attached(state: &Arc<Mutex<SessionState>>) {
    *state.lock().unwrap() = SessionState::Attached;
}

fn set_detached(state: &Arc<Mutex<SessionState>>) {
    let mut s = state.lock().unwrap();
    // Do not overwrite a terminal Stopped state.
    if *s != SessionState::Stopped {
        *s = SessionState::Detached;
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::time::{Duration, Instant};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
        SECURITY_SQOS_PRESENT,
    };
    use windows::Win32::System::Pipes::PeekNamedPipe;

    fn open(path: &str, access: u32) -> HANDLE {
        let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let r = unsafe {
                CreateFileW(
                    PCWSTR::from_raw(wide.as_ptr()),
                    access,
                    FILE_SHARE_MODE(0),
                    None,
                    OPEN_EXISTING,
                    SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION,
                    None,
                )
            };
            match r {
                Ok(h) => return h,
                Err(e) => {
                    if Instant::now() > deadline {
                        panic!("open {} failed: {}", path, e);
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }

    /// Ok(n) with n == 0 when nothing is available; Err when the pipe is closed.
    fn read_some(h: HANDLE, buf: &mut [u8]) -> Result<usize, u32> {
        let mut avail: u32 = 0;
        unsafe {
            PeekNamedPipe(h, None, 0, None, Some(&mut avail), None)
                .map_err(|e| e.code().0 as u32)?;
        }
        if avail == 0 {
            return Ok(0);
        }
        let want = (avail as usize).min(buf.len());
        let mut read: u32 = 0;
        unsafe {
            ReadFile(h, Some(&mut buf[..want]), Some(&mut read), None)
                .map_err(|e| e.code().0 as u32)?;
        }
        Ok(read as usize)
    }

    /// `stop` must not wait for listener threads that were never spawned: the
    /// daemon drops a session on the start-failure path while holding the
    /// session table lock.
    #[test]
    fn stop_without_listener_returns_immediately() {
        let session = Session::new("unit-nostart", 80, 25, "cmd.exe").unwrap();
        let started = Instant::now();
        session.stop().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "stop waited {:?} with no listener thread",
            started.elapsed()
        );
    }

    /// Detaching while output is flowing must disconnect the client promptly and
    /// leave the shell alive and reattachable.
    #[test]
    fn detach_under_load_keeps_shell() {
        let session = Session::new("unit-detach", 80, 25, "cmd.exe").unwrap();
        session.start_data_pipe_listener().unwrap();
        let (out_path, in_path) = session.data_pipe_paths();

        let h_out = open(&out_path, GENERIC_READ.0);
        let h_in = open(&in_path, GENERIC_WRITE.0);

        // Keep the shell busy producing output.
        let cmd = b"for /L %i in (1,1,1000000) do @echo LINE %i\r\n";
        let mut written: u32 = 0;
        unsafe { WriteFile(h_in, Some(cmd), Some(&mut written), None).unwrap() };

        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut total = 0usize;
        while Instant::now() < deadline && total < 4096 {
            match read_some(h_out, &mut buf) {
                Ok(n) => total += n,
                Err(_) => panic!("out pipe broke before detach"),
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(total >= 4096, "expected output before detach");

        session.detach_client();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < deadline {
            match read_some(h_out, &mut buf) {
                Ok(_) => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(closed, "detach did not disconnect the client");
        assert!(session.is_alive(), "shell exited during detach");

        // A real client process exits here, closing both of its handles.
        unsafe {
            let _ = CloseHandle(h_out);
            let _ = CloseHandle(h_in);
        }

        let h_out2 = open(&out_path, GENERIC_READ.0);
        let h_in2 = open(&in_path, GENERIC_WRITE.0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = 0usize;
        while Instant::now() < deadline && got == 0 {
            match read_some(h_out2, &mut buf) {
                Ok(n) => got += n,
                Err(_) => break,
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(got > 0, "no output after reattach");

        unsafe {
            let _ = CloseHandle(h_out2);
            let _ = CloseHandle(h_in2);
        }
        session.stop().unwrap();
    }

    /// While detached, the daemon must keep draining PTY output; otherwise a
    /// program producing more than one pipe buffer blocks and never finishes.
    #[test]
    fn detached_program_is_not_blocked() {
        let marker = std::env::temp_dir().join(format!("sm-drain-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);

        let session = Session::new("unit-drain", 80, 25, "cmd.exe").unwrap();
        session.start_data_pipe_listener().unwrap();
        let (out_path, in_path) = session.data_pipe_paths();

        let h_out = open(&out_path, GENERIC_READ.0);
        let h_in = open(&in_path, GENERIC_WRITE.0);

        // More than one 64 KiB pipe buffer of output, then a marker file.
        let cmd = format!(
            "(for /L %i in (1,1,20000) do @echo LINE %i) & echo DONE> \"{}\"\r\n",
            marker.display()
        );
        let mut written: u32 = 0;
        unsafe { WriteFile(h_in, Some(cmd.as_bytes()), Some(&mut written), None).unwrap() };

        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut total = 0usize;
        while Instant::now() < deadline && total < 4096 {
            match read_some(h_out, &mut buf) {
                Ok(n) => total += n,
                Err(_) => break,
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(total >= 4096, "expected output before detach");

        session.detach_client();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < deadline {
            match read_some(h_out, &mut buf) {
                Ok(_) => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(closed, "detach did not disconnect the client");

        // A real client process exits here, closing both of its handles.
        unsafe {
            let _ = CloseHandle(h_out);
            let _ = CloseHandle(h_in);
        }

        // Without the drain thread the loop would stall once the 64 KiB output
        // pipe fills, so the marker would never appear.
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut contents = String::new();
        loop {
            if let Ok(c) = std::fs::read_to_string(&marker) {
                contents = c;
                if contents.contains("DONE") {
                    break;
                }
            }
            if Instant::now() > deadline {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        assert!(marker.exists(), "detached program did not finish");
        assert!(contents.contains("DONE"), "marker missing DONE");

        // Reattach and force a repaint.
        let h_out2 = open(&out_path, GENERIC_READ.0);
        let h_in2 = open(&in_path, GENERIC_WRITE.0);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got = 0usize;
        while Instant::now() < deadline && got == 0 {
            match read_some(h_out2, &mut buf) {
                Ok(n) => got += n,
                Err(_) => break,
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(got > 0, "no output after reattach");

        unsafe {
            let _ = CloseHandle(h_out2);
            let _ = CloseHandle(h_in2);
        }
        session.stop().unwrap();
        let _ = std::fs::remove_file(&marker);
    }

    #[test]
    fn mouse_modes_are_tracked() {
        let state = AtomicU8::new(0);
        apply_mouse_events(b"\x1b[?1003;1006h", &state);
        assert_eq!(state.load(Ordering::Relaxed), 4 | 8);
        apply_mouse_events(b"\x1b[?1003l", &state);
        assert_eq!(state.load(Ordering::Relaxed), 8);
        apply_mouse_events(b"\x1b[?1000h\x1b[?1002h", &state);
        assert_eq!(state.load(Ordering::Relaxed), 8 | 1 | 2);
        apply_mouse_events(b"\x1b[?1000l\x1b[?1002l\x1b[?1006l", &state);
        assert_eq!(state.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn mouse_scan_ignores_other_sequences() {
        let state = AtomicU8::new(0);
        apply_mouse_events(b"\x1b[1;2H\x1b[?25ltext\x1b[?1049h", &state);
        assert_eq!(state.load(Ordering::Relaxed), 0);
    }

    /// A multi-parameter mouse sequence must be recognised no matter where a
    /// read boundary splits it (23 bytes: longer than the old 16-byte carry).
    #[test]
    fn mouse_sequence_survives_any_read_split() {
        let seq = b"\x1b[?1000;1002;1003;1006h";
        assert_eq!(seq.len(), 23);
        for split in 0..=seq.len() {
            let state = AtomicU8::new(0);
            let mut carry = Vec::new();
            scan_mouse_modes(&seq[..split], &mut carry, &state);
            scan_mouse_modes(&seq[split..], &mut carry, &state);
            assert_eq!(
                state.load(Ordering::Relaxed),
                1 | 2 | 4 | 8,
                "split at byte {}",
                split
            );
        }
    }

    /// Splitting ordinary output (including non-mouse CSI sequences) across
    /// reads must not be mistaken for mouse mode changes.
    #[test]
    fn mouse_scan_ignores_split_non_mouse_sequences() {
        let data = b"\x1b[1;2H\x1b[?25ltext\x1b[?1049h";
        for split in 0..=data.len() {
            let state = AtomicU8::new(0);
            let mut carry = Vec::new();
            scan_mouse_modes(&data[..split], &mut carry, &state);
            scan_mouse_modes(&data[split..], &mut carry, &state);
            assert_eq!(state.load(Ordering::Relaxed), 0, "split at byte {}", split);
        }
    }

    #[test]
    fn mouse_carry_tail_keeps_partial_sequence() {
        assert!(mouse_carry_tail(b"plain text").is_empty());
        assert_eq!(mouse_carry_tail(b"text\x1b[?1000;"), b"\x1b[?1000;");

        // An ESC followed by more than the cap of non-sequence bytes must not
        // grow the carry without bound.
        let mut long = vec![b'a'; 64];
        long.push(0x1b);
        long.extend(std::iter::repeat(b'x').take(MOUSE_CARRY_MAX + 64));
        assert!(mouse_carry_tail(&long).len() <= MOUSE_CARRY_MAX);
    }
}
