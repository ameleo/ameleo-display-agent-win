// SPDX-License-Identifier: GPL-3.0-only

//! Acting on behalf of the user logged on in our session.
//!
//! The session agent runs as SYSTEM inside the user session. Anything that
//! must land in the user profile (HKCU, Downloads folder, Explorer windows)
//! goes through the user token.

use std::io;

use windows_sys::Win32::Foundation::{ERROR_PRIVILEGE_NOT_HELD, HANDLE};
use windows_sys::Win32::Security::{
    ImpersonateLoggedOnUser, RevertToSelf, TOKEN_ASSIGN_PRIMARY, TOKEN_DUPLICATE,
    TOKEN_IMPERSONATE, TOKEN_QUERY,
};
use windows_sys::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
};

use crate::win::OwnedHandle;

pub fn current_session() -> u32 {
    let mut session = 0u32;
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
    session
}

/// Primary token of the user logged on in our session.
///
/// `WTSQueryUserToken` needs SeTcbPrivilege, which only SYSTEM has (the
/// service mode). In `console` mode the agent already runs as the user, its
/// own token is the user's.
pub fn token() -> io::Result<OwnedHandle> {
    let mut h: HANDLE = std::ptr::null_mut();
    if unsafe { WTSQueryUserToken(current_session(), &mut h) } != 0 {
        return OwnedHandle::new(h);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() != Some(ERROR_PRIVILEGE_NOT_HELD as i32) {
        return Err(err);
    }
    let access = TOKEN_QUERY | TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_IMPERSONATE;
    if unsafe { OpenProcessToken(GetCurrentProcess(), access, &mut h) } == 0 {
        return Err(io::Error::last_os_error());
    }
    OwnedHandle::new(h)
}

/// `Agent1.Reset` (the remote client went away): the session locked, if a
/// user is logged on. Always asked of Windows, which does nothing for a
/// session locked already: a stale lock state must never leave the session
/// open behind the client. `LockWorkStation` returns at once.
pub fn lock_session() {
    if token().is_err() {
        return;
    }
    if unsafe { windows_sys::Win32::System::Shutdown::LockWorkStation() } == 0 {
        tracing::warn!("cannot lock the session: {}", io::Error::last_os_error());
    } else {
        tracing::info!("session locked (remote client gone)");
    }
}
/// Impersonates the user on the calling thread until dropped.
pub struct Impersonation {
    _token: OwnedHandle,
}

impl Impersonation {
    pub fn begin() -> io::Result<Self> {
        let token = token()?;
        if unsafe { ImpersonateLoggedOnUser(token.raw()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { _token: token })
    }
}

impl Drop for Impersonation {
    fn drop(&mut self) {
        unsafe { RevertToSelf() };
    }
}
