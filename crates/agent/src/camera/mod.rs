// SPDX-License-Identifier: GPL-3.0-only

//! Client cameras (`fr.ameleo.Agent.Camera1`): one virtual camera and one
//! H.264 decoder each, on a dedicated thread (COM / Media Foundation). The
//! decoders run in sandboxed processes (see [`sandbox`]).
//! Each camera is a Windows 11 virtual camera, served by the media source
//! of `vcam-source` from the shared memory of `vcam-shm`.
//!
//! The thread also watches the stream state the media sources publish and
//! reports apps opening and closing the cameras.

pub mod h264;
pub mod sandbox;
pub mod vcam;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use proto::camera_control::{self as cc, Control};
use proto::{AGENT_PATH, Error};
use tokio::sync::oneshot;
use vcam_shm::{ControlInfo, Format, MAX_CONTROLS, MAX_FORMATS};
use zbus::object_server::SignalEmitter;

use crate::feature::Feature;
use crate::host::Host;
use h264::Decoded;
use sandbox::Decoder;
use vcam::{VirtualCamera, init_mf};

/// Frames waiting for the decoder. A slow decoder drops the rest and asks
/// for an IDR to start over cleanly.
const FRAME_QUEUE: usize = 8;
/// State polling and frame wait granularity.
const TICK: Duration = Duration::from_millis(20);
/// At most one key frame request per camera in this interval.
const KEY_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
/// Statistics line per streaming camera.
const STATS_INTERVAL: Duration = Duration::from_secs(10);
/// Delay before a new decoder process after one failed: a stream that
/// crashes decoders must not start one per frame.
const DECODER_RESTART: Duration = Duration::from_secs(1);

/// The client cameras feature.
pub struct Cameras(CameraHandle);

impl Cameras {
    /// Fails when Windows cannot make virtual cameras (see [`supported`]).
    pub fn new(host: Host) -> anyhow::Result<Self> {
        supported()?;
        Ok(Self(spawn(Signals(host))?))
    }
}

/// Whether Windows can make virtual cameras: Media Foundation is there (not
/// in the N editions without the Media Feature Pack) and has
/// `MFCreateVirtualCamera` (Windows 11, build 22000). The agent links them
/// with delayed loading (see `build.rs`), so it must not call them otherwise.
pub fn supported() -> anyhow::Result<()> {
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    for dll in ["mfplat.dll", "mf.dll"] {
        if unsafe { LoadLibraryW(crate::win::wide(dll).as_ptr()) }.is_null() {
            anyhow::bail!("no Media Foundation ({dll}): is the Media Feature Pack installed?");
        }
    }
    let group = unsafe { LoadLibraryW(crate::win::wide("mfsensorgroup.dll").as_ptr()) };
    if group.is_null()
        || unsafe { GetProcAddress(group, c"MFCreateVirtualCamera".as_ptr().cast()) }.is_none()
    {
        anyhow::bail!("no virtual cameras before Windows 11 (build 22000)");
    }
    Ok(())
}

impl Feature for Cameras {
    fn channels(&self) -> Vec<&'static str> {
        vec![proto::iface::CAMERA]
    }

    fn serve<'a>(
        &self,
        builder: zbus::connection::Builder<'a>,
    ) -> zbus::Result<zbus::connection::Builder<'a>> {
        Ok(builder.serve_at(AGENT_PATH, CameraIface(self.0.clone())))
    }

    fn reset(&self) {
        self.0.remove_all();
    }

    fn stop(&self) {
        // Removing the cameras ends their decoder processes.
        self.0.remove_all_wait(Duration::from_secs(2));
        sandbox::delete_profile();
    }
}

struct CameraIface(CameraHandle);

#[zbus::interface(name = "fr.ameleo.Agent.Camera1")]
impl CameraIface {
    async fn add_camera(
        &self,
        id: u32,
        name: &str,
        formats: Vec<(u32, u32, u32, u32)>,
    ) -> proto::Result<()> {
        self.0.add(id, name.to_string(), formats).await
    }

