// SPDX-License-Identifier: GPL-3.0-only

//! Host files pasted in the guest as virtual files, the way rdpclip does it.
//!
//! When the host announces a file list, an `IDataObject` goes on the
//! clipboard with `FileGroupDescriptorW` + `FileContents` (one `IStream` per
//! file). Explorer pastes it like any virtual file source: it reads the
//! descriptor, creates the tree and pulls each file through its stream, with
//! its own progress dialog. Nothing is staged on the guest disk.
//!
//! The object lives in the clipboard helper, which runs as the user (see
//! [`super::helper`]), on its own STA thread: Explorer's calls are
//! marshaled to it and block it while the host answers.
//!
//! MS-RDPECLIP flow kept as is: Lock Clipboard Data before the file list is
//! requested, File Contents Requests with that clipDataId, Unlock when the
//! data object goes away.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::file_contents::{RANGE, SIZE};
use proto::mime;
use windows::Win32::Foundation::{
    DV_E_FORMATETC, DV_E_LINDEX, DV_E_TYMED, E_FAIL, E_NOTIMPL, E_POINTER, FILETIME, HANDLE,
    HGLOBAL, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION,
};
use windows::Win32::System::Com::IBindCtx;
use windows::Win32::System::Com::{
    COWAIT_DISPATCH_CALLS, COWAIT_DISPATCH_WINDOW_MESSAGES, CoTaskMemAlloc,
    CoWaitForMultipleHandles, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject,
    IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl,
    LOCKTYPE, STATFLAG, STATFLAG_NONAME, STATSTG, STGC, STGMEDIUM, STGTY_STREAM, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::Ole::{OleInitialize, OleSetClipboard, ReleaseStgMedium};
use windows::Win32::UI::Shell::{
    IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
};
use windows_core::{BOOL, HRESULT, Interface, PWSTR, Ref, implement};
use windows_sys::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE,
    PeekMessageW, QS_ALLINPUT, TranslateMessage,
};

use super::call::{HostCall, HostReply, wake_on_replies};
use super::filelist::{self, Entry};
use super::formats;
use super::helper::Calls;
use crate::win::{Event, wide};

/// Wait on one host call. Backstop only: the service answers every call
/// within its own timeout.
const HOST_CALL_TIMEOUT: Duration =
    crate::host::HOST_CALL_TIMEOUT.saturating_add(Duration::from_secs(10));
const DROPEFFECT_COPY: u32 = 1;
/// `E_PENDING`: data not available yet, try again.
const E_PENDING: HRESULT = HRESULT(0x8000_000A_u32 as i32);

pub enum OleCmd {
    /// The host owns the clipboard with a file list (and maybe more).
    Offer { serial: u32, mimes: Vec<String> },
    /// The host gave the clipboard up.
    Clear,
}

/// The OLE thread of the data object.
#[derive(Clone)]
pub struct OleHandle {
    tx: std_mpsc::Sender<OleCmd>,
    wake: Arc<Event>,
}

impl OleHandle {
    pub fn send(&self, cmd: OleCmd) {
        if self.tx.send(cmd).is_ok() {
            self.wake.set();
        }
    }
}

/// Blocking calls to the host, through the pipe to the service.
#[derive(Clone)]
pub struct HostLink(pub Arc<Calls>);

impl HostLink {
    fn call(&self, call: HostCall) -> HostReply {
        wait(self.start(call)?)
    }

    /// Sends the call, the answer comes on the returned receiver.
    fn start(&self, call: HostCall) -> Result<std_mpsc::Receiver<HostReply>, String> {
        self.0.start(call)
    }

    /// For Unlock: nobody waits for the answer.
    fn post(&self, call: HostCall) {
        let _ = self.start(call);
    }
}

