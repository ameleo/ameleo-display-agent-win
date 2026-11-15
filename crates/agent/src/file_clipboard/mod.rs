// SPDX-License-Identifier: GPL-3.0-only

//! The clipboard of files, MS-RDPECLIP style, as `FileClipboard1` defines
//! it. It adds the [`mime::FILE_LIST`] type to `Clipboard1`.
//!
//! * Guest to host: a copy of files (`CF_HDROP`, from Explorer) is
//!   announced as [`mime::FILE_LIST`]. When the host asks for it, the file
//!   list is built as the user ([`filelist`]), and the host reads the files
//!   through `FileClipboard1`.
//! * Host to guest: when the host's clipboard holds a file list, the
//!   clipboard helper, running as the user ([`helper`]), puts a data object
//!   of virtual files on the guest clipboard ([`olecb`]). Explorer pastes
//!   from it, and the helper reads the files from the host through
//!   `Host.Clipboard1` and `Host.FileClipboard1`.

mod call;
pub mod filelist;
mod formats;
pub mod helper;
mod olecb;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use proto::{AGENT_PATH, Error, Result, iface, mime};
use windows_sys::Win32::System::DataExchange::IsClipboardFormatAvailable;
use windows_sys::Win32::System::Ole::CF_HDROP;
use zbus::connection::Builder;

use crate::feature::{ClipboardFormats, Feature, MakeData};
use crate::host::Host;
use filelist::GuestFiles;
use olecb::OleCmd;

const CF_HDROP_U: u32 = CF_HDROP as u32;

struct State {
    /// Guest file lists the host may read (file copy in the guest).
    files: Mutex<GuestFiles>,
    /// Guest file being read by the host.
    open_file: filelist::OpenFile,
    locked: Arc<AtomicBool>,
    helper: helper::Helper,
}

/// The clipboard of files, as a feature.
pub struct FileClipboard(Arc<State>);

impl FileClipboard {
    pub fn new(host: Host, locked: Arc<AtomicBool>) -> Self {
        Self(Arc::new(State {
            helper: helper::Helper::new(Arc::new(host)),
            files: Mutex::default(),
            open_file: Mutex::new(None),
            locked,
        }))
    }
}

impl Feature for FileClipboard {
    fn channels(&self) -> Vec<&'static str> {
        vec![iface::FILE_CLIPBOARD]
    }

    fn serve<'a>(&self, builder: Builder<'a>) -> zbus::Result<Builder<'a>> {
        builder.serve_at(AGENT_PATH, FileClipboardIface(self.0.clone()))
    }

    fn reset(&self) {
        self.0.files.lock().unwrap().reset();
        *self.0.open_file.lock().unwrap() = None;
        self.0.helper.send(OleCmd::Clear);
    }

    fn stop(&self) {
        self.reset();
    }

    fn clipboard_formats(&self) -> Option<Arc<dyn ClipboardFormats>> {
        Some(Arc::new(Formats(self.0.clone())))
    }
}

impl State {
    /// The CLIPRDR_FILELIST of the guest copy `serial`, from its raw
    /// `CF_HDROP`, built once per copy, as the user.
    fn file_list(&self, serial: u32, raw: &[u8]) -> Result<Vec<u8>> {
        if let Some(entries) = self.files.lock().unwrap().cached(serial) {
            return Ok(filelist::encode(&entries));
        }
        let paths = filelist::hdrop_paths(raw)?;
        let entries = Arc::new(filelist::walk(&paths)?);
        tracing::info!(serial, files = entries.len(), "guest file list built");
        self.files
            .lock()
            .unwrap()
            .set_current(serial, entries.clone());
        Ok(filelist::encode(&entries))
    }
}

/// The `mime::FILE_LIST` type of `Clipboard1`.
struct Formats(Arc<State>);

