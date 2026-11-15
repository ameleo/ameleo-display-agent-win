// SPDX-License-Identifier: GPL-3.0-only

//! The host's text and images offered with its files: the Windows
//! clipboard formats they go in, and how to build them.

use proto::mime;
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};

use crate::win::wide;

const CF_TEXT_W: u32 = CF_UNICODETEXT as u32;
const CF_DIB_U: u32 = CF_DIB as u32;

/// The "PNG" registered format (Office, browsers, Paint...), keeps alpha.
pub fn png_format() -> u32 {
    unsafe { RegisterClipboardFormatW(wide("PNG").as_ptr()) }
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
fn mimes_for(format: u32, png: u32) -> Vec<String> {
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

/// Decodes a PNG to top-down RGBA.
fn decode_png(data: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = dec.read_info().map_err(|e| e.to_string())?;
    let size = reader
        .output_buffer_size()
        .ok_or("image too large for memory")?;
    let mut buf = vec![0; size];
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
fn rgba_to_dib(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
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
fn bmp_to_dib(bmp: &[u8]) -> Result<Vec<u8>, String> {
    if bmp.len() < 14 + 40 || &bmp[..2] != b"BM" {
        return Err("not a BMP file".into());
    }
    Ok(bmp[14..].to_vec())
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

    fn encode_png(w: u32, h: u32, rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().unwrap();
            writer.write_image_data(rgba).unwrap();
        }
        out
    }

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
    fn png_to_dib() {
        let rgba: Vec<u8> = (0..2 * 3).flat_map(|i| [i as u8, 10, 20, 255]).collect();
        let (w, h, back) = decode_png(&encode_png(2, 3, &rgba)).unwrap();
        assert_eq!((w, h), (2, 3));
        assert_eq!(back, rgba);
        let dib = rgba_to_dib(w, h, &back);
        assert_eq!(dib.len(), 40 + 2 * 3 * 4);
        // Last top-down row is the first DIB row, BGRA.
        assert_eq!(&dib[40..44], &[20, 10, 4, 255]);
        let mut bmp = b"BM".to_vec();
        bmp.extend_from_slice(&[0; 12]);
        bmp.extend_from_slice(&dib);
        assert_eq!(bmp_to_dib(&bmp).unwrap(), dib);
    }
}
