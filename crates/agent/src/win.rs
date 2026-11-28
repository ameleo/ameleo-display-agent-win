// SPDX-License-Identifier: GPL-3.0-only

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

/// A NUL-terminated UTF-16 string to String, lossy; empty for null.
pub fn pwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let len = (0..).take_while(|&i| unsafe { *p.add(i) } != 0).count();
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(p, len) })
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

/// An instance of the local named pipe `name` (`\\.\pipe\...`), open to
/// the SDDL `sddl`; `first` fails if another process already owns the
/// name. `inbound`: the client only writes.
pub fn pipe_instance(
    name: &str,
    sddl: &str,
    first: bool,
    inbound: bool,
) -> io::Result<std::fs::File> {
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, PIPE_ACCESS_INBOUND,
    };
    use windows_sys::Win32::System::Pipes::{
        CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide(sddl).as_ptr(),
            SDDL_REVISION_1,
            &mut sd,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error());
    }
    let sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd,
        bInheritHandle: 0,
    };
    let access = if inbound {
        PIPE_ACCESS_INBOUND
    } else {
        PIPE_ACCESS_DUPLEX
    };
    let open_mode = access
        | if first {
            FILE_FLAG_FIRST_PIPE_INSTANCE
        } else {
            0
        };
    let h = unsafe {
        CreateNamedPipeW(
            wide(name).as_ptr(),
            open_mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            &sa,
        )
    };
    let err = last_error();
    unsafe { LocalFree(sd as HLOCAL) };
    if h == INVALID_HANDLE_VALUE {
        return Err(err);
    }
    Ok(unsafe { std::fs::File::from_raw_handle(h as RawHandle) })
}

/// Waits for a client on a pipe instance; `false` if none came.
pub fn pipe_accept(pipe: &std::fs::File) -> bool {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{ERROR_PIPE_CONNECTED, GetLastError};
    use windows_sys::Win32::System::Pipes::ConnectNamedPipe;
    unsafe {
        ConnectNamedPipe(pipe.as_raw_handle() as HANDLE, std::ptr::null_mut()) != 0
            || GetLastError() == ERROR_PIPE_CONNECTED
    }
}

/// The process at the other end of a pipe instance.
pub fn pipe_client(pipe: &std::fs::File) -> io::Result<u32> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;
    let mut pid = 0u32;
    if unsafe { GetNamedPipeClientProcessId(pipe.as_raw_handle() as HANDLE, &mut pid) } == 0 {
        return Err(last_error());
    }
    Ok(pid)
}

/// The image path of a process.
pub fn image_path(pid: u32) -> Option<String> {
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    let process =
        OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) }).ok()?;
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    (unsafe {
        QueryFullProcessImageNameW(
            process.raw(),
            PROCESS_NAME_WIN32,
            buf.as_mut_ptr(),
            &mut len,
        )
    } != 0)
        .then(|| String::from_utf16_lossy(&buf[..len as usize]))
}

/// The user a process runs as (`DOMAIN\name` reduced to `name`).
pub fn process_user(pid: u32) -> Option<String> {
    use windows_sys::Win32::Security::{
        GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let process =
        OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) }).ok()?;
    let mut h: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(process.raw(), TOKEN_QUERY, &mut h) } == 0 {
        return None;
    }
    let token = OwnedHandle(h);
    let mut buf = [0u64; 64];
    let mut size = 0u32;
    if unsafe {
        GetTokenInformation(
            token.raw(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            std::mem::size_of_val(&buf) as u32,
            &mut size,
        )
    } == 0
    {
        return None;
    }
    let sid = unsafe { (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid };
    let (mut name, mut domain) = ([0u16; 256], [0u16; 256]);
    let (mut name_len, mut domain_len) = (name.len() as u32, domain.len() as u32);
    let mut use_: SID_NAME_USE = 0;
    let ok = unsafe {
        LookupAccountSidW(
            std::ptr::null(),
            sid,
            name.as_mut_ptr(),
            &mut name_len,
            domain.as_mut_ptr(),
            &mut domain_len,
            &mut use_,
        )
    };
    (ok != 0).then(|| from_wide(&name[..name_len as usize]))
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
