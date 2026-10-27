//! smpaste: forward clipboard images to shell-manager hosts over SSH.
//!
//! The process polls the local clipboard, encodes every new image as a
//! 32-bit bottom-up BMP, and writes it to a persistent remote
//! `sm clipboard-image --stream` process using a simple length-prefixed
//! framing (4-byte big-endian length followed by the BMP bytes).
//!
//! With `--ssh-host` it forwards to exactly that host. Without it the process
//! periodically scans for interactive `ssh` sessions and keeps one stream per
//! discovered host.

use std::io::Write;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use arboard::{Clipboard, Error as ClipboardError, ImageData};

mod discover;

use crate::discover::SshTarget;

/// BMP file header (14 bytes) plus BITMAPINFOHEADER (40 bytes).
const BMP_PIXEL_OFFSET: usize = 14 + 40;
/// Upper bound for one BMP frame, matching the host-side check.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Delay before trying to start a new SSH stream after a failure.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Upper bound for the exponential backoff of discovered hosts.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Default clipboard poll interval.
const DEFAULT_INTERVAL_MS: u64 = 200;
/// Default interval between process-list scans for interactive SSH sessions.
const DEFAULT_SCAN_MS: u64 = 5000;
const DEFAULT_REMOTE_SM: &str = "sm";
/// Pixels per meter resolution stored in the BMP info header (72 DPI).
const PIXELS_PER_METER: i32 = 2835;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Cheap clipboard generation counter.
///
/// Reading image data on every poll is expensive and, on macOS, can trigger
/// repeated clipboard-permission prompts. When the platform exposes a change
/// counter we only read the image after it moves. `None` means there is no
/// cheap counter, so the caller falls back to polling the data every interval.
mod platform {
    #[cfg(windows)]
    pub fn clipboard_sequence() -> Option<u64> {
        Some(unsafe {
            windows::Win32::System::DataExchange::GetClipboardSequenceNumber() as u64
        })
    }

