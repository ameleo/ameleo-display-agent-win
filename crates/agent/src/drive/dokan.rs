// SPDX-License-Identifier: GPL-3.0-only

//! Dokany 2 (user mode file systems, LGPL): the API of `dokan2.dll`, loaded
//! at run time.
//!
//! Loaded, not linked: the agent runs without Dokany installed (no client
//! drives then), and the library stays the user's to replace (LGPL).
//! Declarations from Dokany 2.3.1's `dokan/dokan.h`.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows_sys::Win32::Foundation::{HANDLE, NTSTATUS};
use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, WIN32_FIND_DATAW};
use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};
use windows_sys::core::BOOL;

use crate::win::wide;

/// The API version we are written for (`DOKAN_VERSION`, 2.3.1).
pub const VERSION: u16 = 231;
/// Oldest `dokan2.dll` accepted (`DokanVersion()`): 2.0.0.
const MIN_VERSION: u32 = 200;

pub const SUCCESS: i32 = 0;

const VOLUME_SECURITY_DESCRIPTOR_MAX_SIZE: usize = 1024 * 16;

#[repr(C)]
pub struct Options {
    pub version: u16,
    pub single_thread: u8,
    pub options: u32,
    pub global_context: u64,
    pub mount_point: *const u16,
    pub unc_name: *const u16,
    pub timeout: u32,
    pub allocation_unit_size: u32,
    pub sector_size: u32,
    pub volume_security_descriptor_length: u32,
    pub volume_security_descriptor: [u8; VOLUME_SECURITY_DESCRIPTOR_MAX_SIZE],
}

#[repr(C)]
pub struct FileInfo {
    pub context: u64,
    pub dokan_context: u64,
    pub options: *mut Options,
    pub processing_context: *mut c_void,
    pub process_id: u32,
    pub is_directory: u8,
    pub delete_pending: u8,
    pub paging_io: u8,
    pub synchronous_io: u8,
    pub nocache: u8,
    pub write_to_end_of_file: u8,
}

pub type Info = *mut FileInfo;
pub type Name = *const u16;
pub type FillFindData = unsafe extern "system" fn(*mut WIN32_FIND_DATAW, Info) -> i32;

/// `DOKAN_OPERATIONS`, in its order. `None`: not supported.
#[repr(C)]
#[derive(Default)]
pub struct Operations {
    pub create_file: Option<
        unsafe extern "system" fn(Name, *mut c_void, u32, u32, u32, u32, u32, Info) -> NTSTATUS,
    >,
    pub cleanup: Option<unsafe extern "system" fn(Name, Info)>,
    pub close_file: Option<unsafe extern "system" fn(Name, Info)>,
    pub read_file:
        Option<unsafe extern "system" fn(Name, *mut c_void, u32, *mut u32, i64, Info) -> NTSTATUS>,
    pub write_file: Option<
        unsafe extern "system" fn(Name, *const c_void, u32, *mut u32, i64, Info) -> NTSTATUS,
    >,
    pub flush_file_buffers: Option<unsafe extern "system" fn(Name, Info) -> NTSTATUS>,
    pub get_file_information:
        Option<unsafe extern "system" fn(Name, *mut BY_HANDLE_FILE_INFORMATION, Info) -> NTSTATUS>,
    pub find_files: Option<unsafe extern "system" fn(Name, FillFindData, Info) -> NTSTATUS>,
    pub find_files_with_pattern:
        Option<unsafe extern "system" fn(Name, Name, FillFindData, Info) -> NTSTATUS>,
    pub set_file_attributes: Option<unsafe extern "system" fn(Name, u32, Info) -> NTSTATUS>,
    pub set_file_time: Option<
        unsafe extern "system" fn(Name, *const u64, *const u64, *const u64, Info) -> NTSTATUS,
    >,
    pub delete_file: Option<unsafe extern "system" fn(Name, Info) -> NTSTATUS>,
    pub delete_directory: Option<unsafe extern "system" fn(Name, Info) -> NTSTATUS>,
    pub move_file: Option<unsafe extern "system" fn(Name, Name, BOOL, Info) -> NTSTATUS>,
    pub set_end_of_file: Option<unsafe extern "system" fn(Name, i64, Info) -> NTSTATUS>,
    pub set_allocation_size: Option<unsafe extern "system" fn(Name, i64, Info) -> NTSTATUS>,
    pub lock_file: Option<unsafe extern "system" fn(Name, i64, i64, Info) -> NTSTATUS>,
    pub unlock_file: Option<unsafe extern "system" fn(Name, i64, i64, Info) -> NTSTATUS>,
    pub get_disk_free_space:
        Option<unsafe extern "system" fn(*mut u64, *mut u64, *mut u64, Info) -> NTSTATUS>,
    pub get_volume_information: Option<
        unsafe extern "system" fn(
            *mut u16,
            u32,
            *mut u32,
            *mut u32,
            *mut u32,
            *mut u16,
            u32,
            Info,
        ) -> NTSTATUS,
    >,
    pub mounted: Option<unsafe extern "system" fn(Name, Info) -> NTSTATUS>,
    pub unmounted: Option<unsafe extern "system" fn(Info) -> NTSTATUS>,
    pub get_file_security: Option<
        unsafe extern "system" fn(Name, *mut u32, *mut c_void, u32, *mut u32, Info) -> NTSTATUS,
    >,
    pub set_file_security:
        Option<unsafe extern "system" fn(Name, *mut u32, *mut c_void, u32, Info) -> NTSTATUS>,
    pub find_streams:
        Option<unsafe extern "system" fn(Name, *mut c_void, *mut c_void, Info) -> NTSTATUS>,
}

