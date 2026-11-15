// SPDX-License-Identifier: GPL-3.0-only

//! H.264 for the virtual cameras: the Media Foundation decoder (client
//! frames to NV12) and encoder (test pattern to H.264, for `camera-test`
//! and the tests).
//!
//! Input is Annex B access units (start codes, SPS/PPS in band). The
//! decoder waits for an IDR before decoding and asks for a new one when it
//! loses track.

use std::mem::ManuallyDrop;

use anyhow::{Context, Result, bail};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows_core::GUID;

/// NAL unit types of an Annex B access unit.
pub fn nal_types(au: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 <= au.len() {
            let start = if au[i..].starts_with(&[0, 0, 1]) {
                Some(i + 3)
            } else if au[i..].starts_with(&[0, 0, 0, 1]) {
                Some(i + 4)
            } else {
                None
            };
            match start {
                Some(s) if s < au.len() => {
                    i = s + 1;
                    return Some(au[s] & 0x1F);
                }
                Some(_) => return None,
                None => i += 1,
            }
        }
        None
    })
}

/// An IDR access unit: the decoder can start from it.
pub fn is_key_frame(au: &[u8]) -> bool {
    nal_types(au).any(|t| t == 5)
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

fn unpack(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

fn sample_from(data: &[u8], time: i64) -> Result<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(data.len().max(1) as u32)?;
        let mut p = std::ptr::null_mut();
        buffer.Lock(&mut p, None, None)?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(data.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time)?;
        Ok(sample)
    }
}

/// Runs ProcessOutput once. `Ok(None)`: the transform needs more input.
fn process_output(mft: &IMFTransform) -> windows_core::Result<Option<IMFSample>> {
    unsafe {
        let info = mft.GetOutputStreamInfo(0)?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let sample = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            s.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1))?)?;
            Some(s)
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let res = mft.ProcessOutput(0, &mut out, &mut status);
        let [buf] = out;
        drop(ManuallyDrop::into_inner(buf.pEvents));
        let sample = ManuallyDrop::into_inner(buf.pSample);
        match res {
            Ok(()) => Ok(sample),
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Decoded picture geometry.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    /// Displayed size (aperture).
    width: u32,
    height: u32,
    /// Buffer layout: stride and allocated height (e.g. 1088 for 1080p).
    stride: u32,
    alloc_height: u32,
}

/// What [`Decoder::decode`] made of an access unit.
#[derive(Debug, PartialEq, Eq)]
pub enum Decoded {
    /// Decoded (zero or more pictures were emitted).
    Ok,
    /// Dropped: the decoder waits for an IDR. Ask the sender for one.
    NeedKeyFrame,
}

