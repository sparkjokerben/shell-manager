//! Bridge between a browser connection and one smd session.
//!
//! The daemon serialises every control request, so all control calls from one
//! `smw` process go through a single [`ControlClient`]. The data pipes are two
//! unframed byte streams with one instance per direction, so an attached
//! browser owns an [`Attachment`] with a reader thread (`out` pipe) and a
//! writer thread (`in` pipe).
//!
//! Blocking Win32 calls must never run on a tokio reactor thread: control
//! requests go through `spawn_blocking`, data-pipe I/O uses plain threads, and
//! [`Attachment::shutdown`] moves its teardown onto the blocking pool.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use shell_manager::protocol::{self, Op, Request, Response, SessionInfo};
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tokio::sync::watch;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, FILE_SHARE_MODE, OPEN_EXISTING, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT,
};

/// Capacity of the per-attachment output event channel (and the input queue).
pub const OUTPUT_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the browser-to-session input queue. Kept small because one
/// WebSocket message may be up to 1 MiB and a stuck writer would otherwise
/// retain hundreds of megabytes.
pub const INPUT_CHANNEL_CAPACITY: usize = 32;

/// How long a single control-pipe request may take before the caller gives up.
/// The daemon serialises control requests, and a stuck daemon must not pin an
/// HTTP handler (or the attachment slot) forever.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Size of one read from the data `out` pipe.
const READ_CHUNK: usize = 8 * 1024;

const ERROR_PIPE_BUSY: u32 = 0x8007_00E7;
const BUSY_WAIT_MS: u32 = 500;
const BUSY_ATTEMPTS: u32 = 6;

/// Delay before a takeover detach, giving the old client time to close its
/// data-pipe handles so the daemon can accept the new ones.
const TAKEOVER_GRACE: Duration = Duration::from_millis(300);

/// Shutdown waits at most this long for the reader thread to finish.
const READER_STOP_LIMIT: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Control pipe
// ---------------------------------------------------------------------------

/// Serialised client for the daemon control pipe.
///
/// The daemon serves control requests one at a time. Holding a mutex across the
/// blocking request keeps concurrent HTTP handlers from interleaving requests
/// on the wire and losing them to `ERROR_PIPE_BUSY`.
pub struct ControlClient {
    gate: tokio::sync::Mutex<()>,
}

impl Default for ControlClient {
    fn default() -> Self {
        Self {
            gate: tokio::sync::Mutex::new(()),
        }
    }
}

impl ControlClient {
    pub fn new() -> Self {
        Self::default()
    }

    /// Send one request and wait for its response.
    pub async fn request(&self, req: Request) -> anyhow::Result<Response> {
        let _guard = self.gate.lock().await;
        let task = tokio::task::spawn_blocking(move || {
            protocol::pipe_request(protocol::control_pipe(), &req)
        });
        match tokio::time::timeout(CONTROL_TIMEOUT, task).await {
            Ok(joined) => Ok(joined??),
            Err(_) => anyhow::bail!(
                "control request timed out after {}s (is smd responsive?)",
                CONTROL_TIMEOUT.as_secs()
            ),
        }
    }

    pub async fn list(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let response = self
            .request(Request {
                op: Op::List,
                name: String::new(),
                wsl: None,
                cols: None,
                rows: None,
            })
            .await?;
        if !response.ok {
            anyhow::bail!("list failed: {}", response.error);
        }
        let data = response
            .data
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
        Ok(serde_json::from_value(data)?)
    }

