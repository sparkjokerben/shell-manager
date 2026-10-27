//! Windows clipboard support for image paste.
//!
//! The client's paste watcher stages the latest screenshot as a 32-bit BMP in
//! `%TEMP%\shell-manager-paste\latest.bmp`. Claude Code reads the clipboard
//! through `CF_DIB`, so what has to be published is the DIB payload: the BMP
//! file header (14 bytes) is stripped and the rest is handed to Win32 as a
//! movable global block owned by the clipboard.

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// Clipboard format `CF_DIB` (8).
///
/// `windows` 0.52 defines the typed constant in its Ole module only; pulling
/// that feature in for a single number is not worth it, so the documented
/// value is spelled out here.
const CF_DIB: u32 = 8;

/// Size of a BMP `BITMAPFILEHEADER`.
const BMP_FILE_HEADER_LEN: usize = 14;

/// Size of a `BITMAPINFOHEADER`.
const BITMAPINFOHEADER_LEN: usize = 40;

/// Largest accepted width/height, in pixels. Keeps the size arithmetic and the
/// global allocation from being driven by a bogus header.
const MAX_DIMENSION: i32 = 32768;

/// Where the client stages the most recent image.
pub fn stage_path() -> PathBuf {
    std::env::temp_dir()
        .join("shell-manager-paste")
        .join("latest.bmp")
}

/// Publish the staged image on the clipboard as `CF_DIB`.
///
/// The caller (the daemon) then injects the paste key into the session. A
/// short settle pause is included because the target application reads the
/// clipboard right after that key arrives.
pub fn install_latest_image() -> Result<()> {
    let path = stage_path();
    let file = std::fs::read(&path)
        .with_context(|| format!("failed to read staged image {}", path.display()))?;
    let dib = dib_from_bmp(&file).context("staged image is not a usable BMP")?;
    set_clipboard_dib(dib)?;
    // Give clipboard listeners a moment to pick the data up before the paste
    // key is injected downstream.
    std::thread::sleep(Duration::from_millis(30));
    Ok(())
}

/// Validate a 32-bit BMP and return its DIB payload (header + pixels).
///
/// Only the shape the paste watcher produces is accepted: a `BITMAPFILEHEADER`
/// immediately followed by a `BITMAPINFOHEADER`, `BI_RGB`, 32 bpp. Anything
/// else is rejected rather than copied into the clipboard, where it could make
/// the paste target misbehave.
pub fn dib_from_bmp(file: &[u8]) -> Result<&[u8]> {
    if file.len() < BMP_FILE_HEADER_LEN + BITMAPINFOHEADER_LEN {
        bail!(
            "BMP too short: {} bytes (need at least {})",
            file.len(),
            BMP_FILE_HEADER_LEN + BITMAPINFOHEADER_LEN
        );
    }
    if &file[0..2] != b"BM" {
        bail!("not a BMP: bad signature");
    }
    let pixel_offset = le_u32(file, 10) as usize;
    // The file offset of the pixels is the 14-byte file header plus the DIB
    // header we accept. (The DIB payload itself starts at byte 14; its own
    // header occupies the first 40 bytes of that payload.)
    let expected_pixel_offset = BMP_FILE_HEADER_LEN + BITMAPINFOHEADER_LEN;
    if pixel_offset != expected_pixel_offset {
        bail!(
            "unsupported BMP pixel offset {} (expected {})",
            pixel_offset,
            expected_pixel_offset
        );
    }

    let dib = &file[BMP_FILE_HEADER_LEN..];
    let header_size = le_u32(dib, 0);
    if header_size != BITMAPINFOHEADER_LEN as u32 {
        bail!(
            "unsupported DIB header size {} (expected {})",
            header_size,
            BITMAPINFOHEADER_LEN
        );
    }
    let width = le_i32(dib, 4);
    let height = le_i32(dib, 8);
    let planes = le_u16(dib, 12);
    let bit_count = le_u16(dib, 14);
    let compression = le_u32(dib, 16);

    if width <= 0 || width > MAX_DIMENSION {
        bail!("unreasonable BMP width {}", width);
    }
    if height == 0 || height.unsigned_abs() > MAX_DIMENSION as u32 {
        bail!("unreasonable BMP height {}", height);
    }
    if planes != 1 {
        bail!("unsupported BMP plane count {}", planes);
    }
    if bit_count != 32 {
        bail!("unsupported BMP bit depth {}", bit_count);
    }
    if compression != 0 {
        bail!("unsupported BMP compression {}", compression);
    }

    // 32-bpp rows are already DWORD aligned, so the pixel payload is exactly
    // width * 4 * |height| bytes. `biSizeImage` may legally be zero for BI_RGB
    // and is not needed here.
    let pixels = width as u64 * 4 * height.unsigned_abs() as u64;
    let expected = BITMAPINFOHEADER_LEN as u64 + pixels;
    if (dib.len() as u64) < expected {
        bail!(
            "truncated BMP: {} DIB bytes (expected at least {})",
            dib.len(),
            expected
        );
    }
    Ok(dib)
}