impl ClipboardFormats for Formats {
    fn guest_mimes(&self) -> Vec<String> {
        if unsafe { IsClipboardFormatAvailable(CF_HDROP_U) } != 0 {
            vec![mime::FILE_LIST.to_string()]
        } else {
            Vec::new()
        }
    }

    fn guest_changed(&self, serial: u32) {
        // A new guest copy: the previous file list is not current anymore.
        self.0.files.lock().unwrap().clipboard_changed(serial);
        // Do not keep a guest file open (and locked against deletion) once
        // it left the clipboard. A lock still in use reopens it.
        *self.0.open_file.lock().unwrap() = None;
    }

    fn read(&self, mime_type: &str, serial: u32) -> Option<std::result::Result<MakeData, String>> {
        if mime_type != mime::FILE_LIST || unsafe { IsClipboardFormatAvailable(CF_HDROP_U) } == 0 {
            return None;
        }
        // The raw CF_HDROP now, on the desktop thread that has the
        // clipboard open; the file list off it.
        let raw = match crate::clipboard::read_global(CF_HDROP_U) {
            Ok(raw) => raw,
            Err(e) => return Some(Err(e)),
        };
        let state = self.0.clone();
        Some(Ok(Box::new(move || state.file_list(serial, &raw))))
    }

    fn host_grab(&self, serial: u32, mimes: &[String]) -> bool {
        if !mimes.iter().any(|m| m == mime::FILE_LIST) {
            return false;
        }
        if let Err(e) = self.0.helper.ensure() {
            tracing::warn!("cannot start the clipboard helper (no file paste): {e}");
            return false;
        }
        self.0.helper.send(OleCmd::Offer {
            serial,
            mimes: mimes.to_vec(),
        });
        true
    }

    fn host_release(&self) {
        self.0.helper.send(OleCmd::Clear);
    }

    fn owns(&self, pid: u32) -> bool {
        self.0.helper.pid() == Some(pid)
    }
}

/// `fr.ameleo.Agent.FileClipboard1`: the host reads the guest's files.
struct FileClipboardIface(Arc<State>);

#[zbus::interface(name = "fr.ameleo.Agent.FileClipboard1")]
impl FileClipboardIface {
    fn lock_clip_data(&self, clip_data_id: u32) -> Result<()> {
        tracing::debug!(clip_data_id, "lock clip data");
        self.0.files.lock().unwrap().lock(clip_data_id)
    }

    fn unlock_clip_data(&self, clip_data_id: u32) {
        tracing::debug!(clip_data_id, "unlock clip data");
        self.0.files.lock().unwrap().unlock(clip_data_id);
    }

    async fn file_contents(
        &self,
        lindex: i32,
        flags: u32,
        position: u64,
        requested: u32,
        clip_data_id: u32,
        has_clip_data_id: bool,
    ) -> Result<Vec<u8>> {
        if self.0.locked.load(Ordering::Relaxed) {
            return Err(Error::SessionLocked("the session is locked".into()));
        }
        let entries = self
            .0
            .files
            .lock()
            .unwrap()
            .entries(has_clip_data_id.then_some(clip_data_id))?;
        let state = self.0.clone();
        let t = std::time::Instant::now();
        let res = tokio::task::spawn_blocking(move || {
            filelist::contents(
                &entries,
                &state.open_file,
                lindex,
                flags,
                position,
                requested,
            )
        })
        .await
        .map_err(|e| Error::Failed(e.to_string()))?;
        match &res {
            Ok(d) => {
                tracing::debug!(lindex, flags, position, requested, bytes = d.len(), took = ?t.elapsed(), "guest file contents served")
            }
            Err(e) => tracing::warn!(
                lindex,
                flags,
                position,
                requested,
                "guest file contents failed: {e}"
            ),
        }
        res
    }
}

#[cfg(test)]
mod contract {
    use zbus::object_server::Interface;

    #[test]
    fn interface_names_follow_the_protocol() {
        assert_eq!(
            super::FileClipboardIface::name().as_str(),
            proto::iface::FILE_CLIPBOARD
        );
    }
}