    #[cfg(target_os = "macos")]
    pub fn clipboard_sequence() -> Option<u64> {
        let pasteboard = objc2_app_kit::NSPasteboard::generalPasteboard();
        Some(pasteboard.changeCount() as u64)
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    pub fn clipboard_sequence() -> Option<u64> {
        None
    }
}

const USAGE: &str = concat!(
    "smpaste ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "Forward clipboard images to a shell-manager host over SSH.\n",
    "\n",
    "Usage:\n",
    "  smpaste [--ssh-host <alias>] [--remote-sm <command>] [--interval-ms <ms>] [--scan-ms <ms>]\n",
    "\n",
    "Options:\n",
    "  --ssh-host <alias>     SSH host alias or user@host; fixes a single host\n",
    "                         (default: discover interactive ssh sessions)\n",
    "  --remote-sm <command>  Remote shell-manager command (default: sm)\n",
    "  --interval-ms <ms>     Clipboard poll interval in milliseconds (default: 200)\n",
    "  --scan-ms <ms>         SSH discovery interval in milliseconds (default: 5000)\n",
    "  -h, --help             Print this help and exit\n",
    "\n",
    "Without --ssh-host the process scans for interactive ssh sessions every\n",
    "--scan-ms, keeps one stream per host, and sends the newest clipboard image\n",
    "to every connected host. Automatic discovery is not available on Windows.\n",
    "\n",
    "Each image is sent as one length-prefixed BMP frame; the process reconnects\n",
    "automatically after failures.\n",
);

fn main() {
    if let Err(err) = real_main() {
        eprintln!("smpaste: {err:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    match parse_args(std::env::args().skip(1))? {
        Cli::Help => {
            print!("{USAGE}");
            Ok(())
        }
        Cli::Run(opts) => run(opts),
    }
}

#[derive(Debug)]
struct Options {
    /// `None` means automatic discovery.
    ssh_host: Option<String>,
    remote_sm: String,
    interval: Duration,
    /// How often to rescan for interactive SSH sessions. Only used when
    /// `ssh_host` is `None`.
    scan: Duration,
}

#[derive(Debug)]
enum Cli {
    Help,
    Run(Options),
}

/// Parse command-line arguments without an extra dependency.
///
/// Both `--flag value` and `--flag=value` forms are accepted.
fn parse_args<I>(args: I) -> Result<Cli>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter();
    let mut ssh_host: Option<String> = None;
    let mut remote_sm: Option<String> = None;
    let mut interval_ms = DEFAULT_INTERVAL_MS;
    let mut scan_ms = DEFAULT_SCAN_MS;

    while let Some(arg) = args.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) => (flag.to_string(), Some(value.to_string())),
            None => (arg.clone(), None),
        };

        match flag.as_str() {
            "-h" | "--help" => return Ok(Cli::Help),
            "--ssh-host" => {
                ssh_host = Some(take_value("--ssh-host", inline, &mut args)?);
            }
            "--remote-sm" => {
                remote_sm = Some(take_value("--remote-sm", inline, &mut args)?);
            }
            "--interval-ms" => {
                let value = take_value("--interval-ms", inline, &mut args)?;
                interval_ms = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid --interval-ms value {value:?}"))?;
                if interval_ms == 0 {
                    bail!("--interval-ms must be greater than zero");
                }
            }
            "--scan-ms" => {
                let value = take_value("--scan-ms", inline, &mut args)?;
                scan_ms = value
                    .parse::<u64>()
                    .with_context(|| format!("invalid --scan-ms value {value:?}"))?;
                if scan_ms == 0 {
                    bail!("--scan-ms must be greater than zero");
                }
            }
            _ => bail!("unknown argument {arg:?}\n\n{USAGE}"),
        }
    }

    if let Some(host) = ssh_host.as_deref() {
        if host.is_empty() {
            bail!("--ssh-host must not be empty");
        }
    }

    let remote_sm = remote_sm.unwrap_or_else(|| DEFAULT_REMOTE_SM.to_string());
    if remote_sm.is_empty() {
        bail!("--remote-sm must not be empty");
    }

    Ok(Cli::Run(Options {
        ssh_host,
        remote_sm,
        interval: Duration::from_millis(interval_ms),
        scan: Duration::from_millis(scan_ms),
    }))
}

fn take_value(
    flag: &str,
    inline: Option<String>,
    args: &mut impl Iterator<Item = String>,
) -> Result<String> {
    match inline {
        Some(value) => Ok(value),
        None => args
            .next()
            .ok_or_else(|| anyhow!("missing value for {flag}")),
    }
}

