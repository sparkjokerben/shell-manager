//! smpaste: forward clipboard images to a shell-manager host over SSH.
//!
//! The process polls the local clipboard, encodes every new image as a
//! 32-bit bottom-up BMP, and writes it to a persistent remote
//! `sm clipboard-image --stream` process using a simple length-prefixed
//! framing (4-byte big-endian length followed by the BMP bytes).

use std::io::Write;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use arboard::{Clipboard, Error as ClipboardError, ImageData};

/// BMP file header (14 bytes) plus BITMAPINFOHEADER (40 bytes).
const BMP_PIXEL_OFFSET: usize = 14 + 40;
/// Upper bound for one BMP frame, matching the host-side check.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Delay before trying to start a new SSH stream after a failure.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);
/// Default clipboard poll interval.
const DEFAULT_INTERVAL_MS: u64 = 500;
const DEFAULT_REMOTE_SM: &str = "sm";
/// Pixels per meter resolution stored in the BMP info header (72 DPI).
const PIXELS_PER_METER: i32 = 2835;

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

const USAGE: &str = concat!(
    "smpaste ",
    env!("CARGO_PKG_VERSION"),
    "\n",
    "Forward clipboard images to a shell-manager host over SSH.\n",
    "\n",
    "Usage:\n",
    "  smpaste --ssh-host <alias> [--remote-sm <command>] [--interval-ms <ms>]\n",
    "\n",
    "Options:\n",
    "  --ssh-host <alias>     SSH host alias or user@host (required)\n",
    "  --remote-sm <command>  Remote shell-manager command (default: sm)\n",
    "  --interval-ms <ms>     Clipboard poll interval in milliseconds (default: 500)\n",
    "  -h, --help             Print this help and exit\n",
    "\n",
    "The process keeps one SSH connection open and sends one length-prefixed\n",
    "BMP frame per clipboard image. It reconnects automatically after failures.\n",
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
    ssh_host: String,
    remote_sm: String,
    interval: Duration,
}

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
            _ => bail!("unknown argument {arg:?}\n\n{USAGE}"),
        }
    }

    let ssh_host =
        ssh_host.ok_or_else(|| anyhow!("missing required --ssh-host <alias>\n\n{USAGE}"))?;
    if ssh_host.is_empty() {
        bail!("--ssh-host must not be empty");
    }

    let remote_sm = remote_sm.unwrap_or_else(|| DEFAULT_REMOTE_SM.to_string());
    if remote_sm.is_empty() {
        bail!("--remote-sm must not be empty");
    }

    Ok(Cli::Run(Options {
        ssh_host,
        remote_sm,
        interval: Duration::from_millis(interval_ms),
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

/// Watch the clipboard and keep a remote stream alive.
fn run(opts: Options) -> Result<()> {
    eprintln!(
        "smpaste: polling clipboard every {} ms; forwarding to `ssh -T {} {} clipboard-image --stream`",
        opts.interval.as_millis(),
        opts.ssh_host,
        opts.remote_sm
    );

    let mut clipboard = Clipboard::new().context("failed to open the system clipboard")?;
    let mut watcher = ClipboardWatcher::default();
    let mut last_hash: Option<u64> = None;
    let mut last_bmp: Option<Vec<u8>> = None;

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

    let mut connection: Option<Connection> = None;

    loop {
        // Establish a new stream if we do not have one. After a reconnect the
        // last successfully encoded image is replayed once.
        if connection.is_none() {
            match Connection::connect(&opts) {
                Ok(mut conn) => {
                    eprintln!("smpaste: ssh stream started");
                    let replay_ok = match last_bmp.as_deref() {
                        Some(bmp) => match conn.send(bmp) {
                            Ok(bytes) => {
                                eprintln!("smpaste: replayed last image ({bytes} frame bytes)");
                                true
                            }
                            Err(err) => {
                                eprintln!("smpaste: replaying last image failed: {err:#}");
                                false
                            }
                        },
                        None => true,
                    };
                    if replay_ok {
                        connection = Some(conn);
                    }
                }
                Err(err) => {
                    eprintln!("smpaste: could not start ssh: {err:#}");
                }
            }

            if connection.is_none() {
                eprintln!("smpaste: retrying in {} s", RECONNECT_DELAY.as_secs());
                thread::sleep(RECONNECT_DELAY);
                continue;
            }
        }

        thread::sleep(opts.interval);

        // A remote command can exit without the next write failing, so check
        // the child explicitly on every interval.
        let exited = match connection.as_mut() {
            Some(conn) => match conn.try_wait() {
                Ok(status) => status,
                Err(err) => {
                    eprintln!("smpaste: could not query ssh process: {err}");
                    None
                }
            },
            None => None,
        };
        if let Some(status) = exited {
            eprintln!(
                "smpaste: ssh exited ({status}); reconnecting in {} s",
                RECONNECT_DELAY.as_secs()
            );
            // Dropping the connection closes stdin and reaps the child.
            connection = None;
            thread::sleep(RECONNECT_DELAY);
            continue;
        }

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

        // Scope the mutable borrow of `connection` so we can clear it below.
        let send_result = match (connection.as_mut(), last_bmp.as_deref()) {
            (Some(conn), Some(bmp)) => Some(conn.send(bmp)),
            _ => None,
        };
        match send_result {
            Some(Ok(bytes)) => eprintln!("smpaste: sent {bytes} frame bytes"),
            Some(Err(err)) => {
                eprintln!(
                    "smpaste: send failed: {err:#}; reconnecting in {} s",
                    RECONNECT_DELAY.as_secs()
                );
                connection = None;
                thread::sleep(RECONNECT_DELAY);
                // The next iteration reconnects and replays `last_bmp`.
            }
            None => {}
        }
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

/// A persistent `ssh -T <host> <remote-sm> clipboard-image --stream` process.
struct Connection {
    child: Child,
    stdin: ChildStdin,
}

impl Connection {
    fn connect(opts: &Options) -> Result<Self> {
        // BatchMode keeps a missing key or passphrase from turning into an
        // interactive prompt: stdin is the image pipe, so a prompt would
        // consume the stream and hang. Compression matters because a raw BMP
        // is large but compresses very well.
        let mut child = Command::new("ssh")
            .arg("-T")
            .arg("-C")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg(&opts.ssh_host)
            .arg(&opts.remote_sm)
            .arg("clipboard-image")
            .arg("--stream")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn ssh for host {:?}", opts.ssh_host))?;

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
