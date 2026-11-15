// SPDX-License-Identifier: GPL-3.0-or-later

//! `IMFActivate` for the media source: what Frame Server (and the virtual
//! camera Start) asks the class factory for. Attributes Frame Server sets
//! on it are copied to the source when it is created.

use std::sync::Mutex;

use windows::Win32::Foundation::E_POINTER;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows_core::{BOOL, GUID, IUnknown, Interface, PCWSTR, PWSTR, Ref, implement};

use crate::source::Source;

#[implement(IMFActivate)]
pub struct Activate {
    slot: u32,
    attrs: IMFAttributes,
    source: Mutex<Option<IMFMediaSource>>,
}

impl Activate {
    pub fn create(slot: u32) -> windows_core::Result<IMFActivate> {
        let mut attrs = None;
        unsafe { MFCreateAttributes(&mut attrs, 4)? };
        let attrs = attrs.ok_or(E_POINTER)?;
        Ok(Activate {
            slot,
            attrs,
            source: Mutex::new(None),
        }
        .into())
    }
}

/// The delegate's vtable and raw pointer.
macro_rules! call {
    ($self:ident, $m:ident ( $($arg:expr),* )) => {
        unsafe { (Interface::vtable(&$self.attrs).$m)(Interface::as_raw(&$self.attrs) $(, $arg)*).ok() }
    };
}

/// An interface parameter as the raw pointer the vtable takes.
fn raw<T: Interface>(r: &Ref<T>) -> *mut core::ffi::c_void {
    r.as_ref().map_or(std::ptr::null_mut(), |i| i.as_raw())
}

