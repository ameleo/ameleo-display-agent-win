// SPDX-License-Identifier: GPL-3.0-or-later

//! The video stream: answers RequestSample with the agent's last frame, at
//! the camera frame rate, from a worker thread.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use vcam_shm::{Format, Read, Shm};

use crate::playout::{Playout, Ticker};
use windows::Win32::Foundation::{E_POINTER, S_OK};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{ComObject, GUID, HRESULT, IUnknown, Interface, Ref, implement};

use crate::source::NV12;

/// COM pointers moved to the worker thread. Media Foundation objects are
/// free threaded.
struct Agile<T>(T);
unsafe impl<T> Send for Agile<T> {}
unsafe impl<T> Sync for Agile<T> {}

struct State {
    queue: Option<Agile<IMFMediaEventQueue>>,
    state: MF_STREAM_STATE,
    /// Started once by the source (MENewStream sent).
    active: bool,
    format: Format,
    /// Tokens of RequestSample calls not answered yet.
    pending: VecDeque<Option<Agile<IUnknown>>>,
    worker: bool,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[implement(IMFMediaStream2)]
pub struct Stream {
    source: Mutex<Option<IMFMediaSource>>,
    sd: IMFStreamDescriptor,
    formats: Vec<Format>,
    shm: Option<Arc<Shm>>,
    shared: Arc<Shared>,
}

impl Stream {
    pub fn new(
        source: IMFMediaSource,
        sd: IMFStreamDescriptor,
        formats: Vec<Format>,
        shm: Option<Arc<Shm>>,
    ) -> windows_core::Result<ComObject<Stream>> {
        let queue = unsafe { MFCreateEventQueue()? };
        Ok(ComObject::new(Stream {
            source: Mutex::new(Some(source)),
            sd,
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    queue: Some(Agile(queue)),
                    state: MF_STREAM_STATE_STOPPED,
                    active: false,
                    format: formats[0],
                    pending: VecDeque::new(),
                    worker: false,
                }),
                wake: Condvar::new(),
            }),
            formats,
            shm,
        }))
    }

    pub fn is_active(&self) -> bool {
        self.shared.state.lock().unwrap().active
    }

    fn queue(&self) -> windows_core::Result<IMFMediaEventQueue> {
        let st = self.shared.state.lock().unwrap();
        st.queue
            .as_ref()
            .map(|q| q.0.clone())
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    /// Source Start: run with format `index`.
    pub fn start(&self, index: usize) -> windows_core::Result<()> {
        let format = self.formats[index.min(self.formats.len() - 1)];
        let queue = {
            let mut st = self.shared.state.lock().unwrap();
            st.format = format;
            st.active = true;
            st.queue
                .as_ref()
                .map(|q| q.0.clone())
                .ok_or(MF_E_SHUTDOWN)?
        };
        self.run(true);
        if let Some(shm) = &self.shm {
            shm.set_stream(Some(index));
        }
        let now = PROPVARIANT::from(unsafe { MFGetSystemTime() });
        unsafe { queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID::zeroed(), S_OK, &now) }
    }

    /// Source Stop (event) or deselection.
    pub fn stop(&self, event: bool) {
        let queue = {
            let mut st = self.shared.state.lock().unwrap();
            st.state = MF_STREAM_STATE_STOPPED;
            st.active = false;
            st.pending.clear();
            st.queue.as_ref().map(|q| q.0.clone())
        };
        self.shared.wake.notify_all();
        if let Some(shm) = &self.shm {
            shm.set_stream(None);
        }
        if let (true, Some(q)) = (event, queue) {
            let now = PROPVARIANT::from(unsafe { MFGetSystemTime() });
            let _ = unsafe {
                q.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID::zeroed(), S_OK, &now)
            };
        }
    }

    pub fn shutdown(&self) {
        self.stop(false);
        let queue = self.shared.state.lock().unwrap().queue.take();
        self.shared.wake.notify_all();
        if let Some(q) = queue {
            let _ = unsafe { q.0.Shutdown() };
        }
        // Breaks the source <-> stream cycle.
        self.source.lock().unwrap().take();
    }

    /// RUNNING state and the worker that answers sample requests.
    fn run(&self, running: bool) {
        let spawn = {
            let mut st = self.shared.state.lock().unwrap();
            st.state = if running {
                MF_STREAM_STATE_RUNNING
            } else {
                MF_STREAM_STATE_PAUSED
            };
            let spawn = running && !st.worker;
            st.worker |= spawn;
            spawn
        };
        self.shared.wake.notify_all();
        if spawn {
            let shared = self.shared.clone();
            let shm = self.shm.clone();
            let _ = std::thread::Builder::new()
                .name("vcam-stream".into())
                .spawn(move || worker(shared, shm));
        }
    }
}

