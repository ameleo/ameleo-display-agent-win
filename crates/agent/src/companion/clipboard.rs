// SPDX-License-Identifier: GPL-3.0-only

//! The companion's clipboard formats.
//!
//! The companion registers the clipboard formats it handles (the agent
//! handles text, images and files itself), with the MIME type it exchanges them as
//! in `Clipboard1` (`CompanionLink1.RegisterClipboard`). The agent then:
//!
//! * announces that MIME type when the guest clipboard holds the format,
//!   and calls `ClipboardChanged(u serial)` on each new guest copy;
//! * when the host asks for it, reads the format's raw data on its desktop
//!   thread and has the companion make the host's data from it:
//!   `ReadClipboard(s mime, u serial, ay raw) -> ay`;
//! * when the host's clipboard holds that MIME type, lets the companion put
//!   it on the guest clipboard itself: `HostGrab(u serial, as mimes)`, then
//!   `HostRelease()` when the host gives the clipboard up. The companion's
//!   helper processes, given by `SetClipboardOwners(au)`, own the clipboard
//!   then, and their changes are not taken for copies made by the user.

use std::sync::Arc;
use std::sync::Mutex;

use windows_sys::Win32::System::DataExchange::{
    IsClipboardFormatAvailable, RegisterClipboardFormatW,
};
use zbus::Connection;

use super::queue::Events;
use super::relay::Target;
use super::{COMPANION_IFACE, COMPANION_PATH};
use crate::feature::{ClipboardFormats, MakeData};
use crate::host::HOST_CALL_TIMEOUT;
use crate::win::wide;

use proto::companion::MAX_CLIPBOARD_FORMATS as MAX_FORMATS;

#[derive(Default)]
struct Registered {
    /// (clipboard format, MIME type).
    formats: Vec<(u32, String)>,
    owners: Vec<u32>,
}

pub struct CompanionClipboard {
    link: Target,
    /// The agent's calls to the companion, in order.
    events: Arc<Events>,
    registered: Mutex<Registered>,
    rt: tokio::runtime::Handle,
}

impl CompanionClipboard {
    pub fn new(link: Target, events: Arc<Events>, rt: tokio::runtime::Handle) -> Arc<Self> {
        Arc::new(Self {
            link,
            events,
            registered: Mutex::default(),
            rt,
        })
    }

    pub fn register(&self, formats: Vec<(String, String)>) -> proto::Result<()> {
        if formats.len() > MAX_FORMATS {
            return Err(proto::Error::InvalidArgument(format!(
                "at most {MAX_FORMATS} formats"
            )));
        }
        let mut ids = Vec::with_capacity(formats.len());
        for (name, mime) in formats {
            let id = match name.strip_prefix('#') {
                Some(n) => n.parse().ok().filter(|&id| id != 0),
                None if !name.is_empty() => {
                    Some(unsafe { RegisterClipboardFormatW(wide(&name).as_ptr()) })
                        .filter(|&id| id != 0)
                }
                None => None,
            };
            let Some(id) = id else {
                return Err(proto::Error::InvalidArgument(format!(
                    "bad clipboard format: {name}"
                )));
            };
            ids.push((id, mime));
        }
        tracing::info!(formats = ?ids, "companion clipboard formats");
        self.registered.lock().unwrap().formats = ids;
        Ok(())
    }

    pub fn set_owners(&self, pids: Vec<u32>) {
        tracing::debug!(?pids, "companion clipboard owners");
        self.registered.lock().unwrap().owners = pids;
    }

    /// The companion went away.
    pub fn clear(&self) {
        *self.registered.lock().unwrap() = Registered::default();
    }

    fn link(&self) -> Option<Connection> {
        self.link.lock().unwrap().clone()
    }

    /// Calls `member` on the companion, in order with the other events,
    /// without waiting.
    fn post<B>(&self, member: &'static str, body: B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Send + Sync + 'static,
    {
        self.events.post(member, body);
    }

    fn format_of(&self, mime: &str) -> Option<u32> {
        let registered = self.registered.lock().unwrap();
        registered
            .formats
            .iter()
            .find(|(_, m)| m == mime)
            .map(|(id, _)| *id)
    }
}

impl ClipboardFormats for CompanionClipboard {
    fn guest_mimes(&self) -> Vec<String> {
        let registered = self.registered.lock().unwrap();
        let mut mimes: Vec<String> = Vec::new();
        for (id, mime) in &registered.formats {
            if unsafe { IsClipboardFormatAvailable(*id) } != 0 && !mimes.contains(mime) {
                mimes.push(mime.clone());
            }
        }
        mimes
    }

