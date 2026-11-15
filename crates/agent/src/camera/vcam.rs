// SPDX-License-Identifier: GPL-3.0-or-later

//! Virtual cameras (Windows 11): one per client camera.
//!
//! The agent fills a shared memory slot ([`vcam_shm`]) and registers a
//! virtual camera whose media source (`vcam_source.dll`, loaded by Frame
//! Server) serves that slot. The DLL must be registered (`regsvr32`).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use vcam_shm::{Format, Shm};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFVirtualCamera, MF_VERSION, MFCreateVirtualCamera, MFSTARTUP_FULL,
    MFStartup, MFVirtualCameraAccess_AllUsers, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows_core::HSTRING;

use crate::camera::h264;

pub struct VirtualCamera {
    pub slot: u32,
    pub shm: Arc<Shm>,
    camera: IMFVirtualCamera,
}

/// COM (MTA) and Media Foundation on the calling thread.
pub fn init_mf() -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).context("MFStartup")?;
    }
    Ok(())
}

/// `vcam-source.log`, next to ours, writable by Frame Server (LocalService):
/// the media source logs there what apps ask of the camera.
fn source_log_writable() {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, SetFileSecurityW,
    };
    let path = crate::log_dir().join("vcam-source.log");
    if std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .is_err()
    {
        return;
    }
    // SYSTEM and administrators: all; LocalService: write (append) and
    // read; users: read, like the other logs.
    let sddl = crate::win::wide("D:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FRFW;;;LS)(A;;FR;;;BU)");
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        ) != 0
        {
            let p = crate::win::wide(path.display().to_string());
            if SetFileSecurityW(p.as_ptr(), DACL_SECURITY_INFORMATION, sd) == 0 {
                tracing::debug!("vcam-source.log: {}", std::io::Error::last_os_error());
            }
            LocalFree(sd);
        }
    }
}

impl VirtualCamera {
    /// Publishes `formats` in `slot` and registers the camera. It shows up
    /// for every user (needs admin or SYSTEM) until dropped.
    pub fn create(slot: u32, name: &str, formats: &[Format]) -> Result<Self> {
        init_mf()?;
        source_log_writable();
        let capacity = formats.iter().map(Format::nv12_bytes).max().unwrap_or(0);
        let shm = Arc::new(
            Shm::create(slot, capacity)
                .with_context(|| format!("shared memory of camera slot {slot}"))?,
        );
        shm.set_formats(formats);
        shm.set_target(buffer_frames());
        let clsid = vcam_shm::clsid_string(slot);
        let camera = unsafe {
            MFCreateVirtualCamera(
                MFVirtualCameraType_SoftwareCameraSource,
                MFVirtualCameraLifetime_Session,
                MFVirtualCameraAccess_AllUsers,
                &HSTRING::from(name),
                &HSTRING::from(clsid.as_str()),
                None,
            )
        }
        .context("MFCreateVirtualCamera (is vcam_source.dll registered with regsvr32?)")?;
        unsafe { camera.Start(None::<&IMFAsyncCallback>) }.context("IMFVirtualCamera::Start")?;
        tracing::info!(slot, name, ?formats, "virtual camera created");
        Ok(Self { slot, shm, camera })
    }
}

/// Frames the media source buffers to smooth out jitter
/// (`AMELEO_DISPLAY_AGENT_CAMERA_BUFFER`, default [`vcam_shm::DEFAULT_TARGET`]).
/// One frame period of latency per frame.
fn buffer_frames() -> u32 {
    std::env::var("AMELEO_DISPLAY_AGENT_CAMERA_BUFFER")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(vcam_shm::DEFAULT_TARGET)
}

impl Drop for VirtualCamera {
    fn drop(&mut self) {
        // Session lifetime: shutting down removes the camera.
        if let Err(e) = unsafe { self.camera.Shutdown() } {
            tracing::warn!(slot = self.slot, "virtual camera shutdown: {e}");
        }
        tracing::info!(slot = self.slot, "virtual camera removed");
    }
}

