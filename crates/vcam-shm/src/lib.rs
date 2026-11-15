// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared memory between the guest agent and the virtual camera media
//! source (`vcam-source`).
//!
//! The media source runs inside the Frame Server service (LocalService,
//! session 0), the agent in the user session. One slot per virtual camera:
//!
//! * `Global\ameleo-display-agent-vcam-<slot>`: a section with a [`Header`] and a
//!   ring of [`RING`] NV12 frames (tight, stride = width), each as large as
//!   the biggest format of the camera.
//! * `...-frame` event: set by the agent after each frame.
//! * `...-state` event: set by the source when an app starts or stops the
//!   camera ([`Header::requested`], [`Header::streaming`]).
//!
//! The agent creates everything, with a DACL that lets LocalService read
//! and write. Frames are numbered; frame `n` lives in ring entry `n % RING`
//! under a sequence lock (`2n + 1` while written, `2n + 2` once complete),
//! so a reader knows whether the entry still holds the frame it wants.
//! The ring lets the source play frames out at a steady pace even when they
//! arrive in bursts (network jitter, a slow decode).
//!
//! The header also holds the camera controls (brightness, focus...): the
//! agent writes what the client offers and their values, an app's change
//! goes the other way (a counter per control, the `-state` event set).

#![cfg(windows)]

use std::io;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering, fence};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile,
    OpenFileMappingW, PAGE_READWRITE, UnmapViewOfFile,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
};

/// Virtual cameras at the same time.
pub const SLOTS: u32 = 4;

/// COM class of the media source, one per slot: Frame Server activates the
/// source by CLSID, the CLSID tells it which slot it serves.
pub const CLSIDS: [u128; SLOTS as usize] = [
    0x6c6d2a4e_8f3b_4d1a_9e57_3b1f0a7c5d01,
    0x6c6d2a4e_8f3b_4d1a_9e57_3b1f0a7c5d02,
    0x6c6d2a4e_8f3b_4d1a_9e57_3b1f0a7c5d03,
    0x6c6d2a4e_8f3b_4d1a_9e57_3b1f0a7c5d04,
];

pub const MAX_FORMATS: usize = 16;
pub const MAX_WIDTH: u32 = 3840;
pub const MAX_HEIGHT: u32 = 2160;
/// Frames the agent can publish ahead of the source.
pub const RING: u64 = 6;
/// Frames the source keeps buffered by default (jitter absorbed: about
/// `DEFAULT_TARGET - 1` frame periods, latency added: about as much).
pub const DEFAULT_TARGET: u32 = 3;
const HEADER_BYTES: usize = 4096;
const MAGIC: u32 = u32::from_le_bytes(*b"QDAV");
const VERSION: u32 = 3;

/// Camera controls (brightness, focus...) a camera can offer.
pub const MAX_CONTROLS: usize = 16;

/// Control sets and modes, numbered as in MS-RDPECAM (the host passes
/// them through).
pub mod control {
    pub const CAMERA_CONTROL: u8 = 1;
    pub const VIDEO_PROC_AMP: u8 = 2;
    /// `caps` flag and mode.
    pub const MANUAL: u8 = 1;
    pub const AUTO: u8 = 2;
}

/// A control of the camera: what the client says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ControlInfo {
    /// [`control::CAMERA_CONTROL`] or [`control::VIDEO_PROC_AMP`].
    pub set: u8,
    /// MS-RDPECAM property id within the set.
    pub property: u8,
    /// [`control::MANUAL`] | [`control::AUTO`].
    pub caps: u8,
    pub min: i32,
    pub max: i32,
    pub step: i32,
    pub default: i32,
}

#[repr(C)]
struct Control {
    // Written by the agent before `control_count`.
    set: AtomicU32,
    property: AtomicU32,
    caps: AtomicU32,
    min: AtomicI32,
    max: AtomicI32,
    step: AtomicI32,
    default: AtomicI32,
    // Current value: the agent's (from the client), or the source's
    // right after an app set it.
    mode: AtomicU32,
    value: AtomicI32,
    // An app's change, for the agent: `requested` counts them.
    requested: AtomicU32,
    req_mode: AtomicU32,
    req_value: AtomicI32,
}