thread_local! {
    /// Set on the OLE thread: the event its host answers set (see
    /// [`wake_on_replies`]), waited on with
    /// `CoWaitForMultipleHandles`.
    static PUMP_EVENT: std::cell::Cell<Option<HANDLE>> = const { std::cell::Cell::new(None) };
}

/// Waits for a host answer.
///
/// On the OLE (STA) thread the wait keeps dispatching COM calls and window
/// messages, so every other clipboard client (Explorer refreshing its Paste
/// button, WM_RENDERFORMAT, another stream) is served while a file chunk is
/// on its way. A blocking wait would stall them all for the whole copy.
fn wait(rx: std_mpsc::Receiver<HostReply>) -> HostReply {
    let Some(ev) = PUMP_EVENT.get() else {
        return rx
            .recv_timeout(HOST_CALL_TIMEOUT)
            .map_err(|_| "host did not answer".to_string())?;
    };
    let deadline = std::time::Instant::now() + HOST_CALL_TIMEOUT;
    let flags = (COWAIT_DISPATCH_CALLS.0 | COWAIT_DISPATCH_WINDOW_MESSAGES.0) as u32;
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(std_mpsc::TryRecvError::Disconnected) => return Err("service gone".into()),
            Err(std_mpsc::TryRecvError::Empty) => {}
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err("host did not answer".into());
        }
        // Dispatches what comes in until an answer (to this call or another
        // one in flight: the loop checks) sets the event, or the deadline.
        let ms = u32::try_from(left.as_millis()).unwrap_or(u32::MAX).max(1);
        let _ = unsafe { CoWaitForMultipleHandles(flags, ms, &[ev]) };
    }
}

/// Read-ahead settings.
///
/// The file data often travels from the client to the host on the same
/// connection as the client's input: everything requested and not yet
/// received delays the client's mouse and keyboard by its transfer time.
/// The default keeps ~128 KiB in flight, about 1 s on a 1 Mbit/s uplink,
/// and still hides the round trip on a fast link. Requests also queue at
/// the client, so `depth x chunk / uplink rate` must stay under the
/// host's own per-request timeout.
///
/// `AMELEO_DISPLAY_PRO_FILE_CHUNK_KB` and `AMELEO_DISPLAY_PRO_FILE_DEPTH` override
/// it (tuning, diagnostics).
#[derive(Clone, Copy, Debug)]
struct ReadAheadConfig {
    chunk: u32,
    depth: usize,
}

impl ReadAheadConfig {
    const DEFAULT: Self = Self {
        chunk: 64 * 1024,
        depth: 2,
    };

    fn get() -> Self {
        static CONFIG: std::sync::OnceLock<ReadAheadConfig> = std::sync::OnceLock::new();
        *CONFIG.get_or_init(|| {
            let env = |name: &str| {
                std::env::var(name)
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
            };
            let chunk = env("AMELEO_DISPLAY_PRO_FILE_CHUNK_KB")
                .map(|kb| (kb.clamp(4, 4096) * 1024) as u32)
                .unwrap_or(Self::DEFAULT.chunk);
            let depth = env("AMELEO_DISPLAY_PRO_FILE_DEPTH")
                .map(|d| d.clamp(1, 16) as usize)
                .unwrap_or(Self::DEFAULT.depth);
            let c = Self { chunk, depth };
            tracing::info!(
                chunk_kb = c.chunk / 1024,
                depth = c.depth,
                "file paste read-ahead"
            );
            c
        })
    }
}

/// Sequential reader over File Contents Requests with read-ahead.
struct ReadAhead {
    config: ReadAheadConfig,
    size: u64,
    /// Next byte the caller reads.
    pos: u64,
    /// Bytes received, starting at `buf_start`.
    buf: Vec<u8>,
    buf_start: u64,
    /// Requests sent, in position order: (position, length, answer).
    inflight: std::collections::VecDeque<(u64, u32, std_mpsc::Receiver<HostReply>)>,
    next_fetch: u64,
    started: Option<std::time::Instant>,
}

