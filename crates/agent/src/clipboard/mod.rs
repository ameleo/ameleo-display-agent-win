// SPDX-License-Identifier: GPL-3.0-only

//! Windows clipboard formats and conversions.
//!
//! Ownership and delayed rendering live in `desktop.rs`. This module only
//! knows how to read the clipboard as MIME data and how to build clipboard
//! handles from MIME data.

use proto::mime;
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, GetDC, GetDIBits, GetObjectW,
    HBITMAP, ReleaseDC,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{CF_BITMAP, CF_DIB, CF_UNICODETEXT};

use crate::win::wide;

pub const CF_TEXT_W: u32 = CF_UNICODETEXT as u32;
pub const CF_DIB_U: u32 = CF_DIB as u32;
const CF_BITMAP_U: u32 = CF_BITMAP as u32;

/// The "PNG" registered format (Office, browsers, Paint...), keeps alpha.
pub fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(wide("PNG").as_ptr()) }
}

fn available(format: u32) -> bool {
    unsafe { IsClipboardFormatAvailable(format) != 0 }
}

/// MIME types the current guest clipboard content can be exported as.
pub fn available_mimes(png: u32) -> Vec<String> {
    let mut mimes = Vec::new();
    if available(CF_TEXT_W) {
        mimes.push(mime::TEXT.to_string());
    }
    if available(png) || available(CF_DIB_U) || available(CF_BITMAP_U) {
        mimes.push(mime::PNG.to_string());
        mimes.push(mime::BMP.to_string());
    }
    mimes
}

/// Windows clipboard formats to announce for a host grab.
pub fn formats_for(mimes: &[String], png: u32) -> Vec<u32> {
    let mut formats = Vec::new();
    if mimes.iter().any(|m| is_text(m)) {
        formats.push(CF_TEXT_W);
    }
    let has_png = mimes.iter().any(|m| m == mime::PNG);
    if has_png || mimes.iter().any(|m| m == mime::BMP) {
        formats.push(CF_DIB_U);
    }
    if has_png {
        formats.push(png);
    }
    formats
}

/// MIME types to ask the host for when an app pastes `format`.
pub fn mimes_for(format: u32, png: u32) -> Vec<String> {
    match format {
        CF_TEXT_W => vec![mime::TEXT.into()],
        CF_DIB_U => vec![mime::PNG.into(), mime::BMP.into()],
        f if f == png => vec![mime::PNG.into()],
        _ => vec![],
    }
}

/// The host types, in its own spelling and order, that can render `format`.
/// Image formats keep the guest preference (PNG first).
pub fn host_mimes_for(format: u32, png: u32, host: &[String]) -> Vec<String> {
    if format == CF_TEXT_W {
        return host.iter().filter(|m| is_text(m)).cloned().collect();
    }
    mimes_for(format, png)
        .into_iter()
        .filter(|m| host.contains(m))
        .collect()
}

fn is_text(m: &str) -> bool {
    m.eq_ignore_ascii_case(mime::TEXT) || m == "text/plain" || m == "UTF8_STRING"
}

/// Opens the clipboard, retrying while another app holds it.
pub struct OpenGuard;

