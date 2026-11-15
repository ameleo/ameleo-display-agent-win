// SPDX-License-Identifier: GPL-3.0-or-later

//! Small Win32 helpers shared by the modules.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

/// NUL-terminated UTF-16 copy of `s`.
pub fn wide(s: impl AsRef<OsStr>) -> Vec<u16> {
    s.as_ref().encode_wide().chain(std::iter::once(0)).collect()
}

/// UTF-16 slice (without NUL) to String, lossy.
pub fn from_wide(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

pub fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// Owned kernel handle, closed on drop.
#[derive(Debug)]
pub struct OwnedHandle(pub HANDLE);

// Kernel handles can be used from any thread.
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

impl OwnedHandle {
    pub fn new(h: HANDLE) -> io::Result<Self> {
        if h.is_null() || h == INVALID_HANDLE_VALUE {
            Err(last_error())
        } else {
            Ok(Self(h))
        }
    }

    pub fn raw(&self) -> HANDLE {
        self.0
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Win32 event object.
#[derive(Debug)]
pub struct Event(OwnedHandle);

impl Event {
    pub fn new(manual_reset: bool) -> io::Result<Self> {
        let h = unsafe { CreateEventW(std::ptr::null(), manual_reset as i32, 0, std::ptr::null()) };
        Ok(Self(OwnedHandle::new(h)?))
    }

    pub fn set(&self) {
        unsafe { SetEvent(self.0.raw()) };
    }

    pub fn raw(&self) -> HANDLE {
        self.0.raw()
    }
}