impl ReadAhead {
    fn new(size: u64, config: ReadAheadConfig) -> Self {
        Self {
            config,
            size,
            pos: 0,
            buf: Vec::new(),
            buf_start: 0,
            inflight: Default::default(),
            next_fetch: 0,
            started: None,
        }
    }

    /// Fills `out` from `pos`, returns the byte count (short at EOF).
    /// `fetch(position, length)` sends one request.
    fn read(
        &mut self,
        out: &mut [u8],
        fetch: &dyn Fn(u64, u32) -> Result<std_mpsc::Receiver<HostReply>, String>,
    ) -> Result<usize, String> {
        self.started.get_or_insert_with(std::time::Instant::now);
        let mut done = 0;
        while done < out.len() && self.pos < self.size {
            let buf_end = self.buf_start + self.buf.len() as u64;
            if (self.buf_start..buf_end).contains(&self.pos) {
                let off = (self.pos - self.buf_start) as usize;
                let n = (self.buf.len() - off).min(out.len() - done);
                out[done..done + n].copy_from_slice(&self.buf[off..off + n]);
                done += n;
                self.pos += n as u64;
                continue;
            }
            // The window must start where the caller reads (first read,
            // seek): otherwise start over from there.
            if self.inflight.front().map(|f| f.0) != Some(self.pos) {
                self.inflight.clear();
                self.next_fetch = self.pos;
            }
            while self.inflight.len() < self.config.depth && self.next_fetch < self.size {
                let len = (self.size - self.next_fetch).min(u64::from(self.config.chunk)) as u32;
                self.inflight
                    .push_back((self.next_fetch, len, fetch(self.next_fetch, len)?));
                self.next_fetch += u64::from(len);
            }
            let (at, len, rx) = self
                .inflight
                .pop_front()
                .expect("pos < size, window not empty");
            let (_, data) = wait(rx)?;
            if data.len() < len as usize {
                // Short answer: what was asked after it starts past a hole.
                self.inflight.clear();
                self.next_fetch = at + data.len() as u64;
                if data.is_empty() {
                    break; // host EOF before the announced size
                }
            }
            self.buf = data;
            self.buf_start = at;
        }
        Ok(done)
    }

    fn seek(&mut self, pos: u64) {
        self.pos = pos;
    }

    /// Throughput of a file read to the end, for the log.
    fn finished(&self) -> Option<(u64, std::time::Duration)> {
        (self.pos >= self.size).then(|| {
            (
                self.size,
                self.started.map(|s| s.elapsed()).unwrap_or_default(),
            )
        })
    }
}

/// Registered formats used for virtual files.
#[derive(Clone, Copy)]
struct Formats {
    descriptor: u16,
    contents: u16,
    drop_effect: u16,
    png: u32,
}

fn register(name: &str) -> u16 {
    unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) as u16 }
}

/// The OLE thread of the data object.
pub fn spawn(host: HostLink) -> std::io::Result<OleHandle> {
    let wake = Arc::new(Event::new(false)?);
    let (tx, rx) = std_mpsc::channel();
    let w = wake.clone();
    std::thread::Builder::new()
        .name("ole-clipboard".into())
        .spawn(move || run(rx, &w, host))?;
    Ok(OleHandle { tx, wake })
}

