// SPDX-License-Identifier: GPL-3.0-or-later

//! Host to guest file push into the user Downloads folder
//! (`fr.ameleo.Agent.FileTransfer1`).

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use proto::Error;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;

use windows_sys::Win32::Storage::FileSystem::{
    DELETE, FILE_DISPOSITION_INFO, FILE_GENERIC_WRITE, FileDispositionInfo, GetDiskFreeSpaceExW,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessWithTokenW, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows_sys::Win32::UI::Shell::{FOLDERID_Desktop, FOLDERID_Downloads, SHGetKnownFolderPath};
use windows_sys::core::{GUID, PWSTR};

use crate::feature::Feature;
use crate::user::{self, Impersonation};
use crate::win::{OwnedHandle, wide};

/// The file transfer feature.
pub struct FileTransfer {
    xfer: Arc<Mutex<FileXfer>>,
    locked: Arc<AtomicBool>,
}

impl FileTransfer {
    /// `locked`: the session lock state; no transfer starts while locked.
    pub fn new(locked: Arc<AtomicBool>) -> Self {
        Self {
            xfer: Arc::default(),
            locked,
        }
    }
}

impl Feature for FileTransfer {
    fn channels(&self) -> Vec<&'static str> {
        vec![proto::iface::FILE_TRANSFER]
    }

    fn serve<'a>(
        &self,
        builder: zbus::connection::Builder<'a>,
    ) -> zbus::Result<zbus::connection::Builder<'a>> {
        builder.serve_at(
            proto::AGENT_PATH,
            FileTransferIface {
                xfer: self.xfer.clone(),
                locked: self.locked.clone(),
            },
        )
    }

    fn reset(&self) {
        self.xfer.lock().unwrap().reset();
    }
}

struct FileTransferIface {
    xfer: Arc<Mutex<FileXfer>>,
    locked: Arc<AtomicBool>,
}

#[zbus::interface(name = "fr.ameleo.Agent.FileTransfer1")]
impl FileTransferIface {
    fn start(&self, name: &str, size: u64) -> Result<u32, Error> {
        if self.locked.load(Ordering::Relaxed) {
            return Err(Error::SessionLocked("the session is locked".into()));
        }
        self.xfer.lock().unwrap().start(name, size)
    }

    fn write(&self, id: u32, data: Vec<u8>) -> Result<(), Error> {
        if data.len() > proto::FILE_CHUNK_MAX {
            return Err(Error::InvalidArgument(format!(
                "chunk larger than {}",
                proto::FILE_CHUNK_MAX
            )));
        }
        self.xfer.lock().unwrap().write(id, &data)
    }

    fn finish(&self, id: u32) -> Result<String, Error> {
        self.xfer.lock().unwrap().finish(id)
    }

    fn cancel(&self, id: u32) -> Result<(), Error> {
        self.xfer.lock().unwrap().cancel(id)
    }
}

/// Same limit as the Linux vdagent: `name (63).ext` at most.
const MAX_ATTEMPTS: u32 = 64;
const RESERVED: &[char] = &[':', '/', '\\', '?', '*', '<', '>', '"', '|'];

struct Task {
    file: Option<File>,
    path: PathBuf,
    size: u64,
    written: u64,
}

impl Drop for Task {
    fn drop(&mut self) {
        // Still open: unfinished transfer, remove the partial file. Through
        // the handle, never by path: the user may have swapped a directory
        // of the path for a junction since, and we run as SYSTEM.
        if let Some(file) = self.file.take()
            && let Err(e) = delete_on_close(&file)
        {
            tracing::warn!("cannot remove partial file {}: {e}", self.path.display());
        }
    }
}

/// Marks an open file for deletion when its last handle closes.
fn delete_on_close(file: &File) -> std::io::Result<()> {
    let info = FILE_DISPOSITION_INFO { DeleteFile: true };
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            (&info as *const FILE_DISPOSITION_INFO).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Default)]
pub struct FileXfer {
    next_id: u32,
    tasks: HashMap<u32, Task>,
}

impl FileXfer {
    /// Aborts every transfer.
    pub fn reset(&mut self) {
        self.tasks.clear();
    }

    pub fn start(&mut self, name: &str, size: u64) -> Result<u32, Error> {
        validate_name(name)?;
        let token = user::token().map_err(|e| Error::Failed(format!("no logged on user: {e}")))?;
        let dir = known_folder(&FOLDERID_Downloads, &token)
            .or_else(|_| known_folder(&FOLDERID_Desktop, &token))
            .map_err(|e| Error::Failed(format!("no download folder: {e}")))?;

        let free = free_space(&dir).map_err(|e| Error::Failed(e.to_string()))?;
        if free < size {
            return Err(Error::NotEnoughSpace(free.to_string()));
        }

        // Created as the user so the file belongs to them.
        let (file, path) = {
            let _as_user = Impersonation::begin().map_err(|e| Error::Failed(e.to_string()))?;
            create_unique(&dir, name)?
        };
        tracing::info!("file transfer to {} ({size} bytes)", path.display());

        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.tasks.insert(
            id,
            Task {
                file: Some(file),
                path,
                size,
                written: 0,
            },
        );
        Ok(id)
    }