/// Answers one pending RequestSample per frame period, with the frames of
/// the agent in order (see [`Playout`]), until the stream stops.
fn worker(shared: Arc<Shared>, shm: Option<Arc<Shm>>) {
    // Frame shown last, repeated when no new one is due.
    let mut last = Vec::new();
    let mut playout = Playout::default();
    let mut ticker = Ticker::new();
    loop {
        let format = {
            let mut st = shared.state.lock().unwrap();
            while st.state == MF_STREAM_STATE_RUNNING && st.pending.is_empty() && st.queue.is_some()
            {
                st = shared.wake.wait(st).unwrap();
            }
            if st.state != MF_STREAM_STATE_RUNNING || st.queue.is_none() {
                st.worker = false;
                return;
            }
            st.format
        };
        ticker.tick(Duration::from_nanos(format.frame_duration() as u64 * 100));
        if let Some(shm) = &shm
            && let Some(n) = playout.pick(shm.written(), u64::from(shm.target()))
        {
            let read = shm.read_frame(n, format.width, format.height, |src| {
                last.clear();
                last.extend_from_slice(src);
            });
            if read == Read::Overwritten {
                playout.resync();
            }
        }
        let sample = match make_sample(&format, &mut last) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut st = shared.state.lock().unwrap();
        if st.state != MF_STREAM_STATE_RUNNING || st.format != format {
            continue;
        }
        let (Some(token), Some(queue)) = (st.pending.pop_front(), st.queue.as_ref()) else {
            continue;
        };
        unsafe {
            if let Some(t) = token {
                let _ = sample.SetUnknown(&MFSampleExtension_Token, &t.0);
            }
            let _ =
                queue
                    .0
                    .QueueEventParamUnk(MEMediaSample.0 as u32, &GUID::zeroed(), S_OK, &sample);
        }
    }
}

/// One NV12 sample of `last`, black before the first frame at this size.
fn make_sample(f: &Format, last: &mut Vec<u8>) -> windows_core::Result<IMFSample> {
    let len = f.nv12_bytes();
    if last.len() != len {
        // Nothing yet at this size: black.
        last.clear();
        last.resize(f.width as usize * f.height as usize, 16);
        last.resize(len, 128);
    }
    unsafe {
        let buffer = MFCreate2DMediaBuffer(f.width, f.height, NV12, false)?;
        let b2: IMF2DBuffer2 = buffer.cast()?;
        let mut scan0 = std::ptr::null_mut();
        let mut pitch = 0i32;
        let mut start = std::ptr::null_mut();
        let mut len = 0u32;
        b2.Lock2DSize(
            MF2DBuffer_LockFlags_Write,
            &mut scan0,
            &mut pitch,
            &mut start,
            &mut len,
        )?;
        let (w, h, pitch) = (f.width as usize, f.height as usize, pitch as usize);
        let y_plane = scan0;
        let uv_plane = scan0.add(pitch * h);
        for row in 0..h {
            std::ptr::copy_nonoverlapping(last.as_ptr().add(row * w), y_plane.add(row * pitch), w);
        }
        let uv = &last[w * h..];
        for row in 0..h / 2 {
            std::ptr::copy_nonoverlapping(uv.as_ptr().add(row * w), uv_plane.add(row * pitch), w);
        }
        b2.Unlock2D()?;
        buffer.SetCurrentLength(len)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(MFGetSystemTime())?;
        sample.SetSampleDuration(f.frame_duration())?;
        Ok(sample)
    }
}

impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(
        &self,
        dwflags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS,
    ) -> windows_core::Result<IMFMediaEvent> {
        let q = self.queue()?;
        unsafe { q.GetEvent(dwflags.0) }
    }

    fn BeginGetEvent(
        &self,
        pcallback: Ref<IMFAsyncCallback>,
        punkstate: Ref<IUnknown>,
    ) -> windows_core::Result<()> {
        let q = self.queue()?;
        unsafe { q.BeginGetEvent(pcallback.as_ref(), punkstate.as_ref()) }
    }

    fn EndGetEvent(&self, presult: Ref<IMFAsyncResult>) -> windows_core::Result<IMFMediaEvent> {
        let q = self.queue()?;
        unsafe { q.EndGetEvent(presult.as_ref()) }
    }

    fn QueueEvent(
        &self,
        met: u32,
        guid: *const GUID,
        hr: HRESULT,
        pv: *const PROPVARIANT,
    ) -> windows_core::Result<()> {
        let q = self.queue()?;
        unsafe { q.QueueEventParamVar(met, guid, hr, pv) }
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> windows_core::Result<IMFMediaSource> {
        self.source
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> windows_core::Result<IMFStreamDescriptor> {
        self.queue()?;
        Ok(self.sd.clone())
    }

    fn RequestSample(&self, ptoken: Ref<IUnknown>) -> windows_core::Result<()> {
        {
            let mut st = self.shared.state.lock().unwrap();
            if st.queue.is_none() {
                return Err(MF_E_SHUTDOWN.into());
            }
            if st.state != MF_STREAM_STATE_RUNNING {
                return Err(MF_E_INVALIDREQUEST.into());
            }
            st.pending
                .push_back(ptoken.as_ref().map(|t| Agile(t.clone())));
        }
        self.shared.wake.notify_all();
        Ok(())
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> windows_core::Result<()> {
        match value {
            MF_STREAM_STATE_RUNNING => self.run(true),
            MF_STREAM_STATE_PAUSED => {
                let st = self.shared.state.lock().unwrap().state;
                if st != MF_STREAM_STATE_RUNNING {
                    return Err(MF_E_INVALID_STATE_TRANSITION.into());
                }
                self.run(false);
            }
            MF_STREAM_STATE_STOPPED => self.stop(false),
            _ => return Err(E_POINTER.into()),
        }
        Ok(())
    }

    fn GetStreamState(&self) -> windows_core::Result<MF_STREAM_STATE> {
        Ok(self.shared.state.lock().unwrap().state)
    }
}