fn run(rx: std_mpsc::Receiver<OleCmd>, wake: &Event, host: HostLink) {
    if let Err(e) = unsafe { OleInitialize(None) } {
        tracing::error!("OleInitialize failed, no file paste from the host: {e}");
        return;
    }
    // Auto-reset: one wake per answer; a stale one costs a loop turn.
    let Ok(pump) = Event::new(false) else { return };
    let pump = Arc::new(pump);
    PUMP_EVENT.set(Some(HANDLE(pump.raw())));
    wake_on_replies(pump.clone());
    let formats = Formats {
        descriptor: register("FileGroupDescriptorW"),
        contents: register("FileContents"),
        drop_effect: register("Preferred DropEffect"),
        png: formats::png_format(),
    };
    // Ids unique across processes: a restarted helper must not reuse the
    // ids of locks the previous one still releases.
    let lock_ids = AtomicU32::new(std::process::id().wrapping_mul(0x0001_0000));
    let mut current: Option<IDataObject> = None;
    loop {
        unsafe {
            let handles = [wake.raw()];
            MsgWaitForMultipleObjectsEx(
                1,
                handles.as_ptr(),
                u32::MAX,
                QS_ALLINPUT,
                MWMO_INPUTAVAILABLE,
            );
            let mut msg: MSG = std::mem::zeroed();
            while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        for cmd in rx.try_iter() {
            match cmd {
                OleCmd::Offer { serial, mimes } => {
                    let lock_id = lock_ids.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                    let obj: IDataObject =
                        DataObject::new(serial, &mimes, formats, lock_id, host.clone()).into();
                    match unsafe { OleSetClipboard(&obj) } {
                        Ok(()) => {
                            tracing::info!(serial, ?mimes, "host files offered as virtual files");
                            current = Some(obj);
                        }
                        Err(e) => tracing::warn!("OleSetClipboard failed: {e}"),
                    }
                }
                OleCmd::Clear => {
                    // S_FALSE (another app copied since) maps to Ok in the
                    // windows crate: compare the raw HRESULT.
                    if let Some(obj) = current.take()
                        && unsafe {
                            windows_sys::Win32::System::Ole::OleIsCurrentClipboard(obj.as_raw())
                        } == S_OK.0
                        && let Err(e) = unsafe { OleSetClipboard(None) }
                    {
                        // The agent empties the clipboard right after
                        // (a host grab of text or images): nothing lost.
                        tracing::debug!("clearing the OLE clipboard: {e}");
                    }
                }
            }
        }
    }
}

/// The host file list for one grab, fetched once.
struct HostList {
    blob: Vec<u8>,
    entries: Vec<Entry>,
}

/// `IDataObjectAsyncCapability`: Explorer pastes on a background thread with
/// its progress dialog, not on its window thread, so a big file streamed
/// from the host does not hold its window.
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct DataObject {
    serial: u32,
    host_mimes: Vec<String>,
    formats: Formats,
    /// Every format offered (FORMATETC list for EnumFormatEtc).
    offered: Vec<FORMATETC>,
    lock_id: u32,
    /// [`LOCK_NONE`], [`LOCK_PENDING`] or [`LOCK_HELD`].
    locked: AtomicU8,
    in_operation: AtomicBool,
    list: Mutex<Option<Arc<HostList>>>,
    host: HostLink,
}

impl DataObject {
    fn new(serial: u32, mimes: &[String], formats: Formats, lock_id: u32, host: HostLink) -> Self {
        let fmt = |cf: u16, tymed: i32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: tymed as u32,
        };
        let mut offered = vec![
            fmt(formats.descriptor, TYMED_HGLOBAL.0),
            fmt(formats.contents, TYMED_ISTREAM.0),
            fmt(formats.drop_effect, TYMED_HGLOBAL.0),
        ];
        // Text or images announced with the files.
        for cf in formats::formats_for(mimes, formats.png) {
            offered.push(fmt(cf as u16, TYMED_HGLOBAL.0));
        }
        Self {
            serial,
            host_mimes: mimes.to_vec(),
            formats,
            offered,
            lock_id,
            locked: AtomicU8::new(LOCK_NONE),
            in_operation: AtomicBool::new(false),
            list: Mutex::new(None),
            host,
        }
    }

    /// Locks the host content (first use only) and fetches the file list.
    /// A failure is not kept: the next paste tries again.
    /// No lock is held while waiting for the host: the wait dispatches
    /// incoming COM calls, which may come back here.
    fn list(&self) -> Result<Arc<HostList>, String> {
        if let Some(list) = &*self.list.lock().unwrap() {
            return Ok(list.clone());
        }
        // Streams cite `lock_id`: only once the host has confirmed it. A call
        // coming back here while the Lock is on its way is refused, rather
        // than citing a lock not placed yet (or never, if it fails).
        match self.locked.compare_exchange(
            LOCK_NONE,
            LOCK_PENDING,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                if let Err(e) = self.host.call(HostCall::Lock(self.lock_id)) {
                    self.locked.store(LOCK_NONE, Ordering::Release);
                    return Err(e);
                }
                self.locked.store(LOCK_HELD, Ordering::Release);
            }
            Err(LOCK_PENDING) => {
                return Err("the host has not confirmed the clipboard lock yet".into());
            }
            Err(_) => {}
        }
        let (m, blob) = self.host.call(HostCall::Request {
            serial: self.serial,
            mimes: vec![mime::FILE_LIST.to_string()],
        })?;
        if m != mime::FILE_LIST {
            return Err(format!("host answered {m} for a file list"));
        }
        let entries = filelist::decode(&blob)?;
        let mut blob = blob;
        let cleaned = filelist::clean_attributes(&mut blob);
        if cleaned > 0 {
            tracing::info!(
                serial = self.serial,
                cleaned,
                "storage attributes dropped (cloud placeholders, reparse points...)"
            );
        }
        let s = filelist::summary(&entries);
        tracing::info!(
            serial = self.serial,
            files = entries.len(),
            dirs = s.dirs,
            nested = s.nested,
            duplicates = s.duplicates,
            sample = ?s.sample,
            "host file list received"
        );
        if s.duplicates > 0 {
            tracing::warn!(
                serial = self.serial,
                duplicates = s.duplicates,
                "the host file list has duplicate names: Explorer will ask to replace or skip \
                 (folder paths flattened on the way?)"
            );
        }
        let list = Arc::new(HostList { blob, entries });
        Ok(self.list.lock().unwrap().get_or_insert(list).clone())
    }

    fn supports(&self, f: &FORMATETC) -> HRESULT {
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_FORMATETC;
        }
        match self.offered.iter().find(|o| o.cfFormat == f.cfFormat) {
            None => DV_E_FORMATETC,
            Some(o) if o.tymed & f.tymed == 0 => DV_E_TYMED,
            Some(_) => S_OK,
        }
    }
}