    pub async fn start(
        &self,
        name: &str,
        wsl: bool,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Response> {
        self.request(Request {
            op: Op::Start,
            name: name.to_string(),
            wsl: Some(wsl),
            cols: Some(cols),
            rows: Some(rows),
        })
        .await
    }

    pub async fn continue_session(
        &self,
        name: &str,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<Response> {
        self.request(Request {
            op: Op::Continue,
            name: name.to_string(),
            wsl: None,
            cols: Some(cols),
            rows: Some(rows),
        })
        .await
    }

    pub async fn resize(&self, name: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        let response = self
            .request(Request {
                op: Op::Resize,
                name: name.to_string(),
                wsl: None,
                cols: Some(cols),
                rows: Some(rows),
            })
            .await?;
        expect_ok(response, "resize")
    }

    pub async fn detach(&self, name: &str) -> anyhow::Result<()> {
        let response = self
            .request(Request {
                op: Op::Detach,
                name: name.to_string(),
                wsl: None,
                cols: None,
                rows: None,
            })
            .await?;
        expect_ok(response, "detach")
    }

    pub async fn stop(&self, name: &str) -> anyhow::Result<()> {
        let response = self
            .request(Request {
                op: Op::Stop,
                name: name.to_string(),
                wsl: None,
                cols: None,
                rows: None,
            })
            .await?;
        expect_ok(response, "stop")
    }
}

fn expect_ok(response: Response, action: &str) -> anyhow::Result<()> {
    if response.ok {
        Ok(())
    } else {
        anyhow::bail!("{} failed: {}", action, response.error)
    }
}

// ---------------------------------------------------------------------------
// Attach bookkeeping
// ---------------------------------------------------------------------------

/// Names attached in this `smw` process.
///
/// Cloning shares the same map, so an [`Attachment`] can release its slot even
/// though `attach` only receives a shared reference. Each slot also carries a
/// takeover flag so a new browser tab can ask the current attachment to stand
/// down.
#[derive(Clone, Default)]
pub struct AttachRegistry {
    names: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl AttachRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Claim `name`, returning the takeover flag for the new attachment.
    /// Returns `None` when it is already attached.
    pub fn try_acquire(&self, name: &str) -> Option<Arc<AtomicBool>> {
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        if names.contains_key(name) {
            return None;
        }
        let flag = Arc::new(AtomicBool::new(false));
        names.insert(name.to_string(), flag.clone());
        Some(flag)
    }

    /// Ask the attachment currently holding `name` to disconnect. Returns false
    /// when nobody holds it.
    pub fn kick(&self, name: &str) -> bool {
        let names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        match names.get(name) {
            Some(flag) => {
                flag.store(true, Ordering::Release);
                true
            }
            None => false,
        }
    }

    pub fn release(&self, name: &str) {
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        names.remove(name);
    }
}

/// Events a browser receives from the data `out` pipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputEvent {
    Data(Vec<u8>),
    Exit(String),
}

/// Why an attach could not be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// The session is already attached in this process.
    Busy,
    /// The daemon rejected the control request.
    Control(String),
    /// The data pipes could not be opened.
    Pipe(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::Busy => write!(f, "session is already attached in this smw process"),
            AttachError::Control(msg) => write!(f, "control request failed: {}", msg),
            AttachError::Pipe(msg) => write!(f, "data pipe failed: {}", msg),
        }
    }
}

impl std::error::Error for AttachError {}

// ---------------------------------------------------------------------------
// Win32 helpers
// ---------------------------------------------------------------------------

/// A handle shared between the attachment and its I/O thread.
///
/// Whoever gets there first closes it; a later close is a no-op, so the reader
/// thread can release the pipe as soon as it stops and `shutdown` can still
/// clean up if the thread never got that far.
#[derive(Clone)]
struct HandleSlot(Arc<Mutex<Option<HANDLE>>>);

impl HandleSlot {
    fn new(handle: HANDLE) -> Self {
        Self(Arc::new(Mutex::new(Some(handle))))
    }