pub struct Decoder {
    mft: IMFTransform,
    geometry: Option<Geometry>,
    waiting_key: bool,
    picture: Vec<u8>,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("H.264 decoder")?;
            // Output each picture as soon as it is decoded: no reordering
            // buffer (camera streams have no B frames).
            if let Ok(a) = mft.GetAttributes() {
                let _ = a.SetUINT32(&CODECAPI_AVLowLatencyMode, 1);
            }
            let it = MFCreateMediaType()?;
            it.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            it.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            mft.SetInputType(0, &it, 0)
                .context("H.264 decoder input type")?;
            let mut d = Decoder {
                mft,
                geometry: None,
                waiting_key: true,
                picture: Vec::new(),
            };
            d.set_output()?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            d.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(d)
        }
    }

    /// Picks NV12 output and reads its geometry (again after a stream
    /// change: new size).
    fn set_output(&mut self) -> Result<()> {
        unsafe {
            for i in 0.. {
                let t = match self.mft.GetOutputAvailableType(0, i) {
                    Ok(t) => t,
                    Err(e) if e.code() == MF_E_NO_MORE_TYPES => {
                        bail!("H.264 decoder has no NV12 output")
                    }
                    Err(e) => return Err(e.into()),
                };
                if t.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                    continue;
                }
                self.mft.SetOutputType(0, &t, 0)?;
                let (w, h) = unpack(t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0));
                let stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(w).max(w);
                let mut area = MFVideoArea::default();
                let has_aperture = t
                    .GetBlob(
                        &MF_MT_MINIMUM_DISPLAY_APERTURE,
                        std::slice::from_raw_parts_mut(
                            (&mut area as *mut MFVideoArea).cast(),
                            size_of::<MFVideoArea>(),
                        ),
                        None,
                    )
                    .is_ok();
                let (dw, dh) = if has_aperture && area.Area.cx > 0 && area.Area.cy > 0 {
                    (area.Area.cx as u32, area.Area.cy as u32)
                } else {
                    (w, h)
                };
                self.geometry = Some(Geometry {
                    width: dw,
                    height: dh,
                    stride,
                    alloc_height: h,
                });
                tracing::debug!(geometry = ?self.geometry, "H.264 decoder output");
                return Ok(());
            }
            unreachable!()
        }
    }

    /// Back to waiting for an IDR, decoder state dropped.
    pub fn reset(&mut self) {
        let _ = unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) };
        self.waiting_key = true;
    }

    /// Decodes one access unit; each picture goes to `emit(width, height,
    /// nv12)` (tight, stride = width).
    pub fn decode(
        &mut self,
        au: &[u8],
        time: i64,
        mut emit: impl FnMut(u32, u32, &[u8]),
    ) -> Result<Decoded> {
        if self.waiting_key {
            if !is_key_frame(au) {
                return Ok(Decoded::NeedKeyFrame);
            }
            self.waiting_key = false;
        }
        let sample = sample_from(au, time)?;
        if let Err(e) = unsafe { self.mft.ProcessInput(0, &sample, 0) } {
            tracing::warn!("H.264 decoder rejected a frame: {e}");
            self.reset();
            return Ok(Decoded::NeedKeyFrame);
        }
        loop {
            match process_output(&self.mft) {
                Ok(Some(picture)) => self.emit(&picture, &mut emit)?,
                Ok(None) => return Ok(Decoded::Ok),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.set_output()?,
                Err(e) => {
                    tracing::warn!("H.264 decoding failed: {e}");
                    self.reset();
                    return Ok(Decoded::NeedKeyFrame);
                }
            }
        }
    }

    fn emit(&mut self, picture: &IMFSample, emit: &mut impl FnMut(u32, u32, &[u8])) -> Result<()> {
        let Some(g) = self.geometry else {
            return Ok(());
        };
        let (w, h, stride) = (g.width as usize, g.height as usize, g.stride as usize);
        unsafe {
            let buffer = picture.ConvertToContiguousBuffer()?;
            let mut p = std::ptr::null_mut();
            let mut len = 0u32;
            buffer.Lock(&mut p, None, Some(&mut len))?;
            let src = std::slice::from_raw_parts(p, len as usize);
            let uv_offset = stride * g.alloc_height as usize;
            if uv_offset + stride * (h / 2) <= src.len() && w <= stride {
                self.picture.resize(w * h * 3 / 2, 0);
                let (y, uv) = self.picture.split_at_mut(w * h);
                for row in 0..h {
                    y[row * w..][..w].copy_from_slice(&src[row * stride..][..w]);
                }
                for row in 0..h / 2 {
                    uv[row * w..][..w].copy_from_slice(&src[uv_offset + row * stride..][..w]);
                }
                emit(g.width, g.height, &self.picture);
            }
            buffer.Unlock()?;
        }
        Ok(())
    }
}

/// H.264 encoder (baseline, Annex B), for the test camera and the tests.
pub struct Encoder {
    mft: IMFTransform,
    frame_duration: i64,
    frames: i64,
}

