// SPDX-License-Identifier: GPL-3.0-only

//! Virtual camera media source, loaded by the Windows Frame Server.
//!
//! The guest agent creates a virtual camera with `MFCreateVirtualCamera`,
//! naming one of the CLSIDs of [`vcam_shm::CLSIDS`]. When an app opens the
//! camera, Frame Server (LocalService, session 0) loads this DLL and creates
//! the media source of that CLSID. The source reads its formats and frames
//! from the shared memory slot of the CLSID, and tells the agent when an
//! app starts or stops the camera.
//!
//! Registration (HKLM, admin): `regsvr32 vcam_source.dll`.

#![cfg(windows)]

mod activate;
mod controls;
mod playout;
mod source;
mod stream;

use std::ffi::c_void;

use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_POINTER, ERROR_SUCCESS, HMODULE, S_FALSE,
};
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    GetModuleFileNameW, GetModuleHandleExW,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW,
};
use windows_core::{BOOL, GUID, HRESULT, IUnknown, Interface, PCWSTR, Ref, implement};

#[implement(IClassFactory)]
struct ClassFactory {
    slot: u32,
}

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(
        &self,
        punkouter: Ref<IUnknown>,
        riid: *const GUID,
        ppvobject: *mut *mut c_void,
    ) -> windows_core::Result<()> {
        if ppvobject.is_null() || riid.is_null() {
            return Err(E_POINTER.into());
        }
        unsafe { *ppvobject = std::ptr::null_mut() };
        if punkouter.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        // Frame Server asks for IMFActivate; a direct media source request
        // gets the source.
        let activate = activate::Activate::create(self.slot)?;
        let hr = unsafe { activate.query(riid, ppvobject) };
        if hr.is_ok() {
            return Ok(());
        }
        let source: IUnknown = source::Source::create(self.slot, None)?;
        unsafe { source.query(riid, ppvobject).ok() }
    }

    fn LockServer(&self, _: BOOL) -> windows_core::Result<()> {
        Ok(())
    }
}

/// # Safety
/// COM entry point.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    unsafe { *ppv = std::ptr::null_mut() };
    let clsid = unsafe { *rclsid };
    let Some(slot) = vcam_shm::slot_of(clsid.to_u128()) else {
        return CLASS_E_CLASSNOTAVAILABLE;
    };
    let factory: IClassFactory = ClassFactory { slot }.into();
    unsafe { factory.query(riid, ppv) }
}

/// Frame Server keeps the DLL loaded while sources exist; never unload.
#[unsafe(no_mangle)]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

/// Appends a line to `%ProgramData%\ameleo-display-agent\vcam-source.log` (the
/// agent's log directory), or to the temp directory of Frame Server's
/// account. Never fails.
pub(crate) fn log(line: &str) {
    use std::io::Write;
    let name = "vcam-source.log";
    let dir = std::env::var_os("ProgramData")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| r"C:\ProgramData".into())
        .join("ameleo-display-agent");
    let open = |p: std::path::PathBuf| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
    };
    let Ok(mut f) = open(dir.join(name)).or_else(|_| open(std::env::temp_dir().join(name))) else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let _ = writeln!(
        f,
        "{}.{:03} pid {} {line}",
        now.as_secs(),
        now.subsec_millis(),
        std::process::id()
    );
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn own_path() -> windows_core::Result<String> {
    let mut module = HMODULE::default();
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(own_path as *const u16),
            &mut module,
        )?
    };
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    Ok(String::from_utf16_lossy(&buf[..n]))
}

fn set_value(key: HKEY, name: Option<&str>, value: &str) -> windows_core::Result<()> {
    let data: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
    let name = name.map(wide);
    let r = unsafe {
        RegSetValueExW(
            key,
            name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())),
            None,
            REG_SZ,
            Some(&data),
        )
    };
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(r.into())
    }
}

fn register(slot: u32, path: &str) -> windows_core::Result<()> {
    let clsid = vcam_shm::clsid_string(slot);
    let subkey = wide(&format!(r"Software\Classes\CLSID\{clsid}\InprocServer32"));
    let mut key = HKEY::default();
    let r = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if r != ERROR_SUCCESS {
        return Err(r.into());
    }
    let res =
        set_value(key, None, path).and_then(|()| set_value(key, Some("ThreadingModel"), "Both"));
    unsafe {
        let _ = RegCloseKey(key);
    }
    res
}

/// `regsvr32 vcam_source.dll`: registers every slot CLSID in HKLM.
#[unsafe(no_mangle)]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    let res =
        own_path().and_then(|path| (0..vcam_shm::SLOTS).try_for_each(|slot| register(slot, &path)));
    match res {
        Ok(()) => HRESULT(0),
        Err(e) => e.code(),
    }
}

/// `regsvr32 /u vcam_source.dll`.
#[unsafe(no_mangle)]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    for slot in 0..vcam_shm::SLOTS {
        let subkey = wide(&format!(
            r"Software\Classes\CLSID\{}",
            vcam_shm::clsid_string(slot)
        ));
        unsafe {
            let _ = RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(subkey.as_ptr()));
        }
    }
    HRESULT(0)
}