fn le_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn le_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

fn le_i32(data: &[u8], at: usize) -> i32 {
    i32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Closes the clipboard on every exit path once it has been opened.
struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

/// Copy `dib` into a movable global block and hand it to the clipboard.
fn set_clipboard_dib(dib: &[u8]) -> Result<()> {
    // The clipboard is a shared resource: another process may hold it for a
    // few milliseconds, so retry briefly instead of failing the paste.
    let mut last_err = None;
    let mut opened = false;
    for _ in 0..20 {
        match unsafe { OpenClipboard(None::<&windows::Win32::Foundation::HWND>) } {
            Ok(()) => {
                opened = true;
                break;
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
    if !opened {
        return Err(anyhow::anyhow!(
            "failed to open clipboard: {}",
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".to_string())
        ));
    }
    let _guard = ClipboardGuard;

    unsafe {
        EmptyClipboard().context("EmptyClipboard failed")?;

        let block = GlobalAlloc(GMEM_MOVEABLE, dib.len()).context("GlobalAlloc failed")?;
        let ptr = GlobalLock(block);
        if ptr.is_null() {
            free_block(block);
            bail!("GlobalLock failed");
        }
        std::ptr::copy_nonoverlapping(dib.as_ptr(), ptr.cast::<u8>(), dib.len());
        // GlobalUnlock reports "lock count reached zero" as an error with
        // ERROR_SUCCESS; the copy itself is validated by SetClipboardData.
        let _ = GlobalUnlock(block);

        // `SetClipboardData` takes ownership of the block only on success, so
        // free it manually when the call fails.
        let handle = HANDLE(block.0 as isize);
        if let Err(e) = SetClipboardData(CF_DIB, handle) {
            free_block(block);
            return Err(e).context("SetClipboardData failed");
        }
    }

    Ok(())
}

/// Best-effort free of a block the clipboard did not take ownership of.
fn free_block(block: HGLOBAL) {
    unsafe {
        // GlobalFree returns a null HGLOBAL on success, which the generated
        // wrapper reports as an error; the result is irrelevant here.
        let _ = GlobalFree(block);
    }
}

#[cfg(test)]
mod tests {
    use super::dib_from_bmp;
    use super::{BITMAPINFOHEADER_LEN, BMP_FILE_HEADER_LEN};

    /// Build a minimal, valid 32-bpp BMP of `width` x `height` pixels.
    fn tiny_bmp(width: i32, height: i32) -> Vec<u8> {
        let pixels = width.unsigned_abs() as usize * 4 * height.unsigned_abs() as usize;
        let file_size = BMP_FILE_HEADER_LEN + BITMAPINFOHEADER_LEN + pixels;
        let mut bmp = Vec::with_capacity(file_size);

        // BITMAPFILEHEADER
        bmp.extend_from_slice(b"BM");
        bmp.extend_from_slice(&(file_size as u32).to_le_bytes());
        bmp.extend_from_slice(&0u16.to_le_bytes()); // reserved 1
        bmp.extend_from_slice(&0u16.to_le_bytes()); // reserved 2
        bmp.extend_from_slice(&((BMP_FILE_HEADER_LEN + BITMAPINFOHEADER_LEN) as u32).to_le_bytes());

        // BITMAPINFOHEADER
        bmp.extend_from_slice(&(BITMAPINFOHEADER_LEN as u32).to_le_bytes());
        bmp.extend_from_slice(&width.to_le_bytes());
        bmp.extend_from_slice(&height.to_le_bytes());
        bmp.extend_from_slice(&1u16.to_le_bytes()); // planes
        bmp.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        bmp.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        bmp.extend_from_slice(&(pixels as u32).to_le_bytes());
        bmp.extend_from_slice(&0i32.to_le_bytes()); // x pixels per meter
        bmp.extend_from_slice(&0i32.to_le_bytes()); // y pixels per meter
        bmp.extend_from_slice(&0u32.to_le_bytes()); // colors used
        bmp.extend_from_slice(&0u32.to_le_bytes()); // colors important

        bmp.resize(file_size, 0x7f); // BGRA pixels
        bmp
    }

    #[test]
    fn valid_bmp_yields_the_dib_payload() {
        let bmp = tiny_bmp(2, 2);
        let dib = dib_from_bmp(&bmp).expect("valid 2x2 BMP");
        assert_eq!(dib.len(), bmp.len() - BMP_FILE_HEADER_LEN);
        assert_eq!(dib, &bmp[BMP_FILE_HEADER_LEN..]);
    }

    #[test]
    fn top_down_bmp_is_accepted() {
        // A negative height means the rows are stored top-down; both layouts
        // are legal DIBs.
        let bmp = tiny_bmp(3, -2);
        assert!(dib_from_bmp(&bmp).is_ok());
    }

    #[test]
    fn wrong_signature_is_rejected() {
        let mut bmp = tiny_bmp(2, 2);
        bmp[0..2].copy_from_slice(b"XX");
        assert!(dib_from_bmp(&bmp).is_err());
    }

    #[test]
    fn shortened_file_is_rejected() {
        let bmp = tiny_bmp(2, 2);
        assert!(dib_from_bmp(&bmp[..BMP_FILE_HEADER_LEN + 10]).is_err());
        assert!(dib_from_bmp(&[]).is_err());
    }

    #[test]
    fn non_standard_pixel_offset_is_rejected() {
        let mut bmp = tiny_bmp(2, 2);
        // 14 points at the DIB header instead of the pixel data.
        bmp[10..14].copy_from_slice(&(BMP_FILE_HEADER_LEN as u32).to_le_bytes());
        assert!(dib_from_bmp(&bmp).is_err());
    }

    #[test]
    fn wrong_dib_header_size_is_rejected() {
        let mut bmp = tiny_bmp(2, 2);
        bmp[BMP_FILE_HEADER_LEN..BMP_FILE_HEADER_LEN + 4].copy_from_slice(&108u32.to_le_bytes());
        assert!(dib_from_bmp(&bmp).is_err());
    }

    #[test]
    fn non_32bpp_is_rejected() {
        let mut bmp = tiny_bmp(2, 2);
        let bpp_at = BMP_FILE_HEADER_LEN + 14;
        bmp[bpp_at..bpp_at + 2].copy_from_slice(&24u16.to_le_bytes());
        assert!(dib_from_bmp(&bmp).is_err());
    }

    #[test]
    fn compressed_bmp_is_rejected() {
        let mut bmp = tiny_bmp(2, 2);
        let compression_at = BMP_FILE_HEADER_LEN + 16;
        bmp[compression_at..compression_at + 4].copy_from_slice(&3u32.to_le_bytes());
        assert!(dib_from_bmp(&bmp).is_err());
    }

    #[test]
    fn unreasonable_dimensions_are_rejected() {
        assert!(dib_from_bmp(&tiny_bmp(0, 2)).is_err());
        assert!(dib_from_bmp(&tiny_bmp(2, 0)).is_err());
        assert!(dib_from_bmp(&tiny_bmp(-2, 2)).is_err());
        assert!(dib_from_bmp(&tiny_bmp(2, super::MAX_DIMENSION + 1)).is_err());
    }

    #[test]
    fn truncated_pixel_data_is_rejected() {
        let bmp = tiny_bmp(4, 4);
        // Cut one pixel row: the header still fits, the payload does not.
        let cut = bmp.len() - 4 * 4;
        assert!(dib_from_bmp(&bmp[..cut]).is_err());
    }
}