impl OpenGuard {
    pub fn open(hwnd: HWND) -> Option<Self> {
        for _ in 0..10 {
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Some(Self);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }
}

impl Drop for OpenGuard {
    fn drop(&mut self) {
        unsafe { CloseClipboard() };
    }
}

/// What a read of the guest clipboard gives.
pub enum Content {
    Data(String, Vec<u8>),
}

/// Reads the clipboard in the first of `mimes` that is available.
pub fn read(hwnd: HWND, mimes: &[String], png: u32) -> Result<Content, String> {
    let _guard = OpenGuard::open(hwnd).ok_or("clipboard busy")?;
    for m in mimes {
        let res = if is_text(m) && available(CF_TEXT_W) {
            read_text().map(|d| (mime::TEXT.to_string(), d))
        } else if m == mime::PNG && available(png) {
            read_global(png).map(|d| (mime::PNG.to_string(), d))
        } else if m == mime::PNG && (available(CF_BITMAP_U) || available(CF_DIB_U)) {
            read_bitmap_rgba()
                .and_then(|(w, h, px)| encode_png(w, h, &px))
                .map(|d| (mime::PNG.to_string(), d))
        } else if m == mime::BMP && available(CF_DIB_U) {
            read_global(CF_DIB_U)
                .and_then(|dib| dib_to_bmp(&dib))
                .map(|d| (mime::BMP.to_string(), d))
        } else {
            continue;
        };
        match res {
            Ok((m, d)) => return Ok(Content::Data(m, d)),
            Err(e) => tracing::warn!("reading clipboard as {m}: {e}"),
        }
    }
    Err("no requested type available".into())
}

fn read_global(format: u32) -> Result<Vec<u8>, String> {
    let h = unsafe { GetClipboardData(format) };
    if h.is_null() {
        return Err("GetClipboardData failed".into());
    }
    unsafe {
        let size = GlobalSize(h);
        let p = GlobalLock(h) as *const u8;
        if p.is_null() {
            return Err("GlobalLock failed".into());
        }
        let v = std::slice::from_raw_parts(p, size).to_vec();
        GlobalUnlock(h);
        Ok(v)
    }
}

fn read_text() -> Result<Vec<u8>, String> {
    let raw = read_global(CF_TEXT_W)?;
    let units: Vec<u16> = raw
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let s = crate::win::from_wide(&units);
    Ok(s.replace("\r\n", "\n").into_bytes())
}

/// CF_BITMAP through GDI, so every source depth and palette is handled.
/// Returns top-down RGBA.
fn read_bitmap_rgba() -> Result<(u32, u32, Vec<u8>), String> {
    let hbm = unsafe { GetClipboardData(CF_BITMAP_U) } as HBITMAP;
    if hbm.is_null() {
        return Err("no CF_BITMAP".into());
    }
    let mut bm: BITMAP = unsafe { std::mem::zeroed() };
    if unsafe {
        GetObjectW(
            hbm,
            std::mem::size_of::<BITMAP>() as i32,
            (&mut bm as *mut BITMAP).cast(),
        )
    } == 0
    {
        return Err("GetObject failed".into());
    }
    let (w, h) = (bm.bmWidth.max(0) as u32, bm.bmHeight.unsigned_abs());
    if w == 0 || h == 0 {
        return Err("empty bitmap".into());
    }
    let mut bi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w as i32,
        biHeight: -(h as i32), // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..unsafe { std::mem::zeroed() }
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    let lines = unsafe {
        let dc = GetDC(std::ptr::null_mut());
        let n = GetDIBits(
            dc,
            hbm,
            0,
            h,
            px.as_mut_ptr().cast(),
            &mut bi,
            DIB_RGB_COLORS,
        );
        ReleaseDC(std::ptr::null_mut(), dc);
        n
    };
    if lines == 0 {
        return Err("GetDIBits failed".into());
    }
    let opaque = px.as_chunks::<4>().0.iter().all(|p| p[3] == 0);
    for p in px.as_chunks_mut::<4>().0 {
        p.swap(0, 2); // BGRA -> RGBA
        if opaque {
            p[3] = 255;
        }
    }
    Ok((w, h, px))
}

pub fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// Decodes a PNG to top-down RGBA.
pub fn decode_png(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(data);
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width, info.height);
    let n = (w * h) as usize;
    let rgba = match info.color_type {
        png::ColorType::Rgba => buf[..n * 4].to_vec(),
        png::ColorType::Rgb => buf[..n * 3]
            .as_chunks::<3>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf[..n * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf[..n].iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return Err("unexpanded palette".into()),
    };
    Ok((w, h, rgba))
}

/// Top-down RGBA to a CF_DIB (BITMAPINFOHEADER + bottom-up BGRA rows).
pub fn rgba_to_dib(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
    let stride = (w * 4) as usize;
    let mut dib = Vec::with_capacity(40 + stride * h as usize);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    dib.extend_from_slice(&((stride * h as usize) as u32).to_le_bytes());
    dib.extend_from_slice(&2835i32.to_le_bytes()); // 72 dpi
    dib.extend_from_slice(&2835i32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    for row in rgba.chunks_exact(stride).rev() {
        for p in row.as_chunks::<4>().0 {
            dib.extend_from_slice(&[p[2], p[1], p[0], p[3]]);
        }
    }
    dib
}

/// BMP file to CF_DIB: drop the 14 byte BITMAPFILEHEADER.
pub fn bmp_to_dib(bmp: &[u8]) -> Result<Vec<u8>, String> {
    if bmp.len() < 14 + 40 || &bmp[..2] != b"BM" {
        return Err("not a BMP file".into());
    }
    Ok(bmp[14..].to_vec())
}

/// CF_DIB to a BMP file.
fn dib_to_bmp(dib: &[u8]) -> Result<Vec<u8>, String> {
    if dib.len() < 40 {
        return Err("short DIB".into());
    }
    let u32_at = |o: usize| u32::from_le_bytes(dib[o..o + 4].try_into().unwrap());
    let header = u32_at(0) as usize;
    let bit_count = u16::from_le_bytes([dib[14], dib[15]]);
    let compression = u32_at(16);
    let clr_used = u32_at(32) as usize;
    let masks = if header == 40 && compression == 3 {
        12
    } else {
        0
    }; // BI_BITFIELDS
    let palette = match (clr_used, bit_count) {
        (0, 1 | 4 | 8) => (1usize << bit_count) * 4,
        (n, _) => n * 4,
    };
    let off_bits = 14 + header + masks + palette;
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&(off_bits as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    Ok(bmp)
}

/// Builds the clipboard handle for `format` from host data.
pub fn to_handle(format: u32, png: u32, mime_type: &str, data: &[u8]) -> Result<HANDLE, String> {
    let bytes: Vec<u8> = match format {
        CF_TEXT_W => {
            let text = String::from_utf8_lossy(data)
                .replace("\r\n", "\n")
                .replace('\n', "\r\n");
            wide(text).iter().flat_map(|u| u.to_le_bytes()).collect()
        }
        CF_DIB_U => match mime_type {
            mime::PNG => {
                let (w, h, rgba) = decode_png(data)?;
                rgba_to_dib(w, h, &rgba)
            }
            mime::BMP => bmp_to_dib(data)?,
            other => return Err(format!("cannot render CF_DIB from {other}")),
        },
        f if f == png && mime_type == mime::PNG => data.to_vec(),
        _ => return Err(format!("cannot render format {format} from {mime_type}")),
    };
    global_from(&bytes)
}

pub fn global_from(bytes: &[u8]) -> Result<HANDLE, String> {
    unsafe {
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if h.is_null() {
            return Err("GlobalAlloc failed".into());
        }
        let p = GlobalLock(h) as *mut u8;
        if p.is_null() {
            GlobalFree(h);
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_asks_with_the_host_spelling() {
        let png = 0xC100;
        let host = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // A grab with a text alias announces CF_UNICODETEXT...
        for alias in ["text/plain", "UTF8_STRING", "text/plain;charset=UTF-8"] {
            assert_eq!(formats_for(&host(&[alias]), png), [CF_TEXT_W]);
            // ...and the paste must find it again.
            assert_eq!(host_mimes_for(CF_TEXT_W, png, &host(&[alias])), [alias]);
        }
        assert_eq!(
            host_mimes_for(CF_DIB_U, png, &host(&["image/bmp", "image/png"])),
            ["image/png", "image/bmp"]
        );
        assert!(host_mimes_for(CF_TEXT_W, png, &host(&["image/png"])).is_empty());
    }

    #[test]
    fn png_dib_roundtrip() {
        let rgba: Vec<u8> = (0..2 * 3).flat_map(|i| [i as u8, 10, 20, 255]).collect();
        let png = encode_png(2, 3, &rgba).unwrap();
        let (w, h, back) = decode_png(&png).unwrap();
        assert_eq!((w, h), (2, 3));
        assert_eq!(back, rgba);
        let dib = rgba_to_dib(w, h, &back);
        assert_eq!(dib.len(), 40 + 2 * 3 * 4);
        // Last top-down row is the first DIB row, BGRA.
        assert_eq!(&dib[40..44], &[20, 10, 4, 255]);
        let bmp = dib_to_bmp(&dib).unwrap();
        assert_eq!(bmp_to_dib(&bmp).unwrap(), dib);
    }
}