impl Encoder {
    pub fn new(width: u32, height: u32, fps_num: u32, fps_den: u32, bitrate: u32) -> Result<Self> {
        unsafe {
            let mft: IMFTransform =
                CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)
                    .context("H.264 encoder")?;
            let set_video = |t: &IMFMediaType, subtype: &GUID| -> windows_core::Result<()> {
                t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
                t.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
                t.SetUINT64(&MF_MT_FRAME_RATE, pack(fps_num, fps_den))?;
                t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
                t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            };
            let ot = MFCreateMediaType()?;
            set_video(&ot, &MFVideoFormat_H264)?;
            ot.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            ot.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            mft.SetOutputType(0, &ot, 0)
                .context("H.264 encoder output type")?;
            let it = MFCreateMediaType()?;
            set_video(&it, &MFVideoFormat_NV12)?;
            mft.SetInputType(0, &it, 0)
                .context("H.264 encoder input type")?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let frame_duration = 10_000_000 * i64::from(fps_den.max(1)) / i64::from(fps_num.max(1));
            Ok(Encoder {
                mft,
                frame_duration,
                frames: 0,
            })
        }
    }

    /// Encodes one NV12 picture; returns the access units it produced.
    pub fn encode(&mut self, nv12: &[u8]) -> Result<Vec<Vec<u8>>> {
        let sample = sample_from(nv12, self.frames * self.frame_duration)?;
        unsafe { sample.SetSampleDuration(self.frame_duration)? };
        self.frames += 1;
        unsafe { self.mft.ProcessInput(0, &sample, 0)? };
        let mut out = Vec::new();
        while let Some(au) = process_output(&self.mft)? {
            unsafe {
                let buffer = au.ConvertToContiguousBuffer()?;
                let mut p = std::ptr::null_mut();
                let mut len = 0u32;
                buffer.Lock(&mut p, None, Some(&mut len))?;
                out.push(std::slice::from_raw_parts(p, len as usize).to_vec());
                buffer.Unlock()?;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::vcam::test_pattern;
    use vcam_shm::Format;

    #[test]
    fn nal_parsing() {
        let au = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 9,
        ];
        assert_eq!(nal_types(&au).collect::<Vec<_>>(), [7, 8, 5]);
        assert!(is_key_frame(&au));
        assert!(!is_key_frame(&[0, 0, 0, 1, 0x41, 1, 2]));
        assert_eq!(nal_types(&[0, 0, 1]).count(), 0);
    }

    fn mf() {
        crate::camera::vcam::init_mf().unwrap();
    }

    #[test]
    fn encode_then_decode() {
        mf();
        let f = Format {
            width: 320,
            height: 240,
            fps_num: 30,
            fps_den: 1,
        };
        let mut enc = Encoder::new(f.width, f.height, 30, 1, 1_000_000).unwrap();
        let mut dec = Decoder::new().unwrap();
        let mut frame = vec![0u8; f.nv12_bytes()];
        let mut decoded = 0;
        let mut worst = 0f64;
        let mut first_key = None;
        for t in 0..30 {
            test_pattern(&f, t, &mut frame);
            for au in enc.encode(&frame).unwrap() {
                first_key.get_or_insert(is_key_frame(&au));
                let st = dec
                    .decode(&au, 0, |w, h, nv12| {
                        assert_eq!((w, h), (f.width, f.height));
                        // Mean luma error against the source of this frame
                        // or a neighbour (encoder delay): just bounded.
                        let err = nv12[..(w * h) as usize]
                            .iter()
                            .zip(&frame[..(w * h) as usize])
                            .map(|(a, b)| f64::from(a.abs_diff(*b)))
                            .sum::<f64>()
                            / f64::from(w * h);
                        worst = worst.max(err);
                        decoded += 1;
                    })
                    .unwrap();
                assert_eq!(st, Decoded::Ok);
            }
        }
        assert_eq!(first_key, Some(true), "first access unit is an IDR");
        assert!(decoded >= 20, "decoded {decoded} of 30");
        assert!(worst < 30.0, "mean luma error {worst}");
    }

    /// Large webcam sizes. 1080p is coded on 1088 lines: the decoder output
    /// must be cropped back to 1080, chroma included.
    #[test]
    fn large_sizes_round_trip() {
        mf();
        for (width, height) in [(1920, 1080), (2560, 1440)] {
            let f = Format {
                width,
                height,
                fps_num: 30,
                fps_den: 1,
            };
            let mut enc = Encoder::new(f.width, f.height, 30, 1, 8_000_000).unwrap();
            let mut dec = Decoder::new().unwrap();
            let mut frame = vec![0u8; f.nv12_bytes()];
            let mut checked = 0;
            for t in 0..10 {
                test_pattern(&f, t, &mut frame);
                for au in enc.encode(&frame).unwrap() {
                    dec.decode(&au, 0, |w, h, nv12| {
                        assert_eq!((w, h), (width, height));
                        assert_eq!(nv12.len(), f.nv12_bytes());
                        // Bottom chroma band of the pattern: (u, v) = (128, 128).
                        let uv = &nv12[(w * h) as usize..];
                        let last_row = &uv[uv.len() - w as usize..];
                        let mean_u = last_row
                            .iter()
                            .step_by(2)
                            .map(|&u| u32::from(u))
                            .sum::<u32>()
                            / (w / 2);
                        assert!(
                            mean_u.abs_diff(128) < 12,
                            "{width}x{height}: last chroma row u = {mean_u}"
                        );
                        checked += 1;
                    })
                    .unwrap();
                }
            }
            assert!(checked > 0, "no {width}x{height} picture decoded");
        }
    }

    #[test]
    fn waits_for_a_key_frame() {
        mf();
        let f = Format {
            width: 160,
            height: 120,
            fps_num: 30,
            fps_den: 1,
        };
        let mut enc = Encoder::new(f.width, f.height, 30, 1, 500_000).unwrap();
        let mut frame = vec![0u8; f.nv12_bytes()];
        let mut aus = Vec::new();
        // The encoder holds the first pictures back: encode enough of them
        // to get P frames out.
        for t in 0..20 {
            test_pattern(&f, t, &mut frame);
            aus.extend(enc.encode(&frame).unwrap());
        }
        let mut dec = Decoder::new().unwrap();
        // Joining mid-stream: P frames are dropped until an IDR.
        let p = aus.iter().find(|au| !is_key_frame(au)).expect("a P frame");
        assert_eq!(
            dec.decode(p, 0, |_, _, _| panic!("decoded a P frame first"))
                .unwrap(),
            Decoded::NeedKeyFrame
        );
        assert_eq!(dec.decode(&aus[0], 0, |_, _, _| {}).unwrap(), Decoded::Ok);
    }
}