/// Watch the clipboard and keep a remote stream alive per host.
fn run(opts: Options) -> Result<()> {
    if opts.ssh_host.is_none() && cfg!(windows) {
        bail!(
            "automatic SSH host discovery is not available on Windows; pass --ssh-host <alias>"
        );
    }

    match opts.ssh_host.as_deref() {
        Some(host) => eprintln!(
            "smpaste: polling clipboard every {} ms; forwarding to `ssh -T {host} {} clipboard-image --stream`",
            opts.interval.as_millis(),
            opts.remote_sm
        ),
        None => eprintln!(
            "smpaste: polling clipboard every {} ms; discovering SSH hosts every {} ms",
            opts.interval.as_millis(),
            opts.scan.as_millis()
        ),
    }

    let mut clipboard = Clipboard::new().context("failed to open the system clipboard")?;
    let mut watcher = ClipboardWatcher::default();
    let mut last_hash: Option<u64> = None;
    let mut last_bmp: Option<Vec<u8>> = None;
    let mut last_sequence = platform::clipboard_sequence();

    // Seed the pipeline with whatever is already on the clipboard.
    if let Some(image) = watcher.poll(&mut clipboard) {
        let hash = hash_image(image.width, image.height, image.bytes.as_ref());
        match encode_bmp(image.width, image.height, image.bytes.as_ref()) {
            Ok(bmp) => {
                eprintln!(
                    "smpaste: initial clipboard image {}x{} ({} BMP bytes)",
                    image.width,
                    image.height,
                    bmp.len()
                );
                last_hash = Some(hash);
                last_bmp = Some(bmp);
            }
            Err(err) => {
                eprintln!(
                    "smpaste: ignoring initial clipboard image ({}x{}): {err:#}",
                    image.width, image.height
                );
                // Remember the hash so the same unsupported image is not
                // reported again on every poll.
                last_hash = Some(hash);
            }
        }
    }

    let mut streams: Vec<Stream> = Vec::new();
    if let Some(host) = opts.ssh_host.as_deref() {
        streams.push(Stream::new(
            SshTarget {
                destination: host.to_string(),
                carried: Vec::new(),
            },
            true,
            Instant::now(),
        ));
    }
    let mut next_scan = Instant::now();
    let mut last_scan_error: Option<String> = None;

    loop {
        let now = Instant::now();

        // Discover interactive SSH sessions. A failed scan keeps the existing
        // streams alive and only reports a changed error message.
        if opts.ssh_host.is_none() && now >= next_scan {
            match discover::scan() {
                Ok(targets) => {
                    sync_streams(&mut streams, &targets);
                    last_scan_error = None;
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    if last_scan_error.as_deref() != Some(message.as_str()) {
                        eprintln!("smpaste: SSH host scan failed: {message}");
                        last_scan_error = Some(message);
                    }
                }
            }
            next_scan = now + opts.scan;
        }

        // This runs even on iterations where the clipboard poll returns early.
        for stream in &mut streams {
            maintain_stream(stream, &opts.remote_sm, last_bmp.as_deref());
        }

        thread::sleep(opts.interval);

        let sequence = platform::clipboard_sequence();
        if sequence.is_some() && sequence == last_sequence {
            // Nothing was copied since the last poll; skip reading the data.
            continue;
        }
        last_sequence = sequence;

        let Some(image) = watcher.poll(&mut clipboard) else {
            continue;
        };

        let hash = hash_image(image.width, image.height, image.bytes.as_ref());
        if last_hash == Some(hash) {
            continue;
        }
        last_hash = Some(hash);

        let bmp = match encode_bmp(image.width, image.height, image.bytes.as_ref()) {
            Ok(bmp) => bmp,
            Err(err) => {
                eprintln!(
                    "smpaste: skipping clipboard image ({}x{}): {err:#}",
                    image.width, image.height
                );
                continue;
            }
        };
        eprintln!(
            "smpaste: new clipboard image {}x{} ({} BMP bytes)",
            image.width,
            image.height,
            bmp.len()
        );
        last_bmp = Some(bmp);

        let Some(bmp) = last_bmp.as_deref() else {
            continue;
        };
        for stream in &mut streams {
            // A failure only retires this stream; the others keep their
            // connections.
            let result = match stream.connection.as_mut() {
                Some(connection) => connection.send(bmp),
                None => continue,
            };
            match result {
                Ok(bytes) => eprintln!(
                    "smpaste[{}]: sent {bytes} frame bytes",
                    stream.target.label()
                ),
                Err(err) => {
                    eprintln!(
                        "smpaste[{}]: send failed: {err:#}; reconnecting in {} s",
                        stream.target.label(),
                        stream.retry_delay.as_secs()
                    );
                    stream.connection = None;
                    restart_timer(stream);
                }
            }
        }
    }
}

/// One SSH host this process forwards clipboard images to.
struct Stream {
    target: SshTarget,
    connection: Option<Connection>,
    next_retry: Instant,
    retry_delay: Duration,
    /// A fixed stream comes from `--ssh-host`: it never scans or backs off.
    fixed: bool,
}