/// Moving test pattern: gray gradient, a white bar sweeping across, color
/// bands in the chroma plane.
pub fn test_pattern(f: &Format, t: u32, out: &mut [u8]) {
    let (w, h) = (f.width as usize, f.height as usize);
    let bar = (t as usize * 8) % w;
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    for (row, line) in y_plane.chunks_exact_mut(w).enumerate() {
        for (x, px) in line.iter_mut().enumerate() {
            *px = if x.abs_diff(bar) < 12 {
                235
            } else {
                (16 + (x + row) * 200 / (w + h)) as u8
            };
        }
    }
    for (row, line) in uv_plane.chunks_exact_mut(w).enumerate() {
        let band = row * 4 / (h / 2).max(1);
        let (u, v) = [(90, 240), (54, 34), (240, 110), (128, 128)][band.min(3)];
        for px in line.as_chunks_mut::<2>().0 {
            px[0] = u;
            px[1] = v;
        }
    }
}

static STOP: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn on_ctrl(_: u32) -> windows::core::BOOL {
    STOP.store(true, Ordering::SeqCst);
    true.into()
}

/// `ameleo-display-agent camera-test [--h264]`: a virtual camera with a test
/// pattern, no host. Validates the DLL registration and the Frame Server
/// path; with `--h264` the pattern also goes through the H.264 encoder and
/// the decoder the client frames use.
pub fn run_test(h264: bool) -> Result<()> {
    use windows::Win32::System::Console::SetConsoleCtrlHandler;
    let formats = [
        Format {
            width: 1280,
            height: 720,
            fps_num: 30,
            fps_den: 1,
        },
        Format {
            width: 640,
            height: 480,
            fps_num: 30,
            fps_den: 1,
        },
    ];
    let camera = VirtualCamera::create(0, "QEMU Agent Test", &formats)?;
    unsafe { SetConsoleCtrlHandler(Some(on_ctrl), true)? };
    println!(
        "Virtual camera \"QEMU Agent Test\" is up. Open it in the Camera app; Ctrl+C to remove it."
    );

    let mut buf = Vec::new();
    let mut t = 0u32;
    let mut last = None;
    let mut codec: Option<(h264::Encoder, h264::Decoder)> = None;
    let start = Instant::now();
    while !STOP.load(Ordering::SeqCst) {
        let requested = camera.shm.stream();
        if requested != last {
            match requested {
                Some(i) => tracing::info!(format = ?formats.get(i), "an app started the camera"),
                None => tracing::info!("the camera is stopped"),
            }
            last = requested;
            codec = None; // new size: new encoder and decoder
        }
        let Some(f) = requested.and_then(|i| formats.get(i)) else {
            camera.shm.wait_state(200);
            continue;
        };
        buf.resize(f.nv12_bytes(), 0);
        test_pattern(f, t, &mut buf);
        let ts = start.elapsed().as_micros() as u64;
        if h264 {
            if codec.is_none() {
                codec = Some((
                    h264::Encoder::new(f.width, f.height, f.fps_num, f.fps_den, 2_000_000)?,
                    h264::Decoder::new()?,
                ));
            }
            let (enc, dec) = codec.as_mut().expect("just set");
            for au in enc.encode(&buf)? {
                dec.decode(&au, ts as i64 * 10, |w, h, nv12| {
                    camera
                        .shm
                        .write_frame(w, h, ts, |dst| dst.copy_from_slice(nv12));
                })?;
            }
        } else {
            camera
                .shm
                .write_frame(f.width, f.height, ts, |dst| dst.copy_from_slice(&buf));
        }
        t = t.wrapping_add(1);
        std::thread::sleep(Duration::from_nanos(f.frame_duration() as u64 * 100));
    }
    drop(camera);
    Ok(())
}