/// A mounted file system (`DOKAN_HANDLE`).
pub type Instance = *mut c_void;

/// The functions of `dokan2.dll` we use.
pub struct Api {
    init: InitFn,
    create_file_system: CreateFileSystemFn,
    close_handle: CloseHandleFn,
    open_requestor_token: OpenRequestorTokenFn,
    notify_create: NotifyCreateFn,
    notify_delete: NotifyCreateFn,
    notify_update: NotifyUpdateFn,
    notify_rename: NotifyRenameFn,
}

/// What GetProcAddress gives.
type Proc = unsafe extern "system" fn() -> isize;
type InitFn = unsafe extern "system" fn();
type CreateFileSystemFn =
    unsafe extern "system" fn(*mut Options, *mut Operations, *mut Instance) -> i32;
type CloseHandleFn = unsafe extern "system" fn(Instance);
type OpenRequestorTokenFn = unsafe extern "system" fn(Info) -> HANDLE;
/// `DokanNotifyCreate` and `DokanNotifyDelete`.
type NotifyCreateFn = unsafe extern "system" fn(Instance, Name, BOOL) -> BOOL;
type NotifyUpdateFn = unsafe extern "system" fn(Instance, Name) -> BOOL;
type NotifyRenameFn = unsafe extern "system" fn(Instance, Name, Name, BOOL, BOOL) -> BOOL;

static API: OnceLock<Result<Api, String>> = OnceLock::new();

/// `dokan2.dll` from System32, initialized once; the error says why not.
pub fn api() -> Result<&'static Api, String> {
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

