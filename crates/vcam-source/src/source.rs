// SPDX-License-Identifier: GPL-3.0-only

//! The media source: one video stream, NV12, formats from the agent.

use std::sync::{Arc, Mutex};

use vcam_shm::{Format, Shm};
use windows::Win32::Foundation::{E_POINTER, S_OK};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, IKsControl_Impl, KSCAMERAPROFILE_Legacy, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{BOOL, ComObject, GUID, HRESULT, IUnknown, Interface, Ref, implement, w};

use crate::stream::Stream;

use crate::controls::E_SET_NOT_FOUND;

const NV12_FOURCC: u32 = u32::from_le_bytes(*b"NV12");

/// Shown when the agent has not set the slot up (or is gone).
const FALLBACK: Format = Format {
    width: 640,
    height: 480,
    fps_num: 30,
    fps_den: 1,
};

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl)]
pub struct Source {
    formats: Vec<Format>,
    /// The slot's shared memory (`None`: the agent has not set it up).
    shm: Option<Arc<Shm>>,
    attributes: IMFAttributes,
    stream_attributes: IMFAttributes,
    state: Mutex<State>,
}

struct State {
    queue: Option<IMFMediaEventQueue>,
    pd: Option<IMFPresentationDescriptor>,
    stream: Option<ComObject<Stream>>,
}

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

fn media_type(f: &Format) -> windows_core::Result<IMFMediaType> {
    unsafe {
        let mt = MFCreateMediaType()?;
        mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        mt.SetUINT64(&MF_MT_FRAME_SIZE, pack(f.width, f.height))?;
        mt.SetUINT64(&MF_MT_FRAME_RATE, pack(f.fps_num, f.fps_den))?;
        mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        mt.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        mt.SetUINT32(&MF_MT_DEFAULT_STRIDE, f.width)?;
        mt.SetUINT32(&MF_MT_SAMPLE_SIZE, f.nv12_bytes() as u32)?;
        Ok(mt)
    }
}

/// Attributes every Frame Server custom source stream must carry.
fn set_stream_attributes(a: &IMFAttributes) -> windows_core::Result<()> {
    unsafe {
        a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
        a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
        a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
        a.SetUINT32(
            &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
            MFFrameSourceTypes_Color.0 as u32,
        )?;
    }
    Ok(())
}

fn new_attributes(size: u32) -> windows_core::Result<IMFAttributes> {
    let mut a = None;
    unsafe { MFCreateAttributes(&mut a, size)? };
    a.ok_or_else(|| E_POINTER.into())
}

impl Source {
    /// Builds the source of `slot`. Heavy work is left to Start: Frame
    /// Server also creates sources only to read their descriptors.
    /// `activate`: attributes Frame Server set on the activation object,
    /// copied into the source attributes.
    pub fn create(slot: u32, activate: Option<&IMFAttributes>) -> windows_core::Result<IUnknown> {
        let shm = match Shm::open(slot) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                crate::log(&format!(
                    "source of slot {slot}: shared memory not available: {e}"
                ));
                None
            }
        };
        crate::log(&format!(
            "source of slot {slot} created, controls: VideoProcAmp {}, CameraControl {}",
            shm.as_ref()
                .is_some_and(|s| s.has_control_set(vcam_shm::control::VIDEO_PROC_AMP)),
            shm.as_ref()
                .is_some_and(|s| s.has_control_set(vcam_shm::control::CAMERA_CONTROL)),
        ));
        let mut formats = shm.as_ref().map(|s| s.formats()).unwrap_or_default();
        if formats.is_empty() {
            formats.push(FALLBACK);
        }

        let types: Vec<Option<IMFMediaType>> = formats
            .iter()
            .map(|f| media_type(f).map(Some))
            .collect::<windows_core::Result<_>>()?;
        let sd = unsafe { MFCreateStreamDescriptor(0, &types)? };
        unsafe {
            sd.GetMediaTypeHandler()?
                .SetCurrentMediaType(types[0].as_ref())?
        };
        set_stream_attributes(&sd.cast()?)?;
        let pd = unsafe { MFCreatePresentationDescriptor(Some(&[Some(sd.clone())]))? };
        unsafe { pd.SelectStream(0)? };

        let attributes = new_attributes(2)?;
        if let Some(a) = activate {
            unsafe { a.CopyAllItems(&attributes)? };
        }
        unsafe {
            let profiles = MFCreateSensorProfileCollection()?;
            let legacy = MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, w!(""))?;
            legacy.AddProfileFilter(0, w!("((RES==;FRT<=30,1;SUT==))"))?;
            profiles.AddProfile(&legacy)?;
            attributes.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &profiles)?;
        }
        let stream_attributes = new_attributes(4)?;
        set_stream_attributes(&stream_attributes)?;

        let source = ComObject::new(Source {
            formats: formats.clone(),
            shm: shm.clone(),
            attributes,
            stream_attributes,
            state: Mutex::new(State {
                queue: Some(unsafe { MFCreateEventQueue()? }),
                pd: Some(pd),
                stream: None,
            }),
        });
        let ex: IMFMediaSourceEx = source.to_interface();
        let stream = Stream::new(ex.cast()?, sd, formats, shm)?;
        source.state.lock().unwrap().stream = Some(stream);
        Ok(source.to_interface())
    }

    fn queue(&self) -> windows_core::Result<IMFMediaEventQueue> {
        self.state
            .lock()
            .unwrap()
            .queue
            .clone()
            .ok_or_else(|| MF_E_SHUTDOWN.into())
    }

    /// The format an app picked, by size and rate.
    fn format_index(&self, mt: &IMFMediaType) -> usize {
        let (Ok(size), Ok(rate)) = (unsafe { mt.GetUINT64(&MF_MT_FRAME_SIZE) }, unsafe {
            mt.GetUINT64(&MF_MT_FRAME_RATE)
        }) else {
            return 0;
        };
        self.formats
            .iter()
            .position(|f| pack(f.width, f.height) == size && pack(f.fps_num, f.fps_den) == rate)
            .or_else(|| {
                self.formats
                    .iter()
                    .position(|f| pack(f.width, f.height) == size)
            })
            .unwrap_or(0)
    }
}