/// SYSTEM and Administrators (the agent), LocalService (Frame Server).
const SDDL: &str = if cfg!(test) {
    "D:P(A;;GA;;;WD)" // tests: the plain test user reopens what it created
} else {
    "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GA;;;LS)"
};

/// The CLSID of `slot` as "{...}", for the registry and MFCreateVirtualCamera.
pub fn clsid_string(slot: u32) -> String {
    let v = CLSIDS[slot as usize];
    format!(
        "{{{:08X}-{:04X}-{:04X}-{:04X}-{:012X}}}",
        (v >> 96) as u32,
        (v >> 80) as u16,
        (v >> 64) as u16,
        (v >> 48) as u16,
        v & 0xFFFF_FFFF_FFFF
    )
}

/// The slot served by a CLSID.
pub fn slot_of(clsid: u128) -> Option<u32> {
    CLSIDS.iter().position(|&c| c == clsid).map(|i| i as u32)
}

/// One video format offered by the camera. Frames are NV12.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub fps_num: u32,
    pub fps_den: u32,
}

impl Format {
    pub fn nv12_bytes(&self) -> usize {
        (self.width * self.height * 3 / 2) as usize
    }

    /// Frame duration in 100 ns units.
    pub fn frame_duration(&self) -> i64 {
        if self.fps_num == 0 {
            return 333_333;
        }
        10_000_000i64 * i64::from(self.fps_den.max(1)) / i64::from(self.fps_num)
    }

    pub fn fits(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.width <= MAX_WIDTH
            && self.height <= MAX_HEIGHT
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
    }
}

#[repr(C)]
struct Entry {
    seq: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    timestamp: AtomicU64,
}

#[repr(C)]
pub struct Header {
    magic: AtomicU32,
    version: AtomicU32,
    // Written by the agent before the camera is created.
    format_count: AtomicU32,
    formats: [[AtomicU32; 4]; MAX_FORMATS],
    /// Bytes of one ring entry.
    frame_capacity: AtomicU64,
    /// Frames the source keeps buffered before playing them out.
    target: AtomicU32,
    // Written by the source.
    /// Format index an app started the camera with, -1 when stopped.
    pub requested: AtomicI32,
    /// 1 while an app streams.
    pub streaming: AtomicU32,
    // Frames, written by the agent.
    /// Frames published so far; frame `n` is complete once this is > n.
    written: AtomicU64,
    ring: [Entry; RING as usize],
    /// Controls in `controls`, set by the agent after writing them.
    control_count: AtomicU32,
    controls: [Control; MAX_CONTROLS],
}

const _: () = assert!(std::mem::size_of::<Header>() <= HEADER_BYTES);

/// What [`Shm::read_frame`] found for a frame number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Read {
    /// Copied; the frame timestamp (µs).
    Ok(u64),
    /// Not this size (the agent sends another format): skip it.
    OtherSize,
    /// Overwritten by a newer frame: the reader fell behind.
    Overwritten,
    /// Not published yet.
    NotYet,
}

struct Handle(HANDLE);

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

pub struct Shm {
    _section: Handle,
    view: *mut u8,
    capacity: usize,
    frame_event: Handle,
    state_event: Handle,
}

unsafe impl Send for Shm {}
unsafe impl Sync for Shm {}

fn name(slot: u32, suffix: &str) -> Vec<u16> {
    // Tests run without SeCreateGlobalPrivilege.
    let ns = if cfg!(test) { "Local" } else { "Global" };
    format!("{ns}\\ameleo-display-agent-vcam-{slot}{suffix}")
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

fn check(h: HANDLE) -> io::Result<Handle> {
    if h.is_null() || h == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(h))
    }
}