fn load() -> Result<Api, String> {
    let name = wide("dokan2.dll");
    let lib = unsafe {
        LoadLibraryExW(
            name.as_ptr(),
            std::ptr::null_mut(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        )
    };
    if lib.is_null() {
        return Err(format!(
            "Dokany is not installed (dokan2.dll: {})",
            std::io::Error::last_os_error()
        ));
    }
    let sym = |n: &str| {
        let c = std::ffi::CString::new(n).unwrap();
        unsafe { GetProcAddress(lib, c.as_ptr().cast()) }
            .ok_or_else(|| format!("dokan2.dll has no {n}"))
    };
    // SAFETY: the signatures are dokan.h's.
    unsafe {
        let version: unsafe extern "system" fn() -> u32 = std::mem::transmute(sym("DokanVersion")?);
        let v = version();
        if v < MIN_VERSION {
            return Err(format!("Dokany {v} is too old, 2.0 or later is needed"));
        }
        use std::mem::transmute;
        let api = Api {
            init: transmute::<Proc, InitFn>(sym("DokanInit")?),
            create_file_system: transmute::<Proc, CreateFileSystemFn>(sym(
                "DokanCreateFileSystem",
            )?),
            close_handle: transmute::<Proc, CloseHandleFn>(sym("DokanCloseHandle")?),
            open_requestor_token: transmute::<Proc, OpenRequestorTokenFn>(sym(
                "DokanOpenRequestorToken",
            )?),
            notify_create: transmute::<Proc, NotifyCreateFn>(sym("DokanNotifyCreate")?),
            notify_delete: transmute::<Proc, NotifyCreateFn>(sym("DokanNotifyDelete")?),
            notify_update: transmute::<Proc, NotifyUpdateFn>(sym("DokanNotifyUpdate")?),
            notify_rename: transmute::<Proc, NotifyRenameFn>(sym("DokanNotifyRename")?),
        };
        (api.init)();
        tracing::info!(version = v, "Dokany loaded");
        Ok(api)
    }
}

impl Api {
    /// Mounts; `options` and `operations` must stay alive until
    /// [`Api::unmount`] returned.
    pub fn mount(
        &self,
        options: &mut Options,
        operations: &mut Operations,
    ) -> Result<Instance, i32> {
        let mut instance: Instance = std::ptr::null_mut();
        let r = unsafe { (self.create_file_system)(options, operations, &mut instance) };
        if r == SUCCESS { Ok(instance) } else { Err(r) }
    }

    /// Unmounts and waits until no callback runs anymore.
    pub fn unmount(&self, instance: Instance) {
        unsafe { (self.close_handle)(instance) };
    }

    /// The token of the thread that asked for the operation (to close).
    pub fn requestor_token(&self, info: Info) -> HANDLE {
        unsafe { (self.open_requestor_token)(info) }
    }

    /// Tells Explorer about a change made on the client. `path` with its
    /// leading `\`.
    pub fn notify(&self, instance: Instance, change: Notify<'_>) {
        let w = |p: &str| wide(p);
        unsafe {
            match change {
                Notify::Create { path, dir } => {
                    (self.notify_create)(instance, w(path).as_ptr(), dir as BOOL)
                }
                Notify::Delete { path, dir } => {
                    (self.notify_delete)(instance, w(path).as_ptr(), dir as BOOL)
                }
                Notify::Update { path } => (self.notify_update)(instance, w(path).as_ptr()),
                Notify::Rename { old, new, dir } => {
                    let same_dir =
                        old.rsplit_once('\\').map(|p| p.0) == new.rsplit_once('\\').map(|p| p.0);
                    (self.notify_rename)(
                        instance,
                        w(old).as_ptr(),
                        w(new).as_ptr(),
                        dir as BOOL,
                        same_dir as BOOL,
                    )
                }
            };
        }
    }
}

pub enum Notify<'a> {
    Create {
        path: &'a str,
        dir: bool,
    },
    Delete {
        path: &'a str,
        dir: bool,
    },
    Update {
        path: &'a str,
    },
    Rename {
        old: &'a str,
        new: &'a str,
        dir: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_match_dokan_h() {
        // x64: USHORT, BOOLEAN, ULONG, ULONG64, 2 pointers, 4 ULONG, the
        // descriptor.
        assert_eq!(std::mem::offset_of!(Options, options), 4);
        assert_eq!(std::mem::offset_of!(Options, global_context), 8);
        assert_eq!(std::mem::offset_of!(Options, timeout), 32);
        assert_eq!(
            std::mem::offset_of!(Options, volume_security_descriptor),
            48
        );
        assert_eq!(std::mem::offset_of!(FileInfo, process_id), 32);
        assert_eq!(std::mem::offset_of!(FileInfo, is_directory), 36);
        assert_eq!(std::mem::size_of::<FileInfo>(), 48);
        assert_eq!(std::mem::size_of::<Operations>(), 25 * 8);
    }
}