impl Drop for DataObject {
    fn drop(&mut self) {
        if self.locked.load(Ordering::Acquire) == LOCK_HELD {
            self.host.post(HostCall::Unlock(self.lock_id));
        }
    }
}

fn hglobal_medium(bytes: &[u8]) -> windows_core::Result<STGMEDIUM> {
    let h = formats::global_from(bytes).map_err(|_| windows_core::Error::from(E_FAIL))?;
    let mut m = STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        ..Default::default()
    };
    m.u.hGlobal = HGLOBAL(h);
    Ok(m)
}

fn fail(e: String) -> windows_core::Error {
    tracing::warn!("virtual file paste: {e}");
    windows_core::Error::new(E_FAIL, e)
}

impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows_core::Result<STGMEDIUM> {
        let f = unsafe { pformatetcin.as_ref() }.ok_or(windows_core::Error::from(E_POINTER))?;
        self.get_data(f)
    }

    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        match unsafe { pformatetc.as_ref() } {
            Some(f) => self.supports(f),
            None => E_POINTER,
        }
    }

    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, pformatetcout: *mut FORMATETC) -> HRESULT {
        if let Some(out) = unsafe { pformatetcout.as_mut() } {
            out.ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    /// Explorer reports "Performed DropEffect", "Paste Succeeded"... here.
    /// Accepted and dropped.
    fn SetData(
        &self,
        _: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        frelease: BOOL,
    ) -> windows_core::Result<()> {
        if frelease.as_bool() && !pmedium.is_null() {
            unsafe { ReleaseStgMedium(pmedium as *mut STGMEDIUM) };
        }
        Ok(())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows_core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.offered) }
    }

    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Ref<IAdviseSink>,
    ) -> windows_core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _: u32) -> windows_core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows_core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