impl Stream {
    fn new(target: SshTarget, fixed: bool, first_attempt: Instant) -> Self {
        Self {
            target,
            connection: None,
            next_retry: first_attempt,
            retry_delay: RECONNECT_DELAY,
            fixed,
        }
    }
}

/// Reconcile the discovered streams with the latest scan result.
fn sync_streams(streams: &mut Vec<Stream>, targets: &[SshTarget]) {
    let mut kept = Vec::with_capacity(streams.len());
    for stream in streams.drain(..) {
        let still_present = stream.fixed
            || targets
                .iter()
                .any(|target| target.key() == stream.target.key());
        if still_present {
            kept.push(stream);
        } else {
            eprintln!(
                "smpaste[{}]: SSH host disappeared; closing stream",
                stream.target.label()
            );
            // Dropping the stream closes stdin and kills the ssh process.
        }
    }
    *streams = kept;

    for target in targets {
        let key = target.key();
        if streams.iter().any(|stream| stream.target.key() == key) {
            continue;
        }
        eprintln!("smpaste[{}]: discovered SSH host", target.label());
        streams.push(Stream::new(target.clone(), false, Instant::now()));
    }
}

/// Connect a stream when its retry timer is due and drop connections whose
/// process exited.
fn maintain_stream(stream: &mut Stream, remote_sm: &str, last_bmp: Option<&[u8]>) {
    if stream.connection.is_some() {
        let exit = stream
            .connection
            .as_mut()
            .expect("connection checked above")
            .try_wait();
        match exit {
            Ok(None) => {}
            Ok(Some(status)) => {
                eprintln!(
                    "smpaste[{}]: ssh exited ({status})",
                    stream.target.label()
                );
                stream.connection = None;
                restart_timer(stream);
            }
            Err(err) => {
                eprintln!(
                    "smpaste[{}]: could not query ssh process: {err}",
                    stream.target.label()
                );
                stream.connection = None;
                restart_timer(stream);
            }
        }
        return;
    }

    if Instant::now() < stream.next_retry {
        return;
    }

    match Connection::connect(&stream.target, remote_sm) {
        Ok(mut connection) => {
            eprintln!("smpaste[{}]: ssh stream started", stream.target.label());
            let replay_ok = match last_bmp {
                Some(bmp) => match connection.send(bmp) {
                    Ok(bytes) => {
                        eprintln!(
                            "smpaste[{}]: replayed last image ({bytes} frame bytes)",
                            stream.target.label()
                        );
                        true
                    }
                    Err(err) => {
                        eprintln!(
                            "smpaste[{}]: replaying last image failed: {err:#}",
                            stream.target.label()
                        );
                        false
                    }
                },
                None => true,
            };
            if replay_ok {
                stream.connection = Some(connection);
                stream.retry_delay = RECONNECT_DELAY;
            } else {
                restart_timer(stream);
            }
        }
        Err(err) => {
            eprintln!(
                "smpaste[{}]: could not start ssh: {err:#}",
                stream.target.label()
            );
            restart_timer(stream);
        }
    }
}