impl Shm {
    /// Agent side: creates the slot objects, with ring entries of
    /// `frame_capacity` bytes (the largest NV12 frame of the camera), and
    /// resets the header. `AlreadyExists` if the slot is still open
    /// somewhere: another slot must be used.
    pub fn create(slot: u32, frame_capacity: usize) -> io::Result<Self> {
        let max = (MAX_WIDTH * MAX_HEIGHT * 3 / 2) as usize;
        if frame_capacity == 0 || frame_capacity > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame size out of range",
            ));
        }
        let size = HEADER_BYTES + RING as usize * frame_capacity;
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let sddl: Vec<u16> = SDDL.encode_utf16().chain(Some(0)).collect();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd,
            bInheritHandle: 0,
        };
        let res = (|| {
            let section = check(unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    &sa,
                    PAGE_READWRITE,
                    (size as u64 >> 32) as u32,
                    size as u32,
                    name(slot, "").as_ptr(),
                )
            })?;
            // Still held by the source of a removed camera (an app keeps it
            // open): its size is the old one and resetting the header would
            // pull it from under that source.
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("camera slot {slot} still in use"),
                ));
            }
            let frame_event =
                check(unsafe { CreateEventW(&sa, 0, 0, name(slot, "-frame").as_ptr()) })?;
            let state_event =
                check(unsafe { CreateEventW(&sa, 0, 0, name(slot, "-state").as_ptr()) })?;
            Self::map(section, size, frame_capacity, frame_event, state_event)
        })();
        unsafe { LocalFree(sd) };
        let shm = res?;
        let h = shm.header();
        h.format_count.store(0, Ordering::Relaxed);
        h.frame_capacity
            .store(frame_capacity as u64, Ordering::Relaxed);
        h.target.store(DEFAULT_TARGET, Ordering::Relaxed);
        h.requested.store(-1, Ordering::Relaxed);
        h.streaming.store(0, Ordering::Relaxed);
        h.written.store(0, Ordering::Relaxed);
        for e in &h.ring {
            e.seq.store(0, Ordering::Relaxed);
        }
        h.control_count.store(0, Ordering::Relaxed);
        for c in &h.controls {
            c.requested.store(0, Ordering::Relaxed);
        }
        h.version.store(VERSION, Ordering::Relaxed);
        h.magic.store(MAGIC, Ordering::Release);
        Ok(shm)
    }

    /// Source side: opens the objects the agent created.
    pub fn open(slot: u32) -> io::Result<Self> {
        let section = check(unsafe {
            OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, name(slot, "").as_ptr())
        })?;
        let access = EVENT_MODIFY_STATE | 0x0010_0000; // SYNCHRONIZE
        let frame_event = check(unsafe { OpenEventW(access, 0, name(slot, "-frame").as_ptr()) })?;
        let state_event = check(unsafe { OpenEventW(access, 0, name(slot, "-state").as_ptr()) })?;
        // Map the header first: it gives the ring size.
        let probe = unsafe { MapViewOfFile(section.0, FILE_MAP_READ, 0, 0, HEADER_BYTES) };
        if probe.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        let (magic, version, capacity) = unsafe {
            let h = &*(probe.Value as *const Header);
            (
                h.magic.load(Ordering::Acquire),
                h.version.load(Ordering::Relaxed),
                h.frame_capacity.load(Ordering::Relaxed) as usize,
            )
        };
        unsafe { UnmapViewOfFile(probe) };
        if magic != MAGIC || version != VERSION || capacity == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "virtual camera slot not initialized (or agent version mismatch)",
            ));
        }
        Self::map(
            section,
            HEADER_BYTES + RING as usize * capacity,
            capacity,
            frame_event,
            state_event,
        )
    }

    fn map(
        section: Handle,
        size: usize,
        capacity: usize,
        frame_event: Handle,
        state_event: Handle,
    ) -> io::Result<Self> {
        let view: MEMORY_MAPPED_VIEW_ADDRESS =
            unsafe { MapViewOfFile(section.0, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, size) };
        if view.Value.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _section: section,
            view: view.Value.cast(),
            capacity,
            frame_event,
            state_event,
        })
    }

    pub fn header(&self) -> &Header {
        unsafe { &*(self.view as *const Header) }
    }

    fn entry_ptr(&self, n: u64) -> *mut u8 {
        unsafe {
            self.view
                .add(HEADER_BYTES + (n % RING) as usize * self.capacity)
        }
    }

    pub fn set_formats(&self, formats: &[Format]) {
        let h = self.header();
        let n = formats.len().min(MAX_FORMATS);
        for (slot, f) in h.formats.iter().zip(&formats[..n]) {
            slot[0].store(f.width, Ordering::Relaxed);
            slot[1].store(f.height, Ordering::Relaxed);
            slot[2].store(f.fps_num, Ordering::Relaxed);
            slot[3].store(f.fps_den, Ordering::Relaxed);
        }
        h.format_count.store(n as u32, Ordering::Release);
    }

    pub fn formats(&self) -> Vec<Format> {
        let h = self.header();
        let n = (h.format_count.load(Ordering::Acquire) as usize).min(MAX_FORMATS);
        h.formats[..n]
            .iter()
            .map(|f| Format {
                width: f[0].load(Ordering::Relaxed),
                height: f[1].load(Ordering::Relaxed),
                fps_num: f[2].load(Ordering::Relaxed),
                fps_den: f[3].load(Ordering::Relaxed),
            })
            .filter(|f| f.fits() && f.nv12_bytes() <= self.capacity)
            .collect()
    }

    /// Agent: publishes the next NV12 frame (tight, stride = width).
    pub fn write_frame(
        &self,
        width: u32,
        height: u32,
        timestamp: u64,
        fill: impl FnOnce(&mut [u8]),
    ) {
        let len = (width as usize) * (height as usize) * 3 / 2;
        if len == 0 || len > self.capacity {
            return;
        }
        let h = self.header();
        let n = h.written.load(Ordering::Relaxed);
        let e = &h.ring[(n % RING) as usize];
        e.seq.store(2 * n + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        e.width.store(width, Ordering::Relaxed);
        e.height.store(height, Ordering::Relaxed);
        e.timestamp.store(timestamp, Ordering::Relaxed);
        fill(unsafe { std::slice::from_raw_parts_mut(self.entry_ptr(n), len) });
        fence(Ordering::Release);
        e.seq.store(2 * n + 2, Ordering::Release);
        h.written.store(n + 1, Ordering::Release);
        unsafe { SetEvent(self.frame_event.0) };
    }

    /// Agent: frames the source keeps buffered (1 to `RING - 2`).
    pub fn set_target(&self, frames: u32) {
        self.header()
            .target
            .store(frames.clamp(1, RING as u32 - 2), Ordering::Release);
    }

    pub fn target(&self) -> u32 {
        self.header()
            .target
            .load(Ordering::Acquire)
            .clamp(1, RING as u32 - 2)
    }

    /// Frames published so far.
    pub fn written(&self) -> u64 {
        self.header().written.load(Ordering::Acquire)
    }

    /// Source: copies frame `n` if it is `width` x `height` and still in
    /// the ring.
    pub fn read_frame(&self, n: u64, width: u32, height: u32, mut out: impl FnMut(&[u8])) -> Read {
        let h = self.header();
        if n >= h.written.load(Ordering::Acquire) {
            return Read::NotYet;
        }
        let e = &h.ring[(n % RING) as usize];
        let done = 2 * n + 2;
        if e.seq.load(Ordering::Acquire) != done {
            return Read::Overwritten;
        }
        if e.width.load(Ordering::Relaxed) != width || e.height.load(Ordering::Relaxed) != height {
            return Read::OtherSize;
        }
        let ts = e.timestamp.load(Ordering::Relaxed);
        let len = (width as usize) * (height as usize) * 3 / 2;
        out(unsafe { std::slice::from_raw_parts(self.entry_ptr(n), len) });
        fence(Ordering::Acquire);
        if e.seq.load(Ordering::Relaxed) == done {
            Read::Ok(ts)
        } else {
            Read::Overwritten
        }
    }

    /// Source: an app started (`Some(format index)`) or stopped the camera.
    pub fn set_stream(&self, requested: Option<usize>) {
        let h = self.header();
        h.requested
            .store(requested.map_or(-1, |i| i as i32), Ordering::Relaxed);
        h.streaming
            .store(requested.is_some() as u32, Ordering::Release);
        unsafe { SetEvent(self.state_event.0) };
    }

    /// Agent: the current request, after `wait_state`.
    pub fn stream(&self) -> Option<usize> {
        let h = self.header();
        if h.streaming.load(Ordering::Acquire) == 0 {
            return None;
        }
        usize::try_from(h.requested.load(Ordering::Relaxed)).ok()
    }

    /// Agent: the camera's controls with their current `(mode, value)`.
    /// Apps' pending changes are dropped.
    pub fn set_controls(&self, controls: &[(ControlInfo, u8, i32)]) {
        let h = self.header();
        h.control_count.store(0, Ordering::Release);
        let n = controls.len().min(MAX_CONTROLS);
        for (c, (info, mode, value)) in h.controls.iter().zip(&controls[..n]) {
            c.set.store(info.set.into(), Ordering::Relaxed);
            c.property.store(info.property.into(), Ordering::Relaxed);
            c.caps.store(info.caps.into(), Ordering::Relaxed);
            c.min.store(info.min, Ordering::Relaxed);
            c.max.store(info.max, Ordering::Relaxed);
            c.step.store(info.step, Ordering::Relaxed);
            c.default.store(info.default, Ordering::Relaxed);
            c.mode.store((*mode).into(), Ordering::Relaxed);
            c.value.store(*value, Ordering::Relaxed);
        }
        h.control_count.store(n as u32, Ordering::Release);
    }

    fn control_entries(&self) -> &[Control] {
        let h = self.header();
        let n = (h.control_count.load(Ordering::Acquire) as usize).min(MAX_CONTROLS);
        &h.controls[..n]
    }

    fn find_control(&self, set: u8, property: u8) -> Option<&Control> {
        self.control_entries().iter().find(|c| {
            c.set.load(Ordering::Relaxed) == u32::from(set)
                && c.property.load(Ordering::Relaxed) == u32::from(property)
        })
    }

    /// Source: whether the camera has controls of `set`.
    pub fn has_control_set(&self, set: u8) -> bool {
        self.control_entries()
            .iter()
            .any(|c| c.set.load(Ordering::Relaxed) == u32::from(set))
    }

    /// Source: a control and its current `(mode, value)`.
    pub fn control(&self, set: u8, property: u8) -> Option<(ControlInfo, u8, i32)> {
        let c = self.find_control(set, property)?;
        let info = ControlInfo {
            set,
            property,
            caps: c.caps.load(Ordering::Relaxed) as u8,
            min: c.min.load(Ordering::Relaxed),
            max: c.max.load(Ordering::Relaxed),
            step: c.step.load(Ordering::Relaxed),
            default: c.default.load(Ordering::Relaxed),
        };
        Some((
            info,
            c.mode.load(Ordering::Relaxed) as u8,
            c.value.load(Ordering::Relaxed),
        ))
    }

    /// Source: an app sets a control. It reads back as set right away; the
    /// agent passes it on (see [`Shm::take_requests`]).
    pub fn request_control(&self, set: u8, property: u8, mode: u8, value: i32) -> bool {
        let Some(c) = self.find_control(set, property) else {
            return false;
        };
        c.mode.store(mode.into(), Ordering::Relaxed);
        c.value.store(value, Ordering::Relaxed);
        c.req_mode.store(mode.into(), Ordering::Relaxed);
        c.req_value.store(value, Ordering::Relaxed);
        c.requested.fetch_add(1, Ordering::Release);
        unsafe { SetEvent(self.state_event.0) };
        true
    }

    /// Agent: what the client says a control is now.
    pub fn update_control(&self, set: u8, property: u8, mode: u8, value: i32) -> bool {
        let Some(c) = self.find_control(set, property) else {
            return false;
        };
        c.mode.store(mode.into(), Ordering::Relaxed);
        c.value.store(value, Ordering::Relaxed);
        true
    }

    /// Agent: the apps' changes since the last call, as
    /// `(set, property, mode, value)` (the last one per control). `seen`
    /// keeps track, one counter per control.
    pub fn take_requests(&self, seen: &mut [u32; MAX_CONTROLS]) -> Vec<(u8, u8, u8, i32)> {
        let mut out = Vec::new();
        for (c, seen) in self.control_entries().iter().zip(seen.iter_mut()) {
            let n = c.requested.load(Ordering::Acquire);
            if n != *seen {
                *seen = n;
                out.push((
                    c.set.load(Ordering::Relaxed) as u8,
                    c.property.load(Ordering::Relaxed) as u8,
                    c.req_mode.load(Ordering::Relaxed) as u8,
                    c.req_value.load(Ordering::Relaxed),
                ));
            }
        }
        out
    }

    /// Agent: waits for a stream state change, at most `timeout_ms`.
    /// Returns true if the source signaled one.
    pub fn wait_state(&self, timeout_ms: u32) -> bool {
        unsafe { WaitForSingleObject(self.state_event.0, timeout_ms) == 0 }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast(),
            })
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clsid_format() {
        assert_eq!(clsid_string(0), "{6C6D2A4E-8F3B-4D1A-9E57-3B1F0A7C5D01}");
        assert_eq!(slot_of(CLSIDS[3]), Some(3));
        assert_eq!(slot_of(0), None);
    }

    #[test]
    fn frame_duration() {
        let f = Format {
            width: 640,
            height: 480,
            fps_num: 30,
            fps_den: 1,
        };
        assert_eq!(f.frame_duration(), 333_333);
        assert_eq!(
            Format {
                fps_num: 30000,
                fps_den: 1001,
                ..f
            }
            .frame_duration(),
            333_666
        );
    }

    #[test]
    fn ring() {
        let f = Format {
            width: 4,
            height: 2,
            fps_num: 30,
            fps_den: 1,
        };
        let agent = Shm::create(3, f.nv12_bytes()).unwrap();
        agent.set_formats(&[
            f,
            Format {
                width: 640,
                height: 480,
                ..f
            },
        ]);
        let source = Shm::open(3).unwrap();
        // Formats larger than the ring entries are not offered.
        assert_eq!(source.formats(), [f]);
        assert_eq!(source.read_frame(0, 4, 2, |_| {}), Read::NotYet);
        for n in 0..8u8 {
            agent.write_frame(4, 2, u64::from(n) * 10, |d| d.fill(n));
        }
        assert_eq!(source.written(), 8);
        // 0 and 1 were overwritten by 6 and 7.
        assert_eq!(source.read_frame(1, 4, 2, |_| {}), Read::Overwritten);
        let mut got = Vec::new();
        assert_eq!(
            source.read_frame(3, 4, 2, |d| got = d.to_vec()),
            Read::Ok(30)
        );
        assert_eq!(got, [3u8; 12]);
        assert_eq!(source.read_frame(5, 2, 2, |_| {}), Read::OtherSize);
        assert_eq!(source.read_frame(8, 4, 2, |_| {}), Read::NotYet);
        assert_eq!(source.target(), DEFAULT_TARGET);
        agent.set_target(99);
        assert_eq!(source.target(), RING as u32 - 2);
    }

    #[test]
    fn controls() {
        use control::*;
        let f = Format {
            width: 4,
            height: 2,
            fps_num: 30,
            fps_den: 1,
        };
        let agent = Shm::create(2, f.nv12_bytes()).unwrap();
        let source = Shm::open(2).unwrap();
        let brightness = ControlInfo {
            set: VIDEO_PROC_AMP,
            property: 2,
            caps: MANUAL,
            min: 0,
            max: 255,
            step: 1,
            default: 128,
        };
        let focus = ControlInfo {
            set: CAMERA_CONTROL,
            property: 2,
            caps: MANUAL | AUTO,
            min: 0,
            max: 100,
            step: 5,
            default: 0,
        };
        assert!(!source.has_control_set(VIDEO_PROC_AMP));
        agent.set_controls(&[(brightness, MANUAL, 100), (focus, AUTO, 40)]);
        assert!(source.has_control_set(VIDEO_PROC_AMP) && source.has_control_set(CAMERA_CONTROL));
        assert_eq!(
            source.control(VIDEO_PROC_AMP, 2),
            Some((brightness, MANUAL, 100))
        );
        assert_eq!(source.control(VIDEO_PROC_AMP, 3), None);

        let mut seen = [0; MAX_CONTROLS];
        assert!(agent.take_requests(&mut seen).is_empty());
        assert!(source.request_control(CAMERA_CONTROL, 2, MANUAL, 60));
        assert!(source.request_control(CAMERA_CONTROL, 2, MANUAL, 70));
        assert_eq!(source.control(CAMERA_CONTROL, 2).unwrap().2, 70);
        assert_eq!(
            agent.take_requests(&mut seen),
            [(CAMERA_CONTROL, 2, MANUAL, 70)]
        );
        assert!(agent.take_requests(&mut seen).is_empty());
        // The client clamped it.
        assert!(agent.update_control(CAMERA_CONTROL, 2, MANUAL, 65));
        assert_eq!(source.control(CAMERA_CONTROL, 2).unwrap().2, 65);
        assert!(!source.request_control(CAMERA_CONTROL, 6, MANUAL, 1));
    }
}