    fn get(&self) -> Option<HANDLE> {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn close(&self) {
        let handle = self.0.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(handle) = handle {
            unsafe {
                let _ = CloseHandle(handle);
            }
        }
    }
}

/// Open one end of a session pipe.
///
/// The daemon creates a single instance per direction, so a previous client
/// that is still tearing down makes `CreateFile` fail with `ERROR_PIPE_BUSY`.
/// Wait for the instance to become free instead of failing the attach.
fn open_pipe(path: &str, access: u32) -> anyhow::Result<HANDLE> {
    use std::os::windows::ffi::OsStrExt;

    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(Some(0))
        .collect();

    for _ in 0..BUSY_ATTEMPTS {
        let handle = unsafe {
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
        match handle {
            Ok(handle) => return Ok(handle),
            Err(e) if (e.code().0 as u32) == ERROR_PIPE_BUSY => unsafe {
                let _ = windows::Win32::System::Pipes::WaitNamedPipeW(
                    PCWSTR::from_raw(wide.as_ptr()),
                    BUSY_WAIT_MS,
                );
            },
            Err(e) => anyhow::bail!("failed to connect to data pipe {}: {}", path, e),
        }
    }

    anyhow::bail!("timed out connecting to data pipe {}", path)
}

/// `open_pipe` on the blocking pool, so a busy pipe cannot stall the reactor.
async fn open_pipe_async(path: String, access: u32) -> anyhow::Result<HANDLE> {
    tokio::task::spawn_blocking(move || open_pipe(&path, access))
        .await
        .map_err(|e| anyhow::anyhow!("pipe open task failed: {}", e))?
}

/// Cancel a blocking synchronous I/O call in the given thread.
fn cancel_thread_io(tid: u32) {
    if tid == 0 {
        return;
    }
    unsafe {
        use windows::Win32::System::Threading::{OpenThread, THREAD_TERMINATE};
        use windows::Win32::System::IO::CancelSynchronousIo;

        if let Ok(handle) = OpenThread(THREAD_TERMINATE, false, tid) {
            let _ = CancelSynchronousIo(handle);
            let _ = CloseHandle(handle);
        }
    }
}

// ---------------------------------------------------------------------------
// Attachment
// ---------------------------------------------------------------------------

/// One attached session: browser data flows through `out_rx`/`in_tx`.
pub struct Attachment {
    pub name: String,
    /// DEC mouse modes the hosted app had enabled at attach time.
    pub mouse: u8,
    out_rx: Option<Receiver<OutputEvent>>,
    in_tx: Option<Sender<Vec<u8>>>,
    out_handle: HandleSlot,
    in_handle: HandleSlot,
    reader: Option<std::thread::JoinHandle<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    reader_tid: Arc<AtomicU32>,
    reader_done: Arc<AtomicBool>,
    writer_tid: Arc<AtomicU32>,
    writer_done: Arc<AtomicBool>,
    takeover: Arc<AtomicBool>,
    /// Fatal I/O notification from the reader or writer thread. A `watch`
    /// channel keeps the handler informed even when the output queue is full.
    status: watch::Receiver<Option<String>>,
    registry: AttachRegistry,
}

impl Attachment {
    /// Receiver of pipe output. Data is forwarded verbatim: xterm.js consumes
    /// the raw bytes, so no input-mode stripping or UTF-8 reassembly is needed.
    pub fn output_receiver(&mut self) -> &mut Receiver<OutputEvent> {
        self.out_rx.as_mut().expect("attachment already shut down")
    }

    /// Sender for keyboard/paste bytes. Dropping every copy ends the writer.
    pub fn input_sender(&self) -> Sender<Vec<u8>> {
        self.in_tx
            .as_ref()
            .expect("attachment already shut down")
            .clone()
    }

    /// Flag set when another browser tab asks this attachment to stand down.
    pub fn takeover_flag(&self) -> Arc<AtomicBool> {
        self.takeover.clone()
    }

    /// Fatal I/O notifications from the pipe threads.
    pub fn status_receiver(&self) -> watch::Receiver<Option<String>> {
        self.status.clone()
    }

    /// Tear the attachment down: stop both threads, close the pipes and release
    /// the registry slot. The blocking work runs on the blocking pool.
    pub async fn shutdown(mut self) {
        let parts = self.take_parts();
        if parts.is_active() {
            let _ = tokio::task::spawn_blocking(move || parts.shutdown()).await;
        }
        self.registry.release(&self.name);
    }

    fn take_parts(&mut self) -> BlockingParts {
        BlockingParts {
            in_tx: self.in_tx.take(),
            out_handle: self.out_handle.clone(),
            in_handle: self.in_handle.clone(),
            reader: self.reader.take(),
            writer: self.writer.take(),
            reader_tid: self.reader_tid.clone(),
            reader_done: self.reader_done.clone(),
            writer_tid: self.writer_tid.clone(),
            writer_done: self.writer_done.clone(),
        }
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        let parts = self.take_parts();
        if !parts.is_active() {
            return;
        }
        // A normal caller awaits `shutdown`. As a fallback do not block here
        // (the dropper may be a reactor thread): hand the bounded teardown to a
        // detached thread.
        let registry = self.registry.clone();
        let name = self.name.clone();
        std::thread::spawn(move || {
            parts.shutdown();
            registry.release(&name);
        });
    }
}

/// The thread handles and pipe handles needed for a bounded teardown.
struct BlockingParts {
    in_tx: Option<Sender<Vec<u8>>>,
    out_handle: HandleSlot,
    in_handle: HandleSlot,
    reader: Option<std::thread::JoinHandle<()>>,
    writer: Option<std::thread::JoinHandle<()>>,
    reader_tid: Arc<AtomicU32>,
    reader_done: Arc<AtomicBool>,
    writer_tid: Arc<AtomicU32>,
    writer_done: Arc<AtomicBool>,
}

impl BlockingParts {
    fn is_active(&self) -> bool {
        self.in_tx.is_some()
            || self.reader.is_some()
            || self.writer.is_some()
            || self.out_handle.get().is_some()
            || self.in_handle.get().is_some()
    }

    fn shutdown(mut self) {
        // Dropping the sender ends the writer's `blocking_recv` loop.
        drop(self.in_tx.take());

        // The reader is blocked in ReadFile. Wait for its thread id to appear,
        // then keep cancelling until it reports done, with a hard bound.
        let mut waited = 0;
        while self.reader_tid.load(Ordering::Acquire) == 0
            && !self.reader_done.load(Ordering::Acquire)
            && waited < 100
        {
            std::thread::sleep(Duration::from_millis(10));
            waited += 1;
        }

        let deadline = Instant::now() + READER_STOP_LIMIT;
        while !self.reader_done.load(Ordering::Acquire) && Instant::now() < deadline {
            cancel_thread_io(self.reader_tid.load(Ordering::Relaxed));
            std::thread::sleep(Duration::from_millis(20));
        }

        // A write may be stuck on a daemon that stopped reading; cancel it and
        // close our end as a fallback so WriteFile fails out.
        let writer_deadline = Instant::now() + READER_STOP_LIMIT;
        while !self.writer_done.load(Ordering::Acquire) && Instant::now() < writer_deadline {
            cancel_thread_io(self.writer_tid.load(Ordering::Relaxed));
            std::thread::sleep(Duration::from_millis(20));
        }
        if !self.writer_done.load(Ordering::Acquire) {
            self.in_handle.close();
        }

        // Last resort for the reader: closing our end makes the blocked read
        // fail so the join below cannot hang.
        if !self.reader_done.load(Ordering::Acquire) {
            self.out_handle.close();
        }

        // Only join threads that actually reported completion. Dropping a
        // JoinHandle detaches it, which beats blocking the async handler
        // forever when a cancellation never lands.
        if self.reader_done.load(Ordering::Acquire) {
            if let Some(handle) = self.reader.take() {
                let _ = handle.join();
            }
        } else {
            drop(self.reader.take());
        }
        if self.writer_done.load(Ordering::Acquire) {
            if let Some(handle) = self.writer.take() {
                let _ = handle.join();
            }
        } else {
            drop(self.writer.take());
        }

        // No-ops when the threads already closed their own end.
        self.out_handle.close();
        self.in_handle.close();
    }
}

/// Releases the attachment slot unless ownership moves to an [`Attachment`].
///
/// `attach` has several await points after claiming the slot. If the future is
/// dropped (runtime shutdown, caller cancellation) the guard releases it;
/// `disarm` hands the slot to the returned attachment instead.
struct SlotGuard {
    registry: AttachRegistry,
    name: String,
    armed: bool,
}

impl SlotGuard {
    fn new(registry: &AttachRegistry, name: &str) -> Self {
        Self {
            registry: registry.clone(),
            name: name.to_string(),
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.armed {
            self.registry.release(&self.name);
        }
    }
}

/// Open the data pipes and start the I/O threads.
pub async fn attach(
    control: &ControlClient,
    registry: &AttachRegistry,
    name: &str,
    cols: u16,
    rows: u16,
    takeover: bool,
) -> Result<Attachment, AttachError> {
    if takeover {
        // Ask an in-process attachment to stand down, detach any console
        // client, then give the daemon a moment to see the pipe handles close
        // before we try to open them.
        registry.kick(name);
        let _ = control.detach(name).await;
        tokio::time::sleep(TAKEOVER_GRACE).await;
    }

    let mut takeover_flag = registry.try_acquire(name);
    if takeover && takeover_flag.is_none() {
        // The previous browser connection may still be tearing down. Keep
        // signalling it and retry; teardown is bounded at roughly five seconds.
        for _ in 0..100 {
            registry.kick(name);
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Some(flag) = registry.try_acquire(name) {
                takeover_flag = Some(flag);
                break;
            }
        }
    }
    let Some(takeover_flag) = takeover_flag else {
        return Err(AttachError::Busy);
    };
    let slot = SlotGuard::new(registry, name);

    // Attach only; creating a missing session is the HTTP layer's job (POST).
    let response = match control.continue_session(name, cols, rows).await {
        Ok(response) => response,
        Err(e) => return Err(AttachError::Control(e.to_string())),
    };
    if !response.ok {
        return Err(AttachError::Control(response.error));
    }

    let (pipe_out, pipe_in, mouse) = match extract_pipes(&response.data) {
        Some(parts) => parts,
        None => {
            return Err(AttachError::Control(
                "control response did not include data pipe names".to_string(),
            ))
        }
    };

    let out_handle = match open_pipe_async(pipe_out, GENERIC_READ.0).await {
        Ok(handle) => HandleSlot::new(handle),
        Err(e) => return Err(AttachError::Pipe(e.to_string())),
    };
    let in_handle = match open_pipe_async(pipe_in, GENERIC_WRITE.0).await {
        Ok(handle) => HandleSlot::new(handle),
        Err(e) => {
            out_handle.close();
            return Err(AttachError::Pipe(e.to_string()));
        }
    };

    let (out_tx, out_rx) = mpsc::channel(OUTPUT_CHANNEL_CAPACITY);
    let (in_tx, in_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
    let (status_tx, status_rx) = watch::channel(None::<String>);

    let reader_tid = Arc::new(AtomicU32::new(0));
    let reader_done = Arc::new(AtomicBool::new(false));
    let writer_tid = Arc::new(AtomicU32::new(0));
    let writer_done = Arc::new(AtomicBool::new(false));

    let reader = spawn_reader(
        out_handle.clone(),
        out_tx.clone(),
        reader_tid.clone(),
        reader_done.clone(),
        status_tx.clone(),
    );
    let writer = spawn_writer(
        in_handle.clone(),
        in_rx,
        writer_tid.clone(),
        writer_done.clone(),
        status_tx,
    );

    let attachment = Attachment {
        name: name.to_string(),
        mouse,
        out_rx: Some(out_rx),
        in_tx: Some(in_tx),
        out_handle,
        in_handle,
        reader: Some(reader),
        writer: Some(writer),
        reader_tid,
        reader_done,
        writer_tid,
        writer_done,
        takeover: takeover_flag,
        status: status_rx,
        registry: registry.clone(),
    };
    slot.disarm();
    Ok(attachment)
}

fn extract_pipes(data: &Option<serde_json::Value>) -> Option<(String, String, u8)> {
    let data = data.as_ref()?;
    let pipe_out = data.get("pipe_out")?.as_str()?.to_string();
    let pipe_in = data.get("pipe_in")?.as_str()?.to_string();
    let mouse = data
        .get("mouse")
        .and_then(|value| value.as_u64())
        .unwrap_or(0) as u8;
    Some((pipe_out, pipe_in, mouse))
}

/// Report a fatal reader condition through both the event queue and the
/// status watch. The queue may be full (it is usually why the reader is
/// stopping), so the watch channel is the reliable path.
fn finish_reader(
    status: &watch::Sender<Option<String>>,
    events: &Sender<OutputEvent>,
    message: String,
) {
    let _ = status.send(Some(message.clone()));
    let _ = events.try_send(OutputEvent::Exit(message));
}

/// Data `out` pipe -> `OutputEvent` channel.
fn spawn_reader(
    handle: HandleSlot,
    events: Sender<OutputEvent>,
    tid: Arc<AtomicU32>,
    done: Arc<AtomicBool>,
    status: watch::Sender<Option<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let current = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        tid.store(current, Ordering::Release);

        let mut buf = [0u8; READ_CHUNK];
        loop {
            let Some(pipe) = handle.get() else {
                break;
            };
            let mut read: u32 = 0;
            let result = unsafe { ReadFile(pipe, Some(&mut buf), Some(&mut read), None) };
            match result {
                Ok(()) if read == 0 => {
                    finish_reader(&status, &events, "session ended".to_string());
                    break;
                }
                Ok(()) => {
                    let chunk = buf[..read as usize].to_vec();
                    match events.try_send(OutputEvent::Data(chunk)) {
                        Ok(()) => {}
                        // The browser is not keeping up. Detach rather than
                        // block the pipe, which would stall the session.
                        Err(TrySendError::Full(_)) => {
                            finish_reader(
                                &status,
                                &events,
                                "output backlog exceeded; detached".to_string(),
                            );
                            break;
                        }
                        Err(TrySendError::Closed(_)) => {
                            let _ = status.send(Some("session ended".to_string()));
                            break;
                        }
                    }
                }
                Err(e) => {
                    finish_reader(&status, &events, format!("output read failed: {}", e));
                    break;
                }
            }
        }

        handle.close();
        done.store(true, Ordering::Release);
    })
}

/// Queued browser input -> data `in` pipe.
fn spawn_writer(
    handle: HandleSlot,
    mut input: Receiver<Vec<u8>>,
    tid: Arc<AtomicU32>,
    done: Arc<AtomicBool>,
    status: watch::Sender<Option<String>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let current = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
        tid.store(current, Ordering::Release);

        // `blocking_recv` is only legal outside the async runtime; this is a
        // plain thread, so it can sit on the queue without holding the reactor.
        while let Some(data) = input.blocking_recv() {
            let Some(pipe) = handle.get() else {
                break;
            };
            if let Err(e) = protocol::write_all_pipe(pipe, &data) {
                let _ = status.send(Some(format!("session input failed: {}", e)));
                break;
            }
        }

        handle.close();
        done.store(true, Ordering::Release);
    })
}