/// Schedule the next attempt and grow the backoff for discovered streams.
fn restart_timer(stream: &mut Stream) {
    stream.next_retry = Instant::now() + stream.retry_delay;
    if !stream.fixed {
        stream.retry_delay = (stream.retry_delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Polls the clipboard and reports unusual errors only when they change.
#[derive(Default)]
struct ClipboardWatcher {
    last_error: Option<String>,
}

impl ClipboardWatcher {
    fn poll(&mut self, clipboard: &mut Clipboard) -> Option<ImageData<'static>> {
        match clipboard.get_image() {
            Ok(image) => {
                self.last_error = None;
                Some(image)
            }
            Err(ClipboardError::ContentNotAvailable) => {
                // No image on the clipboard: normal state while polling.
                self.last_error = None;
                None
            }
            Err(err) => {
                let message = err.to_string();
                if self.last_error.as_deref() != Some(message.as_str()) {
                    eprintln!("smpaste: clipboard read failed: {message}");
                    self.last_error = Some(message);
                }
                None
            }
        }
    }
}

/// A persistent `ssh ... <remote-sm> clipboard-image --stream` process.
struct Connection {
    child: Child,
    stdin: ChildStdin,
}

impl Connection {
    fn connect(target: &SshTarget, remote_sm: &str) -> Result<Self> {
        // BatchMode keeps a missing key or passphrase from turning into an
        // interactive prompt: stdin is the image pipe, so a prompt would
        // consume the stream and hang. Compression matters because a raw BMP
        // is large but compresses very well. Options inherited from the
        // original session were filtered by `discover`.
        let args = discover::ssh_command_args(target, remote_sm);
        let mut child = Command::new("ssh")
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn ssh for host {:?}", target.label()))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("ssh stdin was not captured"))?;

        Ok(Self { child, stdin })
    }

    /// Send one BMP image and return the number of frame bytes written.
    fn send(&mut self, bmp: &[u8]) -> Result<usize> {
        let frame = encode_frame(bmp)?;
        self.stdin
            .write_all(&frame)
            .context("failed to write frame to ssh stdin")?;
        self.stdin.flush().context("failed to flush ssh stdin")?;
        Ok(frame.len())
    }

    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // Closing stdin lets ssh finish; kill is a fallback for a stuck child.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Encode RGBA pixels as a 32-bit, bottom-up, BGRA BMP.
///
/// The remote side strips the 14-byte file header and uses the remaining
/// BITMAPINFOHEADER + pixels as a CF_DIB payload.
fn encode_bmp(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>> {
    if width == 0 || height == 0 {
        bail!("image dimensions must be non-zero (got {width}x{height})");
    }

    let pixel_len = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| anyhow!("image dimensions {width}x{height} overflow"))?;
    let file_len = BMP_PIXEL_OFFSET
        .checked_add(pixel_len)
        .ok_or_else(|| anyhow!("image dimensions {width}x{height} overflow"))?;

    if file_len > MAX_FRAME_BYTES {
        bail!(
            "image {width}x{height} needs {file_len} BMP bytes, which exceeds the {} MiB frame limit",
            MAX_FRAME_BYTES / (1024 * 1024)
        );
    }

    if rgba.len() != pixel_len {
        bail!(
            "expected {pixel_len} RGBA bytes for {width}x{height}, got {}",
            rgba.len()
        );
    }

    let width_i32 = i32::try_from(width)
        .with_context(|| format!("image width {width} does not fit in a BMP header"))?;
    let height_i32 = i32::try_from(height)
        .with_context(|| format!("image height {height} does not fit in a BMP header"))?;
    // Both values are below MAX_FRAME_BYTES at this point, so these cannot fail.
    let file_len_u32 = u32::try_from(file_len).context("BMP file size does not fit in u32")?;
    let pixel_len_u32 = u32::try_from(pixel_len).context("pixel data size does not fit in u32")?;

    let mut bmp = vec![0u8; file_len];

    // BITMAPFILEHEADER.
    bmp[0..2].copy_from_slice(b"BM");
    bmp[2..6].copy_from_slice(&file_len_u32.to_le_bytes());
    // bfReserved1 and bfReserved2 stay zero.
    bmp[10..14].copy_from_slice(&(BMP_PIXEL_OFFSET as u32).to_le_bytes());

    // BITMAPINFOHEADER. A positive height means bottom-up rows.
    bmp[14..18].copy_from_slice(&40u32.to_le_bytes());
    bmp[18..22].copy_from_slice(&width_i32.to_le_bytes());
    bmp[22..26].copy_from_slice(&height_i32.to_le_bytes());
    bmp[26..28].copy_from_slice(&1u16.to_le_bytes());
    bmp[28..30].copy_from_slice(&32u16.to_le_bytes());
    bmp[30..34].copy_from_slice(&0u32.to_le_bytes());
    bmp[34..38].copy_from_slice(&pixel_len_u32.to_le_bytes());
    bmp[38..42].copy_from_slice(&PIXELS_PER_METER.to_le_bytes());
    bmp[42..46].copy_from_slice(&PIXELS_PER_METER.to_le_bytes());
    // biClrUsed and biClrImportant stay zero.

    let row_bytes = width
        .checked_mul(4)
        .ok_or_else(|| anyhow!("image width {width} overflows the row size"))?;
    let mut dst = BMP_PIXEL_OFFSET;
    for y in (0..height).rev() {
        let src_row = &rgba[y * row_bytes..(y + 1) * row_bytes];
        for pixel in src_row.as_chunks::<4>().0 {
            // RGBA -> BGRA.
            bmp[dst] = pixel[2];
            bmp[dst + 1] = pixel[1];
            bmp[dst + 2] = pixel[0];
            bmp[dst + 3] = pixel[3];
            dst += 4;
        }
    }
    debug_assert_eq!(dst, file_len);

    Ok(bmp)
}

/// Prefix a BMP payload with its 4-byte big-endian length.
fn encode_frame(bmp: &[u8]) -> Result<Vec<u8>> {
    let len = u32::try_from(bmp.len()).context("BMP payload does not fit in a frame length")?;
    let mut frame = Vec::with_capacity(4 + bmp.len());
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(bmp);
    Ok(frame)
}

/// FNV-1a over the dimensions and RGBA bytes.
fn hash_image(width: usize, height: usize, rgba: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    let mut update = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(FNV_PRIME);
        }
    };
    update(&width.to_le_bytes());
    update(&height.to_le_bytes());
    update(rgba);
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_le(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }

    fn u32_le(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn i32_le(bytes: &[u8], offset: usize) -> i32 {
        i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    fn parse(args: &[&str]) -> Result<Cli> {
        parse_args(args.iter().map(|arg| arg.to_string()))
    }

    #[test]
    fn ssh_host_is_optional() {
        let Cli::Run(opts) = parse(&[]).unwrap() else {
            panic!("expected run options");
        };
        assert_eq!(opts.ssh_host, None);
        assert_eq!(opts.remote_sm, "sm");
        assert_eq!(opts.interval, Duration::from_millis(200));
        assert_eq!(opts.scan, Duration::from_millis(5000));
    }

    #[test]
    fn scan_ms_defaults_and_validates() {
        let Cli::Run(opts) = parse(&["--scan-ms", "250"]).unwrap() else {
            panic!("expected run options");
        };
        assert_eq!(opts.scan, Duration::from_millis(250));

        let err = parse(&["--scan-ms", "0"]).unwrap_err();
        assert!(err.to_string().contains("--scan-ms"), "{err:#}");
        assert!(parse(&["--scan-ms", "nope"]).is_err());
    }

    #[test]
    fn explicit_host_is_used_instead_of_discovery() {
        // Discovery is gated on `ssh_host.is_none()` in `run`, so a fixed host
        // is all the parse-level assertion that is needed here.
        let Cli::Run(opts) = parse(&["--ssh-host", "my-server", "--scan-ms", "1234"]).unwrap()
        else {
            panic!("expected run options");
        };
        assert_eq!(opts.ssh_host.as_deref(), Some("my-server"));
        assert_eq!(opts.scan, Duration::from_millis(1234));
    }

    #[test]
    fn rejects_empty_ssh_host() {
        assert!(parse(&["--ssh-host", ""]).is_err());
    }

    #[test]
    fn help_is_recognised() {
        assert!(matches!(parse(&["-h"]).unwrap(), Cli::Help));
        assert!(matches!(parse(&["--help"]).unwrap(), Cli::Help));
    }

    #[test]
    fn encodes_1x1_bmp_with_expected_headers() {
        let rgba = [10u8, 20, 30, 40];
        let bmp = encode_bmp(1, 1, &rgba).unwrap();

        assert_eq!(bmp.len(), 58);
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(u32_le(&bmp, 2), 58);
        assert_eq!(u16_le(&bmp, 6), 0);
        assert_eq!(u16_le(&bmp, 8), 0);
        assert_eq!(u32_le(&bmp, 10), 54);

        assert_eq!(u32_le(&bmp, 14), 40);
        assert_eq!(i32_le(&bmp, 18), 1);
        assert_eq!(i32_le(&bmp, 22), 1);
        assert_eq!(u16_le(&bmp, 26), 1);
        assert_eq!(u16_le(&bmp, 28), 32);
        assert_eq!(u32_le(&bmp, 30), 0);
        assert_eq!(u32_le(&bmp, 34), 4);
        assert_eq!(i32_le(&bmp, 38), 2835);
        assert_eq!(i32_le(&bmp, 42), 2835);
        assert_eq!(u32_le(&bmp, 46), 0);
        assert_eq!(u32_le(&bmp, 50), 0);

        // RGBA -> BGRA and bottom-up (trivial for one pixel).
        assert_eq!(&bmp[54..], &[30u8, 20, 10, 40][..]);
    }

    #[test]
    fn encodes_2x2_bmp_bottom_up_and_bgra() {
        let rgba = [
            255, 0, 0, 255, // top-left: red
            0, 255, 0, 255, // top-right: green
            0, 0, 255, 255, // bottom-left: blue
            255, 255, 255, 255, // bottom-right: white
        ];
        let bmp = encode_bmp(2, 2, &rgba).unwrap();

        assert_eq!(bmp.len(), 54 + 16);
        assert_eq!(u32_le(&bmp, 2), 70);
        assert_eq!(i32_le(&bmp, 18), 2);
        assert_eq!(i32_le(&bmp, 22), 2);
        assert_eq!(u32_le(&bmp, 34), 16);

        // BMP rows run bottom-to-top; each pixel is BGRA.
        assert_eq!(
            &bmp[54..],
            &[
                255, 0, 0, 255, // blue
                255, 255, 255, 255, // white
                0, 0, 255, 255, // red
                0, 255, 0, 255, // green
            ][..]
        );
    }

    #[test]
    fn frame_has_big_endian_length_prefix() {
        let bmp = encode_bmp(1, 1, &[1, 2, 3, 4]).unwrap();
        let frame = encode_frame(&bmp).unwrap();

        assert_eq!(frame.len(), bmp.len() + 4);
        assert_eq!(&frame[..4], &(bmp.len() as u32).to_be_bytes());
        assert_eq!(&frame[4..], &bmp[..]);
    }

    #[test]
    fn rejects_zero_dimensions() {
        assert!(encode_bmp(0, 1, &[]).is_err());
        assert!(encode_bmp(1, 0, &[]).is_err());
    }

    #[test]
    fn rejects_pixel_buffer_with_wrong_length() {
        let err = encode_bmp(2, 2, &[0u8; 15]).unwrap_err();
        assert!(err.to_string().contains("RGBA bytes"), "{err:#}");
    }

    #[test]
    fn rejects_oversized_images() {
        let err = encode_bmp(10_000, 10_000, &[]).unwrap_err();
        assert!(err.to_string().contains("frame limit"), "{err:#}");
    }

    #[test]
    fn rejects_dimension_overflow() {
        let err = encode_bmp(usize::MAX, 2, &[]).unwrap_err();
        assert!(err.to_string().contains("overflow"), "{err:#}");
    }

    #[test]
    fn hash_tracks_pixels_and_dimensions() {
        let base = hash_image(1, 1, &[1, 2, 3, 4]);
        assert_eq!(base, hash_image(1, 1, &[1, 2, 3, 4]));
        assert_ne!(base, hash_image(1, 1, &[1, 2, 3, 5]));
        assert_ne!(base, hash_image(2, 1, &[1, 2, 3, 4]));
    }
}