/// `ameleo-display-agent camera-controls <name>`: opens the camera whose name
/// contains `<name>` the way apps do (through Frame Server) and prints the
/// VideoProcAmp and CameraControl properties it answers.
pub fn run_controls(filter: &str) -> Result<()> {
    use windows::Win32::Media::KernelStreaming::{IKsControl, KSIDENTIFIER};
    use windows::Win32::Media::MediaFoundation::{
        IMFActivate, IMFAttributes, IMFMediaSource, MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME,
        MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        MFCreateAttributes, MFEnumDeviceSources,
    };
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows_core::{GUID, Interface};

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ControlS {
        set: GUID,
        id: u32,
        flags: u32,
        value: i32,
        value_flags: u32,
        capabilities: u32,
    }
    /// A property set: its name, its GUID, its properties (id, name).
    type PropertySet = (&'static str, u128, &'static [(u32, &'static str)]);
    const SETS: [PropertySet; 2] = [
        (
            "VideoProcAmp",
            0xC6E13360_30AC_11D0_A18C_00A0C9118956,
            &[
                (0, "Brightness"),
                (1, "Contrast"),
                (2, "Hue"),
                (7, "WhiteBalance"),
                (8, "BacklightCompensation"),
            ],
        ),
        (
            "CameraControl",
            0xC6E13370_30AC_11D0_A18C_00A0C9118956,
            &[
                (0, "Pan"),
                (1, "Tilt"),
                (2, "Roll"),
                (3, "Zoom"),
                (4, "Exposure"),
                (6, "Focus"),
            ],
        ),
    ];

    init_mf()?;
    let mut attrs: Option<IMFAttributes> = None;
    unsafe { MFCreateAttributes(&mut attrs, 1)? };
    let attrs = attrs.context("attributes")?;
    unsafe {
        attrs.SetGUID(
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID,
        )?
    };
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count)? };
    let devices: Vec<IMFActivate> = unsafe { std::slice::from_raw_parts_mut(list, count as usize) }
        .iter_mut()
        .filter_map(Option::take)
        .collect();
    unsafe { CoTaskMemFree(Some(list.cast())) };
    for dev in devices {
        let mut name = windows_core::PWSTR::null();
        let mut len = 0;
        unsafe {
            dev.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut name, &mut len)?
        };
        let n = unsafe { name.to_string() }.unwrap_or_default();
        unsafe { CoTaskMemFree(Some(name.0.cast())) };
        println!("camera: {n}");
        if !n.to_lowercase().contains(&filter.to_lowercase()) {
            continue;
        }
        let source: IMFMediaSource = unsafe { dev.ActivateObject()? };
        let ks: IKsControl = source.cast().context("IKsControl")?;
        for (set_name, set, props) in SETS {
            for &(id, prop_name) in props {
                let req = ControlS {
                    set: GUID::from_u128(set),
                    id,
                    flags: 1,
                    value: 0,
                    value_flags: 0,
                    capabilities: 0,
                };
                let mut out = req;
                let mut ret = 0u32;
                let get = unsafe {
                    ks.KsProperty(
                        (&req as *const ControlS).cast::<KSIDENTIFIER>(),
                        36,
                        (&mut out as *mut ControlS).cast(),
                        36,
                        &mut ret,
                    )
                };
                let mut range = [0u8; 72];
                let mut breq = req;
                breq.flags = 0x200; // BASICSUPPORT
                let basic = unsafe {
                    ks.KsProperty(
                        (&breq as *const ControlS).cast::<KSIDENTIFIER>(),
                        36,
                        range.as_mut_ptr().cast(),
                        72,
                        &mut ret,
                    )
                };
                let i = |at: usize| i32::from_le_bytes(range[at..at + 4].try_into().unwrap());
                match (get, basic) {
                    (Ok(()), Ok(())) => println!(
                        "  {set_name} {prop_name}: value {} flags {:#x} caps {:#x}, range {}..{} step {}",
                        out.value,
                        out.value_flags,
                        out.capabilities,
                        i(64),
                        i(68),
                        i(56)
                    ),
                    (g, b) => println!(
                        "  {set_name} {prop_name}: get {:?}, basic support {:?}",
                        g.err(),
                        b.err()
                    ),
                }
            }
        }
        unsafe { source.Shutdown()? };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pattern_fills_nv12() {
        let f = Format {
            width: 64,
            height: 48,
            fps_num: 30,
            fps_den: 1,
        };
        let mut buf = vec![0u8; f.nv12_bytes()];
        test_pattern(&f, 3, &mut buf);
        assert!(buf[..64 * 48].iter().all(|&y| (16..=235).contains(&y)));
        assert_eq!(buf[64 * 48], 90); // U of the first band
    }
}