/// Ask the hosted app to redraw its whole screen after attaching.
///
/// Apps redraw when the console size changes, so shrink the pseudo console by
/// one row and restore it. The pause between the two is essential: sent back to
/// back, the app can see only the final size and skip the redraw, which leaves
/// the last line of a TUI untouched.
pub async fn force_repaint(
    control: &ControlClient,
    name: &str,
    cols: u16,
    rows: u16,
    abort: &AtomicBool,
) {
    if rows < 2 {
        let _ = control.resize(name, cols, rows.max(1)).await;
        return;
    }
    tokio::time::sleep(Duration::from_millis(120)).await;
    if abort.load(Ordering::Acquire) {
        return;
    }
    let _ = control.resize(name, cols, rows - 1).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    if abort.load(Ordering::Acquire) {
        return;
    }
    let _ = control.resize(name, cols, rows).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_is_exclusive_per_name() {
        let registry = AttachRegistry::new();
        assert!(registry.try_acquire("alpha").is_some());
        assert!(registry.try_acquire("alpha").is_none());
        let beta = registry.try_acquire("beta").expect("beta is free");
        assert!(!registry.kick("missing"));
        assert!(registry.kick("beta"));
        assert!(beta.load(Ordering::Acquire));

        registry.release("alpha");
        assert!(registry.try_acquire("alpha").is_some());
        assert!(registry.try_acquire("beta").is_none());
    }

    #[test]
    fn registry_clones_share_the_same_set() {
        let registry = AttachRegistry::new();
        let clone = registry.clone();

        assert!(registry.try_acquire("alpha").is_some());
        assert!(clone.try_acquire("alpha").is_none());

        clone.release("alpha");
        assert!(registry.try_acquire("alpha").is_some());
    }

    #[test]
    fn kick_targets_only_the_named_attachment() {
        let registry = AttachRegistry::new();
        let alpha = registry.try_acquire("alpha").expect("alpha is free");
        let beta = registry.try_acquire("beta").expect("beta is free");

        assert!(registry.kick("alpha"));
        assert!(alpha.load(Ordering::Acquire));
        assert!(!beta.load(Ordering::Acquire));
    }

    #[test]
    fn attach_errors_have_useful_messages() {
        assert!(AttachError::Busy.to_string().contains("already attached"));
        assert!(AttachError::Control("boom".into())
            .to_string()
            .contains("boom"));
        assert!(AttachError::Pipe("lost".into())
            .to_string()
            .contains("lost"));
    }

    #[test]
    fn output_events_keep_data_and_exit_distinct() {
        let data = OutputEvent::Data(vec![1, 2, 3]);
        let exit = OutputEvent::Exit("session ended".into());
        assert_ne!(data, exit);
        assert_eq!(data, OutputEvent::Data(vec![1, 2, 3]));
        assert_eq!(OUTPUT_CHANNEL_CAPACITY, 256);

        match data {
            OutputEvent::Data(bytes) => assert_eq!(bytes, vec![1, 2, 3]),
            OutputEvent::Exit(_) => panic!("expected data"),
        }
    }

    #[test]
    fn extract_pipes_requires_both_paths() {
        let both = serde_json::json!({
            "pipe_out": r"\\.\pipe\out",
            "pipe_in": r"\\.\pipe\in",
            "mouse": 3,
        });
        let (out, inp, mouse) = extract_pipes(&Some(both)).unwrap();
        assert!(out.ends_with("out"));
        assert!(inp.ends_with("in"));
        assert_eq!(mouse, 3);

        let missing = serde_json::json!({ "pipe_out": r"\\.\pipe\out" });
        assert!(extract_pipes(&Some(missing)).is_none());
        assert!(extract_pipes(&None).is_none());
    }
}