impl IMFActivate_Impl for Activate_Impl {
    fn ActivateObject(
        &self,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> windows_core::Result<()> {
        if ppv.is_null() {
            return Err(E_POINTER.into());
        }
        let mut cached = self.source.lock().unwrap();
        let source = match &*cached {
            Some(s) => s.clone(),
            None => {
                let s: IMFMediaSource = Source::create(self.slot, Some(&self.attrs))?.cast()?;
                *cached = Some(s.clone());
                s
            }
        };
        unsafe { source.query(riid, ppv).ok() }
    }

    fn ShutdownObject(&self) -> windows_core::Result<()> {
        if let Some(s) = self.source.lock().unwrap().take() {
            unsafe { s.Shutdown()? };
        }
        Ok(())
    }

    fn DetachObject(&self) -> windows_core::Result<()> {
        self.source.lock().unwrap().take();
        Ok(())
    }
}

impl IMFAttributes_Impl for Activate_Impl {
    fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> windows_core::Result<()> {
        call!(self, GetItem(k, v))
    }
    fn GetItemType(&self, k: *const GUID) -> windows_core::Result<MF_ATTRIBUTE_TYPE> {
        let mut t = MF_ATTRIBUTE_TYPE::default();
        call!(self, GetItemType(k, &mut t)).map(|()| t)
    }
    fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> windows_core::Result<BOOL> {
        let mut r = BOOL(0);
        call!(self, CompareItem(k, v, &mut r)).map(|()| r)
    }
    fn Compare(
        &self,
        theirs: Ref<IMFAttributes>,
        mt: MF_ATTRIBUTES_MATCH_TYPE,
    ) -> windows_core::Result<BOOL> {
        let mut r = BOOL(0);
        call!(self, Compare(raw(&theirs), mt, &mut r)).map(|()| r)
    }
    fn GetUINT32(&self, k: *const GUID) -> windows_core::Result<u32> {
        let mut v = 0;
        call!(self, GetUINT32(k, &mut v)).map(|()| v)
    }
    fn GetUINT64(&self, k: *const GUID) -> windows_core::Result<u64> {
        let mut v = 0;
        call!(self, GetUINT64(k, &mut v)).map(|()| v)
    }
    fn GetDouble(&self, k: *const GUID) -> windows_core::Result<f64> {
        let mut v = 0.0;
        call!(self, GetDouble(k, &mut v)).map(|()| v)
    }
    fn GetGUID(&self, k: *const GUID) -> windows_core::Result<GUID> {
        let mut v = GUID::zeroed();
        call!(self, GetGUID(k, &mut v)).map(|()| v)
    }
    fn GetStringLength(&self, k: *const GUID) -> windows_core::Result<u32> {
        let mut v = 0;
        call!(self, GetStringLength(k, &mut v)).map(|()| v)
    }
    fn GetString(
        &self,
        k: *const GUID,
        s: PWSTR,
        cch: u32,
        len: *mut u32,
    ) -> windows_core::Result<()> {
        call!(self, GetString(k, s, cch, len))
    }
    fn GetAllocatedString(
        &self,
        k: *const GUID,
        s: *mut PWSTR,
        len: *mut u32,
    ) -> windows_core::Result<()> {
        call!(self, GetAllocatedString(k, s, len))
    }
    fn GetBlobSize(&self, k: *const GUID) -> windows_core::Result<u32> {
        let mut v = 0;
        call!(self, GetBlobSize(k, &mut v)).map(|()| v)
    }
    fn GetBlob(
        &self,
        k: *const GUID,
        buf: *mut u8,
        size: u32,
        len: *mut u32,
    ) -> windows_core::Result<()> {
        call!(self, GetBlob(k, buf, size, len))
    }
    fn GetAllocatedBlob(
        &self,
        k: *const GUID,
        buf: *mut *mut u8,
        len: *mut u32,
    ) -> windows_core::Result<()> {
        call!(self, GetAllocatedBlob(k, buf, len))
    }
    fn GetUnknown(
        &self,
        k: *const GUID,
        riid: *const GUID,
        ppv: *mut *mut core::ffi::c_void,
    ) -> windows_core::Result<()> {
        call!(self, GetUnknown(k, riid, ppv))
    }
    fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> windows_core::Result<()> {
        call!(self, SetItem(k, v))
    }
    fn DeleteItem(&self, k: *const GUID) -> windows_core::Result<()> {
        call!(self, DeleteItem(k))
    }
    fn DeleteAllItems(&self) -> windows_core::Result<()> {
        call!(self, DeleteAllItems())
    }
    fn SetUINT32(&self, k: *const GUID, v: u32) -> windows_core::Result<()> {
        call!(self, SetUINT32(k, v))
    }
    fn SetUINT64(&self, k: *const GUID, v: u64) -> windows_core::Result<()> {
        call!(self, SetUINT64(k, v))
    }
    fn SetDouble(&self, k: *const GUID, v: f64) -> windows_core::Result<()> {
        call!(self, SetDouble(k, v))
    }
    fn SetGUID(&self, k: *const GUID, v: *const GUID) -> windows_core::Result<()> {
        call!(self, SetGUID(k, v))
    }
    fn SetString(&self, k: *const GUID, v: &PCWSTR) -> windows_core::Result<()> {
        call!(self, SetString(k, *v))
    }
    fn SetBlob(&self, k: *const GUID, buf: *const u8, size: u32) -> windows_core::Result<()> {
        call!(self, SetBlob(k, buf, size))
    }
    fn SetUnknown(&self, k: *const GUID, unk: Ref<IUnknown>) -> windows_core::Result<()> {
        call!(self, SetUnknown(k, raw(&unk)))
    }
    fn LockStore(&self) -> windows_core::Result<()> {
        call!(self, LockStore())
    }
    fn UnlockStore(&self) -> windows_core::Result<()> {
        call!(self, UnlockStore())
    }
    fn GetCount(&self) -> windows_core::Result<u32> {
        let mut v = 0;
        call!(self, GetCount(&mut v)).map(|()| v)
    }
    fn GetItemByIndex(
        &self,
        i: u32,
        k: *mut GUID,
        v: *mut PROPVARIANT,
    ) -> windows_core::Result<()> {
        call!(self, GetItemByIndex(i, k, v))
    }
    fn CopyAllItems(&self, dest: Ref<IMFAttributes>) -> windows_core::Result<()> {
        call!(self, CopyAllItems(raw(&dest)))
    }
}