impl DataObject {
    /// Size of host file `lindex` from the host (FILECONTENTS_SIZE). A
    /// descriptor may leave FD_FILESIZE out (MS-RDPECLIP 2.2.5.2.3.1): its
    /// size then reads 0, and without asking the file would paste empty.
    fn host_size(&self, lindex: i32) -> u64 {
        let reply = self.host.call(HostCall::FileContents {
            lindex,
            flags: SIZE,
            position: 0,
            requested: 8,
            clip_data_id: Some(self.lock_id),
        });
        match reply {
            Ok((_, data)) if data.len() >= 8 => u64::from_le_bytes(data[..8].try_into().unwrap()),
            Ok((_, data)) => {
                tracing::warn!(lindex, bytes = data.len(), "short file size reply");
                0
            }
            Err(e) => {
                tracing::warn!(lindex, "file size request failed: {e}");
                0
            }
        }
    }

    fn get_data(&self, f: &FORMATETC) -> windows_core::Result<STGMEDIUM> {
        self.supports(f).ok()?;
        let cf = f.cfFormat;
        if cf == self.formats.descriptor {
            let list = self.list().map_err(fail)?;
            return hglobal_medium(&list.blob);
        }
        if cf == self.formats.contents {
            let list = self.list().map_err(fail)?;
            let entry = usize::try_from(f.lindex)
                .ok()
                .and_then(|i| list.entries.get(i))
                .ok_or(windows_core::Error::from(DV_E_LINDEX))?;
            if entry.is_dir() {
                return Err(DV_E_LINDEX.into());
            }
            let size = if entry.size == 0 {
                self.host_size(f.lindex)
            } else {
                entry.size
            };
            tracing::debug!(lindex = f.lindex, name = %entry.name, size, "file stream");
            let stream: IStream = HostStream {
                lindex: f.lindex,
                size,
                write_time: entry.write_time,
                reader: Mutex::new(ReadAhead::new(size, ReadAheadConfig::get())),
                lock_id: self.lock_id,
                host: self.host.clone(),
            }
            .into();
            let mut m = STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                ..Default::default()
            };
            m.u.pstm = std::mem::ManuallyDrop::new(Some(stream));
            return Ok(m);
        }
        if cf == self.formats.drop_effect {
            return hglobal_medium(&DROPEFFECT_COPY.to_le_bytes());
        }
        // Text / image announced with the files.
        let format = u32::from(cf);
        let mimes = formats::host_mimes_for(format, self.formats.png, &self.host_mimes);
        let (m, data) = self
            .host
            .call(HostCall::Request {
                serial: self.serial,
                mimes,
            })
            .map_err(fail)?;
        let h = formats::to_handle(format, self.formats.png, &m, &data).map_err(fail)?;
        let mut medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        medium.u.hGlobal = HGLOBAL(h);
        Ok(medium)
    }
}