    fn remove_camera(&self, id: u32) {
        self.0.remove(id);
    }

    fn push_frame(&self, id: u32, timestamp_us: u64, data: Vec<u8>) {
        self.0.push(id, timestamp_us, data);
    }

    async fn set_controls(&self, id: u32, controls: Vec<Control>) -> proto::Result<()> {
        self.0.set_controls(id, controls).await
    }

    fn control_value(&self, id: u32, set: u8, property: u8, mode: u8, value: i32) {
        self.0.control_value(id, set, property, mode, value);
    }

    #[zbus(signal)]
    async fn control_requested(
        emitter: &SignalEmitter<'_>,
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn stream_requested(
        emitter: &SignalEmitter<'_>,
        id: u32,
        format: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn stream_stopped(emitter: &SignalEmitter<'_>, id: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn key_frame_needed(emitter: &SignalEmitter<'_>, id: u32) -> zbus::Result<()>;
}

/// Camera events for the host.
enum Event {
    /// An app opened (`Some(format index)`) or closed a client camera.
    Stream { id: u32, format: Option<u32> },
    /// The decoder of a client camera needs an IDR.
    KeyFrame { id: u32 },
    /// An app changed a control of a client camera.
    Control {
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    },
}

/// Sends the camera events to the host as `Camera1` signals.
struct Signals(Host);

impl Signals {
    fn send(&self, event: Event) {
        self.0.emit(move |conn| async move {
            let Ok(iface) = conn
                .object_server()
                .interface::<_, CameraIface>(AGENT_PATH)
                .await
            else {
                return;
            };
            let e = iface.signal_emitter();
            let res = match event {
                Event::Stream { id, format } => {
                    tracing::info!(id, ?format, "camera stream signal sent to the host");
                    match format {
                        Some(f) => CameraIface::stream_requested(e, id, f).await,
                        None => CameraIface::stream_stopped(e, id).await,
                    }
                }
                Event::KeyFrame { id } => CameraIface::key_frame_needed(e, id).await,
                Event::Control {
                    id,
                    set,
                    property,
                    mode,
                    value,
                } => CameraIface::control_requested(e, id, set, property, mode, value).await,
            };
            if let Err(err) = res {
                tracing::warn!("cannot send a camera signal: {err}");
            }
        });
    }
}

/// `install`: registers the media source, `vcam_source.dll`, next to the
/// executable.
pub fn install() {
    match register_source(true) {
        Ok(path) => println!("virtual camera source registered: {}", path.display()),
        Err(e) => println!("virtual camera source not registered, no client cameras: {e:#}"),
    }
}

/// `uninstall`: unregisters the media source.
pub fn uninstall() {
    if register_source(false).is_ok() {
        println!("virtual camera source unregistered");
    }
}

/// `camera-test [--h264]`: a test pattern camera, without a host.
pub fn run_test() -> anyhow::Result<()> {
    let _log = crate::init_logging("camera-test", true);
    supported()?;
    vcam::run_test(std::env::args().any(|a| a == "--h264"))
}

/// `camera-controls <filter>`: the controls of the cameras, as apps see
/// them.
pub fn run_controls() -> anyhow::Result<()> {
    supported()?;
    vcam::run_controls(&std::env::args().nth(2).unwrap_or_default())
}

/// Calls DllRegisterServer or DllUnregisterServer of `vcam_source.dll`,
/// next to the executable (what `regsvr32` does).
fn register_source(register: bool) -> anyhow::Result<std::path::PathBuf> {
    use anyhow::bail;
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    let path = std::env::current_exe()?.with_file_name("vcam_source.dll");
    if !path.exists() {
        bail!("{} not found", path.display());
    }
    unsafe {
        let module = LoadLibraryW(crate::win::wide(&path).as_ptr());
        if module.is_null() {
            bail!(
                "cannot load {}: {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let name: &[u8] = if register {
            b"DllRegisterServer\0"
        } else {
            b"DllUnregisterServer\0"
        };
        let res = match GetProcAddress(module, name.as_ptr()) {
            Some(f) => {
                let f: unsafe extern "system" fn() -> i32 = std::mem::transmute(f);
                let hr = f();
                if hr < 0 {
                    Err(anyhow::anyhow!(
                        "{} failed: {:#010x}",
                        String::from_utf8_lossy(&name[..name.len() - 1]),
                        hr
                    ))
                } else {
                    Ok(())
                }
            }
            None => Err(anyhow::anyhow!("{} is not a COM server", path.display())),
        };
        FreeLibrary(module);
        res.map(|()| path)
    }
}

enum Ctl {
    Add {
        id: u32,
        name: String,
        formats: Vec<Format>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    Remove {
        id: u32,
    },
    Controls {
        id: u32,
        controls: Vec<(ControlInfo, u8, i32)>,
        reply: oneshot::Sender<Result<(), Error>>,
    },
    ControlValue {
        id: u32,
        set: u8,
        property: u8,
        mode: u8,
        value: i32,
    },
    RemoveAll,
    /// RemoveAll, then a reply once the virtual cameras are shut down.
    RemoveAllSync(std_mpsc::SyncSender<()>),
}

struct Frame {
    id: u32,
    timestamp_us: u64,
    data: Vec<u8>,
    /// When it came out of D-Bus: arrival jitter, apart from our decode time.
    arrived: Instant,
}

#[derive(Clone)]
pub struct CameraHandle {
    ctl: std_mpsc::Sender<Ctl>,
    frames: std_mpsc::SyncSender<Frame>,
    /// Frames dropped so far: the decoders must restart from an IDR.
    dropped: Arc<AtomicU64>,
}

impl CameraHandle {
    pub async fn add(
        &self,
        id: u32,
        name: String,
        formats: Vec<(u32, u32, u32, u32)>,
    ) -> Result<(), Error> {
        let formats: Vec<Format> = formats
            .into_iter()
            .map(|(width, height, fps_num, fps_den)| Format {
                width,
                height,
                fps_num,
                fps_den,
            })
            .collect();
        if formats.is_empty() || formats.len() > MAX_FORMATS {
            return Err(Error::InvalidArgument(format!(
                "1 to {MAX_FORMATS} formats"
            )));
        }
        if let Some(f) = formats.iter().find(|f| !f.fits() || f.fps_num == 0) {
            return Err(Error::InvalidArgument(format!("unsupported format {f:?}")));
        }
        let (reply, rx) = oneshot::channel();
        self.ctl
            .send(Ctl::Add {
                id,
                name,
                formats,
                reply,
            })
            .map_err(|_| Error::Failed("camera thread gone".into()))?;
        rx.await
            .map_err(|_| Error::Failed("camera thread gone".into()))?
    }

    pub fn remove(&self, id: u32) {
        let _ = self.ctl.send(Ctl::Remove { id });
    }

    pub async fn set_controls(&self, id: u32, controls: Vec<Control>) -> Result<(), Error> {
        if controls.len() > cc::MAX_CONTROLS {
            return Err(Error::InvalidArgument(format!(
                "at most {} controls",
                cc::MAX_CONTROLS
            )));
        }
        let mut list = Vec::with_capacity(controls.len());
        for (set, property, caps, min, max, step, default, mode, value) in controls {
            let known = match set {
                cc::CAMERA_CONTROL => (cc::EXPOSURE..=cc::ZOOM).contains(&property),
                cc::VIDEO_PROC_AMP => {
                    (cc::BACKLIGHT_COMPENSATION..=cc::WHITE_BALANCE).contains(&property)
                }
                _ => false,
            };
            let caps = caps & (cc::MANUAL | cc::AUTO);
            if !known || caps == 0 || min > max {
                return Err(Error::InvalidArgument(format!(
                    "invalid control: set {set} property {property}"
                )));
            }
            if list
                .iter()
                .any(|(c, _, _): &(ControlInfo, u8, i32)| c.set == set && c.property == property)
            {
                return Err(Error::InvalidArgument(format!(
                    "control given twice: set {set} property {property}"
                )));
            }
            let info = ControlInfo {
                set,
                property,
                caps,
                min,
                max,
                step: step.max(1),
                default,
            };
            list.push((info, mode, value));
        }
        let (reply, rx) = oneshot::channel();
        self.ctl
            .send(Ctl::Controls {
                id,
                controls: list,
                reply,
            })
            .map_err(|_| Error::Failed("camera thread gone".into()))?;
        rx.await
            .map_err(|_| Error::Failed("camera thread gone".into()))?
    }

    pub fn control_value(&self, id: u32, set: u8, property: u8, mode: u8, value: i32) {
        let _ = self.ctl.send(Ctl::ControlValue {
            id,
            set,
            property,
            mode,
            value,
        });
    }

    pub fn remove_all(&self) {
        let _ = self.ctl.send(Ctl::RemoveAll);
    }

    /// Agent stopping: remove the cameras and wait for it (bounded).
    pub fn remove_all_wait(&self, timeout: Duration) {
        let (tx, rx) = std_mpsc::sync_channel(1);
        if self.ctl.send(Ctl::RemoveAllSync(tx)).is_ok() {
            let _ = rx.recv_timeout(timeout);
        }
    }

    /// Never blocks the D-Bus task: a full queue drops the frame.
    pub fn push(&self, id: u32, timestamp_us: u64, data: Vec<u8>) {
        if self
            .frames
            .try_send(Frame {
                id,
                timestamp_us,
                data,
                arrived: Instant::now(),
            })
            .is_err()
        {
            self.dropped.fetch_add(1, Ordering::AcqRel);
            tracing::debug!(id, "camera frame dropped (decoder behind)");
        }
    }
}

struct Camera {
    vc: VirtualCamera,
    decoder: Option<Decoder>,
    /// When the last decoder process failed: the next one waits
    /// [`DECODER_RESTART`].
    decoder_failed: Option<Instant>,
    /// Stream state last reported to the host.
    stream: Option<usize>,
    last_key_request: Option<Instant>,
    stats: Stats,
    /// Apps' control changes passed on so far, per control.
    seen_controls: [u32; MAX_CONTROLS],
}

/// Counters over the last [`STATS_INTERVAL`], logged while streaming.
#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    received: u64,
    pictures: u64,
    dropped: u64,
    key_requests: u64,
    /// Longest time between two frames coming out of D-Bus: network jitter.
    max_gap: Duration,
    max_decode: Duration,
    last_arrival: Option<Instant>,
}

impl Stats {
    fn flush(&mut self, id: u32, streaming: bool) {
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() < STATS_INTERVAL {
            return;
        }
        if streaming || self.received > 0 {
            tracing::info!(
                id,
                received = self.received,
                pictures = self.pictures,
                dropped = self.dropped,
                key_requests = self.key_requests,
                max_gap_ms = self.max_gap.as_millis() as u64,
                max_decode_ms = self.max_decode.as_millis() as u64,
                "camera stats (10 s)"
            );
        }
        *self = Stats {
            since: Some(Instant::now()),
            last_arrival: self.last_arrival,
            ..Default::default()
        };
    }
}

fn spawn(notify: Signals) -> std::io::Result<CameraHandle> {
    let (ctl, ctl_rx) = std_mpsc::channel();
    let (frames, frames_rx) = std_mpsc::sync_channel(FRAME_QUEUE);
    let dropped = Arc::new(AtomicU64::new(0));
    let d = dropped.clone();
    std::thread::Builder::new()
        .name("cameras".into())
        .spawn(move || run(ctl_rx, frames_rx, &d, notify))?;
    Ok(CameraHandle {
        ctl,
        frames,
        dropped,
    })
}

fn run(
    ctl: std_mpsc::Receiver<Ctl>,
    frames: std_mpsc::Receiver<Frame>,
    dropped: &AtomicU64,
    notify: Signals,
) {
    if let Err(e) = init_mf() {
        tracing::error!("cameras disabled: {e:#}");
        return;
    }
    let mut cameras: HashMap<u32, Camera> = HashMap::new();
    let mut seen_drops = 0;
    loop {
        loop {
            match ctl.try_recv() {
                Ok(c) => control(c, &mut cameras, &notify),
                Err(std_mpsc::TryRecvError::Empty) => break,
                Err(std_mpsc::TryRecvError::Disconnected) => return,
            }
        }
        match frames.recv_timeout(TICK) {
            Ok(f) => {
                let drops = dropped.load(Ordering::Acquire);
                if drops != seen_drops {
                    for cam in cameras.values_mut() {
                        cam.stats.dropped += drops - seen_drops;
                        if let Some(d) = &mut cam.decoder {
                            d.reset();
                        }
                    }
                    seen_drops = drops;
                }
                frame(f, &mut cameras, &notify)
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => return,
        }
        for (&id, cam) in cameras.iter_mut() {
            watch_stream(id, cam, &notify);
            for (set, property, mode, value) in cam.vc.shm.take_requests(&mut cam.seen_controls) {
                tracing::info!(
                    id,
                    set,
                    property,
                    mode,
                    value,
                    "camera control changed by an app"
                );
                notify.send(Event::Control {
                    id,
                    set,
                    property,
                    mode,
                    value,
                });
            }
            cam.stats.flush(id, cam.stream.is_some());
        }
    }
}

fn control(c: Ctl, cameras: &mut HashMap<u32, Camera>, notify: &Signals) {
    match c {
        Ctl::Add {
            id,
            name,
            formats,
            reply,
        } => {
            if let Some(old) = cameras.remove(&id) {
                stopped(id, &old, notify);
            }
            let used: Vec<u32> = cameras.values().map(|c| c.vc.slot).collect();
            // A free slot can still be held by an app that kept the source of
            // a removed camera: the next one is tried.
            let mut created = None;
            for slot in (0..vcam_shm::SLOTS).filter(|s| !used.contains(s)) {
                match VirtualCamera::create(slot, &name, &formats) {
                    Err(e)
                        if e.downcast_ref::<std::io::Error>()
                            .is_some_and(|e| e.kind() == std::io::ErrorKind::AlreadyExists) =>
                    {
                        tracing::debug!(slot, "camera slot still held, trying the next one");
                    }
                    res => {
                        created = Some((slot, res));
                        break;
                    }
                }
            }
            let Some((slot, res)) = created else {
                let _ = reply.send(Err(Error::Failed(format!(
                    "at most {} cameras",
                    vcam_shm::SLOTS
                ))));
                return;
            };
            let res = match res {
                Ok(vc) => {
                    cameras.insert(
                        id,
                        Camera {
                            vc,
                            decoder: None,
                            decoder_failed: None,
                            stream: None,
                            last_key_request: None,
                            stats: Stats::default(),
                            seen_controls: [0; MAX_CONTROLS],
                        },
                    );
                    tracing::info!(id, slot, name, "client camera added");
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(id, "cannot create the virtual camera: {e:#}");
                    Err(Error::Failed(format!("{e:#}")))
                }
            };
            let _ = reply.send(res);
        }
        Ctl::Remove { id } => {
            if let Some(cam) = cameras.remove(&id) {
                stopped(id, &cam, notify);
                tracing::info!(id, "client camera removed");
            }
        }
        Ctl::Controls {
            id,
            controls,
            reply,
        } => {
            let res = match cameras.get_mut(&id) {
                Some(cam) => {
                    cam.vc.shm.set_controls(&controls);
                    // What apps asked before belongs to the old list.
                    let _ = cam.vc.shm.take_requests(&mut cam.seen_controls);
                    tracing::info!(id, count = controls.len(), "camera controls set");
                    Ok(())
                }
                None => Err(Error::InvalidArgument(format!("no camera {id}"))),
            };
            let _ = reply.send(res);
        }
        Ctl::ControlValue {
            id,
            set,
            property,
            mode,
            value,
        } => {
            if let Some(cam) = cameras.get(&id)
                && !cam.vc.shm.update_control(set, property, mode, value)
            {
                tracing::debug!(id, set, property, "value of an unknown camera control");
            }
        }
        Ctl::RemoveAll => {
            for (id, cam) in cameras.drain() {
                stopped(id, &cam, notify);
            }
        }
        Ctl::RemoveAllSync(done) => {
            cameras.clear();
            let _ = done.send(());
        }
    }
}

/// A removed camera that was streaming: the host must stop the client
/// stream.
fn stopped(id: u32, cam: &Camera, notify: &Signals) {
    if cam.stream.is_some() {
        notify.send(Event::Stream { id, format: None });
    }
}

fn watch_stream(id: u32, cam: &mut Camera, notify: &Signals) {
    let now = cam.vc.shm.stream();
    if now == cam.stream {
        return;
    }
    tracing::info!(id, from = ?cam.stream, to = ?now, "camera stream state");
    cam.stream = now;
    if now.is_some() {
        // A new stream starts with an IDR from the client.
        if let Some(d) = &mut cam.decoder {
            d.reset();
        }
        cam.last_key_request = None;
    }
    notify.send(Event::Stream {
        id,
        format: now.map(|i| i as u32),
    });
}

fn frame(f: Frame, cameras: &mut HashMap<u32, Camera>, notify: &Signals) {
    let Some(cam) = cameras.get_mut(&f.id) else {
        return;
    };
    if cam.decoder.is_none() {
        if cam
            .decoder_failed
            .is_some_and(|t| t.elapsed() < DECODER_RESTART)
        {
            return;
        }
        match Decoder::new() {
            Ok(d) => cam.decoder = Some(d),
            Err(e) => {
                tracing::warn!(id = f.id, "H.264 decoder unavailable: {e:#}");
                cam.decoder_failed = Some(Instant::now());
                return;
            }
        }
    }
    let now = Instant::now();
    let stats = &mut cam.stats;
    stats.received += 1;
    if let Some(prev) = stats.last_arrival.replace(f.arrived) {
        stats.max_gap = stats.max_gap.max(f.arrived.saturating_duration_since(prev));
    }
    let decoder = cam.decoder.as_mut().expect("just created");
    let shm = &cam.vc.shm;
    let ts = f.timestamp_us;
    let mut pictures = 0;
    let res = decoder.decode(&f.data, (ts as i64).saturating_mul(10), |w, h, nv12| {
        shm.write_frame(w, h, ts, |dst| dst.copy_from_slice(nv12));
        pictures += 1;
    });
    stats.pictures += pictures;
    stats.max_decode = stats.max_decode.max(now.elapsed());
    let need_key = match res {
        Ok(Decoded::Ok) => false,
        Ok(Decoded::NeedKeyFrame) => true,
        Err(e) => {
            // The decoder process crashed, hung or broke the protocol.
            tracing::warn!(id = f.id, "camera decoder dropped: {e:#}");
            cam.decoder = None;
            cam.decoder_failed = Some(Instant::now());
            true
        }
    };
    if need_key
        && cam
            .last_key_request
            .is_none_or(|t| t.elapsed() >= KEY_REQUEST_INTERVAL)
    {
        cam.last_key_request = Some(Instant::now());
        cam.stats.key_requests += 1;
        tracing::info!(id = f.id, "asking the host for a key frame");
        notify.send(Event::KeyFrame { id: f.id });
    }
}