impl IMFMediaEventGenerator_Impl for Source_Impl {
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

impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> windows_core::Result<u32> {
        self.queue()?;
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> windows_core::Result<IMFPresentationDescriptor> {
        let pd = self.state.lock().unwrap().pd.clone().ok_or(MF_E_SHUTDOWN)?;
        unsafe { pd.Clone() }
    }

    fn Start(
        &self,
        pd: Ref<IMFPresentationDescriptor>,
        _: *const GUID,
        _: *const PROPVARIANT,
    ) -> windows_core::Result<()> {
        let (queue, stream) = {
            let st = self.state.lock().unwrap();
            (
                st.queue.clone().ok_or(MF_E_SHUTDOWN)?,
                st.stream.clone().ok_or(MF_E_SHUTDOWN)?,
            )
        };
        let pd = pd.ok()?;
        let mut selected = BOOL(0);
        let mut sd = None;
        unsafe { pd.GetStreamDescriptorByIndex(0, &mut selected, &mut sd)? };
        if selected.as_bool() {
            let sd = sd.ok_or(E_POINTER)?;
            let mt = unsafe { sd.GetMediaTypeHandler()?.GetCurrentMediaType()? };
            let index = self.format_index(&mt);
            let event = if stream.is_active() {
                MEUpdatedStream
            } else {
                MENewStream
            };
            let unk: IUnknown = stream.to_interface();
            unsafe { queue.QueueEventParamUnk(event.0 as u32, &GUID::zeroed(), S_OK, &unk)? };
            stream.start(index)?;
        } else {
            stream.stop(true);
        }
        let now = PROPVARIANT::from(unsafe { MFGetSystemTime() });
        unsafe { queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID::zeroed(), S_OK, &now) }
    }

    fn Stop(&self) -> windows_core::Result<()> {
        let (queue, stream) = {
            let st = self.state.lock().unwrap();
            (st.queue.clone().ok_or(MF_E_SHUTDOWN)?, st.stream.clone())
        };
        if let Some(s) = stream {
            s.stop(true);
        }
        let now = PROPVARIANT::from(unsafe { MFGetSystemTime() });
        unsafe { queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID::zeroed(), S_OK, &now) }
    }

    /// Frame Server sources do not pause.
    fn Pause(&self) -> windows_core::Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> windows_core::Result<()> {
        let (queue, stream) = {
            let mut st = self.state.lock().unwrap();
            st.pd = None;
            (st.queue.take(), st.stream.take())
        };
        if let Some(s) = stream {
            s.shutdown();
        }
        if let Some(q) = queue {
            unsafe { q.Shutdown()? };
        }
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    /// The same object every time, never a copy (Frame Server writes to it).
    fn GetSourceAttributes(&self) -> windows_core::Result<IMFAttributes> {
        Ok(self.attributes.clone())
    }

    fn GetStreamAttributes(&self, id: u32) -> windows_core::Result<IMFAttributes> {
        if id != 0 {
            return Err(MF_E_INVALIDSTREAMNUMBER.into());
        }
        Ok(self.stream_attributes.clone())
    }

    /// System memory frames only.
    fn SetD3DManager(&self, _: Ref<IUnknown>) -> windows_core::Result<()> {
        Ok(())
    }
}

impl IMFGetService_Impl for Source_Impl {
    fn GetService(
        &self,
        _: *const GUID,
        _: *const GUID,
        _: *mut *mut core::ffi::c_void,
    ) -> windows_core::Result<()> {
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IKsControl_Impl for Source_Impl {
    /// Camera controls (see [`crate::controls`]).
    fn KsProperty(
        &self,
        property: *const KSIDENTIFIER,
        property_len: u32,
        data: *mut core::ffi::c_void,
        data_len: u32,
        returned: *mut u32,
    ) -> windows_core::Result<()> {
        unsafe {
            crate::controls::ks_property(
                self.shm.as_deref(),
                property.cast(),
                property_len,
                data.cast(),
                data_len,
                returned,
            )
        }
    }

    fn KsMethod(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> windows_core::Result<()> {
        Err(E_SET_NOT_FOUND.into())
    }

    fn KsEvent(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut core::ffi::c_void,
        _: u32,
        _: *mut u32,
    ) -> windows_core::Result<()> {
        Err(E_SET_NOT_FOUND.into())
    }
}

pub(crate) const NV12: u32 = NV12_FOURCC;