    pub fn write(&mut self, id: u32, data: &[u8]) -> Result<(), Error> {
        let task = self
            .tasks
            .get_mut(&id)
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))?;
        if task.written + data.len() as u64 > task.size {
            self.tasks.remove(&id);
            return Err(Error::InvalidArgument("more data than announced".into()));
        }
        let res = task.file.as_mut().expect("open task").write_all(data);
        if let Err(e) = res {
            self.tasks.remove(&id);
            return Err(Error::Failed(format!("write failed: {e}")));
        }
        task.written += data.len() as u64;
        Ok(())
    }

    pub fn finish(&mut self, id: u32) -> Result<String, Error> {
        let mut task = self
            .tasks
            .remove(&id)
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))?;
        if task.written != task.size {
            return Err(Error::InvalidArgument(format!(
                "{} of {} bytes received",
                task.written, task.size
            )));
        }
        drop(task.file.take()); // close and keep
        let path = std::mem::take(&mut task.path);
        show_in_explorer(&path);
        Ok(path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default())
    }

    pub fn cancel(&mut self, id: u32) -> Result<(), Error> {
        self.tasks
            .remove(&id)
            .map(|_| ())
            .ok_or_else(|| Error::NotFound(format!("transfer {id}")))
    }
}

/// DOS device names, reserved with any extension (`nul.txt` is `NUL`).
fn is_device_name(name: &str) -> bool {
    let stem = name
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    match stem.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        s => {
            let b = s.as_bytes();
            b.len() == 4 && (s.starts_with("COM") || s.starts_with("LPT")) && b[3].is_ascii_digit()
        }
    }
}

fn validate_name(name: &str) -> Result<(), Error> {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.contains(RESERVED)
        || name.chars().any(char::is_control)
        // Windows strips trailing dots and spaces: "a." would open "a".
        || name.ends_with(['.', ' '])
        || is_device_name(name)
    {
        return Err(Error::InvalidArgument(format!(
            "invalid file name {name:?}"
        )));
    }
    Ok(())
}

fn create_unique(dir: &Path, name: &str) -> Result<(File, PathBuf), Error> {
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for attempt in 0..MAX_ATTEMPTS {
        let candidate = if attempt == 0 {
            name.to_string()
        } else {
            format!("{stem} ({attempt}){ext}")
        };
        let path = dir.join(candidate);
        // DELETE access so an aborted transfer is removed through the handle.
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(FILE_GENERIC_WRITE | DELETE)
            .open(&path)
        {
            Ok(f) => return Ok((f, path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(Error::Failed(format!(
                    "cannot create {}: {e}",
                    path.display()
                )));
            }
        }
    }
    Err(Error::Failed(format!("too many copies of {name}")))
}

fn known_folder(id: &GUID, token: &OwnedHandle) -> std::io::Result<PathBuf> {
    let mut p: PWSTR = std::ptr::null_mut();
    let hr = unsafe { SHGetKnownFolderPath(id, 0, token.raw(), &mut p) };
    if hr < 0 {
        return Err(std::io::Error::from_raw_os_error(hr));
    }
    let path = unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        CoTaskMemFree(p.cast());
        s
    };
    Ok(PathBuf::from(path))
}

fn free_space(dir: &Path) -> std::io::Result<u64> {
    let d = wide(dir);
    let mut avail = 0u64;
    if unsafe {
        GetDiskFreeSpaceExW(
            d.as_ptr(),
            &mut avail,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(avail)
}

/// `explorer.exe /select,<file>` started as the user.
fn show_in_explorer(path: &Path) {
    let res = (|| -> std::io::Result<()> {
        let token = user::token()?;
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let app = wide(format!(r"{windir}\explorer.exe"));
        let mut cmd = wide(format!(
            r#""{windir}\explorer.exe" /select,"{}""#,
            path.display()
        ));
        let mut desktop = wide(r"winsta0\default");
        let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        si.lpDesktop = desktop.as_mut_ptr();
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { CreateEnvironmentBlock(&mut env, token.raw(), 0) };
        let ok = unsafe {
            CreateProcessWithTokenW(
                token.raw(),
                0,
                app.as_ptr(),
                cmd.as_mut_ptr(),
                CREATE_UNICODE_ENVIRONMENT,
                env,
                std::ptr::null(),
                &si,
                &mut pi,
            )
        };
        let err = std::io::Error::last_os_error();
        unsafe {
            if !env.is_null() {
                DestroyEnvironmentBlock(env);
            }
        }
        if ok == 0 {
            return Err(err);
        }
        drop(OwnedHandle(pi.hThread));
        drop(OwnedHandle(pi.hProcess));
        Ok(())
    })();
    if let Err(e) = res {
        tracing::warn!("cannot open Explorer on {}: {e}", path.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(validate_name("report.pdf").is_ok());
        assert!(validate_name("a:b").is_err());
        assert!(validate_name("..").is_err());
        assert!(validate_name("dir\\x").is_err());
        assert!(validate_name("a.").is_err());
        assert!(validate_name("a ").is_err());
        for dev in ["CON", "nul.txt", "Aux.tar.gz", "com1", "LPT9.log", "conin$"] {
            assert!(validate_name(dev).is_err(), "{dev}");
        }
        for ok in [
            "console.txt",
            "com10.txt",
            "nul_report",
            "lpt.txt",
            ".gitignore",
        ] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn unfinished_task_is_deleted_finished_is_kept() {
        let dir = std::env::temp_dir().join(format!("qda-del-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let (file, path) = create_unique(&dir, "partial.bin").unwrap();
        drop(Task {
            file: Some(file),
            path: path.clone(),
            size: 10,
            written: 0,
        });
        assert!(!path.exists(), "partial file left behind");

        let (file, path) = create_unique(&dir, "done.bin").unwrap();
        let mut task = Task {
            file: Some(file),
            path: path.clone(),
            size: 0,
            written: 0,
        };
        drop(task.file.take());
        drop(task);
        assert!(path.exists(), "finished file removed");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unique_suffix() {
        let dir = std::env::temp_dir().join(format!("qda-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (_, a) = create_unique(&dir, "f.txt").unwrap();
        let (_, b) = create_unique(&dir, "f.txt").unwrap();
        assert_eq!(a.file_name().unwrap(), "f.txt");
        assert_eq!(b.file_name().unwrap(), "f (1).txt");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