impl IDataObjectAsyncCapability_Impl for DataObject_Impl {
    fn SetAsyncMode(&self, _: BOOL) -> windows_core::Result<()> {
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows_core::Result<BOOL> {
        Ok(true.into())
    }

    fn StartOperation(&self, _: Ref<IBindCtx>) -> windows_core::Result<()> {
        tracing::debug!(serial = self.serial, "async paste started");
        self.in_operation.store(true, Ordering::Release);
        Ok(())
    }

    fn InOperation(&self) -> windows_core::Result<BOOL> {
        Ok(self.in_operation.load(Ordering::Acquire).into())
    }

    fn EndOperation(&self, hresult: HRESULT, _: Ref<IBindCtx>, _: u32) -> windows_core::Result<()> {
        tracing::info!(
            serial = self.serial,
            ?hresult,
            "paste of host files finished"
        );
        self.in_operation.store(false, Ordering::Release);
        Ok(())
    }
}

const LOCK_NONE: u8 = 0;
const LOCK_PENDING: u8 = 1;
const LOCK_HELD: u8 = 2;

/// One host file, read through File Contents Requests.
#[implement(IStream)]
struct HostStream {
    lindex: i32,
    size: u64,
    write_time: u64,
    reader: Mutex<ReadAhead>,
    lock_id: u32,
    host: HostLink,
}

impl ISequentialStream_Impl for HostStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        let out = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        // A call re-entering this stream while it waits for the host
        // (dispatched by the wait) is told to come back later.
        let Ok(mut reader) = self.reader.try_lock() else {
            return E_PENDING;
        };
        let was_done = reader.finished().is_some();
        let fetch = |position: u64, requested: u32| {
            self.host.start(HostCall::FileContents {
                lindex: self.lindex,
                flags: RANGE,
                position,
                requested,
                clip_data_id: Some(self.lock_id),
            })
        };
        let done = match reader.read(out, &fetch) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    lindex = self.lindex,
                    pos = reader.pos,
                    "file contents request failed: {e}"
                );
                return E_FAIL;
            }
        };
        if !was_done && let Some((bytes, took)) = reader.finished() {
            let mbps = bytes as f64 / took.as_secs_f64().max(1e-6) / 1e6;
            tracing::info!(
                lindex = self.lindex,
                bytes,
                ?took,
                "host file streamed ({mbps:.1} MB/s)"
            );
        }
        if let Some(r) = unsafe { pcbread.as_mut() } {
            *r = done as u32;
        }
        if done == out.len() { S_OK } else { S_FALSE }
    }

    fn Write(&self, _: *const core::ffi::c_void, _: u32, _: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for HostStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows_core::Result<()> {
        let Ok(mut reader) = self.reader.try_lock() else {
            return Err(E_PENDING.into());
        };
        let base = match dworigin {
            STREAM_SEEK_SET => 0i128,
            STREAM_SEEK_CUR => reader.pos as i128,
            STREAM_SEEK_END => self.size as i128,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let new = base + dlibmove as i128;
        if new < 0 {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        let new = new as u64;
        reader.seek(new);
        if let Some(p) = unsafe { plibnewposition.as_mut() } {
            *p = new;
        }
        Ok(())
    }

    fn SetSize(&self, _: u64) -> windows_core::Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }

    fn CopyTo(
        &self,
        _: Ref<IStream>,
        _: u64,
        _: *mut u64,
        _: *mut u64,
    ) -> windows_core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(&self, _: &STGC) -> windows_core::Result<()> {
        Ok(())
    }

    fn Revert(&self) -> windows_core::Result<()> {
        Ok(())
    }

    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> windows_core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> windows_core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> windows_core::Result<()> {
        let st = unsafe { pstatstg.as_mut() }.ok_or(windows_core::Error::from(E_POINTER))?;
        *st = STATSTG::default();
        st.r#type = STGTY_STREAM.0 as u32;
        st.cbSize = self.size;
        st.mtime = FILETIME {
            dwLowDateTime: self.write_time as u32,
            dwHighDateTime: (self.write_time >> 32) as u32,
        };
        if *grfstatflag != STATFLAG_NONAME {
            // Callers free it with CoTaskMemFree: an empty name is enough.
            let p = unsafe { CoTaskMemAlloc(2) } as *mut u16;
            if !p.is_null() {
                unsafe { *p = 0 };
            }
            st.pwcsName = PWSTR(p);
        }
        Ok(())
    }

    fn Clone(&self) -> windows_core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const BIG: ReadAheadConfig = ReadAheadConfig {
        chunk: 1024 * 1024,
        depth: 3,
    };

    /// Fake host over `data`, answering at most `cap` bytes per request.
    fn host<'a>(
        data: &'a [u8],
        cap: usize,
        calls: &'a RefCell<Vec<(u64, u32)>>,
    ) -> impl Fn(u64, u32) -> Result<std_mpsc::Receiver<HostReply>, String> + 'a {
        move |pos, len| {
            calls.borrow_mut().push((pos, len));
            let (tx, rx) = std_mpsc::sync_channel(1);
            let start = (pos as usize).min(data.len());
            let end = (start + (len as usize).min(cap)).min(data.len());
            tx.send(Ok((String::new(), data[start..end].to_vec())))
                .unwrap();
            Ok(rx)
        }
    }

    fn file(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 7 % 251) as u8).collect()
    }

    fn read_all(
        r: &mut ReadAhead,
        step: usize,
        fetch: &dyn Fn(u64, u32) -> Result<std_mpsc::Receiver<HostReply>, String>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = vec![0; step];
        loop {
            let n = r.read(&mut buf, fetch).unwrap();
            out.extend_from_slice(&buf[..n]);
            if n < step {
                return out;
            }
        }
    }

    #[test]
    fn small_reads_use_big_requests() {
        let data = file(3 * 1024 * 1024 + 123);
        let calls = RefCell::new(Vec::new());
        let fetch = host(&data, usize::MAX, &calls);
        let mut r = ReadAhead::new(data.len() as u64, BIG);
        // Explorer style 4 KiB reads.
        assert_eq!(read_all(&mut r, 4096, &fetch), data);
        drop(fetch);
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 4, "{calls:?}");
        assert!(calls.iter().all(|&(_, len)| len <= BIG.chunk));
        assert!(r.finished().is_some());
    }

    #[test]
    fn default_keeps_little_in_flight() {
        let data = file(10 * 1024 * 1024);
        let calls = RefCell::new(Vec::new());
        let fetch = host(&data, usize::MAX, &calls);
        let mut r = ReadAhead::new(data.len() as u64, ReadAheadConfig::DEFAULT);
        let mut buf = vec![0; 4096];
        r.read(&mut buf, &fetch).unwrap();
        drop(fetch);
        let asked: u64 = calls
            .into_inner()
            .iter()
            .map(|&(_, len)| u64::from(len))
            .sum();
        // The client's uplink carries its input too: keep it short.
        assert!(
            asked <= 128 * 1024,
            "{asked} bytes requested for a 4 KiB read"
        );
    }

    #[test]
    fn short_answers_are_completed() {
        let data = file(2_500_000);
        let calls = RefCell::new(Vec::new());
        // The host answers 100 000 bytes at most.
        let fetch = host(&data, 100_000, &calls);
        let mut r = ReadAhead::new(data.len() as u64, BIG);
        assert_eq!(read_all(&mut r, 65536, &fetch), data);
    }

    #[test]
    fn seek_then_read() {
        let data = file(5_000_000);
        let calls = RefCell::new(Vec::new());
        let fetch = host(&data, usize::MAX, &calls);
        let mut r = ReadAhead::new(data.len() as u64, BIG);
        let mut buf = vec![0; 10];
        r.read(&mut buf, &fetch).unwrap();
        r.seek(4_000_000);
        r.read(&mut buf, &fetch).unwrap();
        assert_eq!(buf, data[4_000_000..4_000_010]);
        r.seek(5);
        r.read(&mut buf, &fetch).unwrap();
        assert_eq!(buf, data[5..15]);
    }

    #[test]
    fn host_eof_before_announced_size() {
        let data = file(1000);
        let calls = RefCell::new(Vec::new());
        let fetch = host(&data, usize::MAX, &calls);
        // Descriptor said 5000, the host has 1000.
        let mut r = ReadAhead::new(5000, BIG);
        assert_eq!(read_all(&mut r, 4096, &fetch), data);
    }
}