    fn guest_changed(&self, serial: u32) {
        if !self.registered.lock().unwrap().formats.is_empty() {
            self.post("ClipboardChanged", (serial,));
        }
    }

    fn read(&self, mime: &str, serial: u32) -> Option<Result<MakeData, String>> {
        let id = self.format_of(mime)?;
        if unsafe { IsClipboardFormatAvailable(id) } == 0 {
            return None;
        }
        let raw = match crate::clipboard::read_global(id) {
            Ok(raw) => raw,
            Err(e) => return Some(Err(e)),
        };
        let link = self.link()?;
        let rt = self.rt.clone();
        let events = self.events.clone();
        let mime = mime.to_string();
        let make: MakeData = Box::new(move || {
            let body = (mime, serial, raw);
            let call = link.call_method(
                None::<&str>,
                COMPANION_PATH,
                Some(COMPANION_IFACE),
                "ReadClipboard",
                &body,
            );
            // After the events queued before it (a reset, a host grab). The
            // timeout is made inside `block_on`: its timer needs the runtime,
            // and this runs on a clipboard thread.
            let reply = rt
                .block_on(async {
                    events.barrier().await;
                    tokio::time::timeout(HOST_CALL_TIMEOUT, call).await
                })
                .map_err(|_| proto::Error::Failed("the companion did not answer".into()))?;
            match reply {
                Ok(msg) => msg
                    .body()
                    .deserialize::<Vec<u8>>()
                    .map_err(|e| proto::Error::Failed(e.to_string())),
                // The companion's own error, such as SessionLocked, as it is.
                Err(zbus::Error::MethodError(name, text, _)) => {
                    Err(proto_error(name.as_str(), text.unwrap_or_default()))
                }
                Err(e) => Err(proto::Error::Failed(e.to_string())),
            }
        });
        Some(Ok(make))
    }

    fn host_grab(&self, serial: u32, mimes: &[String]) -> bool {
        if !mimes.iter().any(|m| self.format_of(m).is_some()) {
            return false;
        }
        self.post("HostGrab", (serial, mimes.to_vec()));
        true
    }

    fn host_release(&self) {
        self.post("HostRelease", ());
    }

    fn owns(&self, pid: u32) -> bool {
        self.registered.lock().unwrap().owners.contains(&pid)
    }
}

/// A `fr.ameleo.Agent.Error.*` name back to its `proto::Error`.
fn proto_error(name: &str, text: String) -> proto::Error {
    match name.strip_prefix("fr.ameleo.Agent.Error.") {
        Some("NoData") => proto::Error::NoData(text),
        Some("SessionLocked") => proto::Error::SessionLocked(text),
        Some("NotEnoughSpace") => proto::Error::NotEnoughSpace(text),
        Some("NotFound") => proto::Error::NotFound(text),
        Some("InvalidArgument") => proto::Error::InvalidArgument(text),
        _ => proto::Error::Failed(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registered_formats_decide_the_host_grabs() {
        let link = Target::default();
        let cb = CompanionClipboard::new(
            link.clone(),
            Events::new(link),
            tokio::runtime::Handle::current(),
        );
        let mimes = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Nothing registered: the agent keeps every grab.
        assert!(!cb.host_grab(1, &mimes(&[proto::mime::FILE_LIST])));
        assert!(
            cb.register(vec![("#0".into(), "x".into())]).is_err(),
            "format 0 does not exist"
        );
        cb.register(vec![("#15".into(), proto::mime::FILE_LIST.into())])
            .unwrap();
        assert!(cb.host_grab(2, &mimes(&[proto::mime::TEXT, proto::mime::FILE_LIST])));
        assert!(!cb.host_grab(3, &mimes(&[proto::mime::TEXT])));
        cb.set_owners(vec![42]);
        assert!(cb.owns(42) && !cb.owns(43));
        cb.clear();
        assert!(!cb.owns(42));
        assert!(!cb.host_grab(4, &mimes(&[proto::mime::FILE_LIST])));
    }
}
