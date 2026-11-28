// SPDX-License-Identifier: GPL-3.0-only

//! The agent's end of the link with the Print Support App
//! ([`proto::print_link`]): a named pipe that only processes of the app's
//! package may use. A process is taken for the app when its package family
//! name is [`PACKAGE_FAMILY_NAME`] and its package is installed under
//! `WindowsApps`: the family name alone comes from the package manifest,
//! and a package registered from a folder in developer mode can claim any.
//!
//! - `DocumentProperties`: the printing preferences. The agent has the host
//!   show the client driver's own dialog on the client
//!   (`Host.Printer1.DocumentProperties`), waits for
//!   `Printer1.DocumentPropertiesDone` and answers with the PrintTicket.
//! - `Job`: a job printed to a virtual printer, to the host (`print_jobs`).
//! - `Capabilities`: the print capabilities of a virtual printer.

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::print_link::{PACKAGE_FAMILY_NAME, PIPE_NAME, Reply, Request};
use windows_sys::Win32::Storage::Packaging::Appx::{
    GetPackageFamilyName, GetPackageFullName, GetPackagePathByFullName,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::host::Host;
use crate::printer::Printers;
use crate::win::{OwnedHandle, pipe_accept, pipe_client, pipe_instance};

/// The user may keep the client dialog open that long.
const DIALOG_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// SYSTEM and the interactive users (the app runs as the user); its
/// background tasks run in an app container, low integrity: all app
/// packages (AC) and low integrity writers too. The package family of the
/// client is checked afterwards.
const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GRGW;;;IU)(A;;GRGW;;;AC)S:(ML;;NW;;;LW)";

/// What `Printer1.DocumentPropertiesDone` brought.
pub struct Done {
    pub ticket: String,
    pub cancelled: bool,
}

/// Client driver dialogs waiting for `DocumentPropertiesDone`.
#[derive(Default)]
pub struct Dialogs {
    next: AtomicU32,
    pending: Mutex<HashMap<u32, std_mpsc::Sender<Done>>>,
}

impl Dialogs {
    fn open(&self) -> (u32, std_mpsc::Receiver<Done>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = std_mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        (id, rx)
    }

    fn forget(&self, id: u32) {
        self.pending.lock().unwrap().remove(&id);
    }

    /// `false`: no dialog `id` pending.
    pub fn done(&self, id: u32, done: Done) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(tx) => tx.send(done).is_ok(),
            None => false,
        }
    }

    /// Session gone: the pending dialogs fail.
    pub fn clear(&self) {
        self.pending.lock().unwrap().clear();
    }
}

pub fn spawn(printers: Arc<Printers>, dialogs: Arc<Dialogs>, host: Host) {
    let _ = std::thread::Builder::new()
        .name("print-link".into())
        .spawn(move || {
            let mut first = true;
            loop {
                let pipe = match pipe_instance(PIPE_NAME, PIPE_SDDL, first, false) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!("no link with the Print Support App: {PIPE_NAME}: {e}");
                        return;
                    }
                };
                first = false;
                if !pipe_accept(&pipe) {
                    continue;
                }
                let (printers, dialogs, host) = (printers.clone(), dialogs.clone(), host.clone());
                let _ = std::thread::Builder::new()
                    .name("print-link-conn".into())
                    .spawn(move || {
                        if let Err(e) = serve(pipe, &printers, &dialogs, &host) {
                            tracing::warn!("Print Support App: {e}");
                        }
                    });
            }
        });
}

/// The package of a process.
struct Package {
    family: String,
    /// Where the package is installed.
    path: String,
}

const APPMODEL_ERROR_NO_PACKAGE: u32 = 15700;

/// The package of a process, `None` if it is not packaged.
fn package_of(pid: u32) -> Result<Option<Package>, String> {
    let process =
        OwnedHandle::new(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) })
            .map_err(|e| format!("pid {pid}: {e}"))?;
    let failed = |what: &str, e: u32| {
        format!(
            "pid {pid}: package {what}: {}",
            std::io::Error::from_raw_os_error(e as i32)
        )
    };
    let mut family = [0u16; 256];
    let mut len = family.len() as u32;
    match unsafe { GetPackageFamilyName(process.raw(), &mut len, family.as_mut_ptr()) } {
        0 => {}
        APPMODEL_ERROR_NO_PACKAGE => return Ok(None),
        e => return Err(failed("family name", e)),
    }
    let family = String::from_utf16_lossy(&family[..(len as usize).saturating_sub(1)]);
    let mut full = [0u16; 256];
    let mut len = full.len() as u32;
    match unsafe { GetPackageFullName(process.raw(), &mut len, full.as_mut_ptr()) } {
        0 => {}
        e => return Err(failed("full name", e)),
    }
    let mut path = vec![0u16; 1024];
    let mut len = path.len() as u32;
    match unsafe { GetPackagePathByFullName(full.as_ptr(), &mut len, path.as_mut_ptr()) } {
        0 => {}
        e => return Err(failed("path", e)),
    }
    let path = String::from_utf16_lossy(&path[..(len as usize).saturating_sub(1)]);
    Ok(Some(Package { family, path }))
}

/// Whether a package installed at `path` was installed from a signed
/// package, into `%ProgramFiles%\WindowsApps`, which only the system
/// writes to. A package registered from a folder (developer mode) runs from
/// that folder, and its family name proves nothing: it comes from its
/// manifest's publisher, which anyone can write.
fn installed_package(path: &str) -> bool {
    let Some(root) = program_files() else {
        return false;
    };
    let windows_apps = format!("{}\\windowsapps\\", root.trim_end_matches('\\')).to_lowercase();
    let path = path.to_lowercase();
    // No way out of the folder through its name.
    path.starts_with(&windows_apps) && !path.contains("\\..")
}

/// `%ProgramFiles%`, from the shell's known folder, not the environment.
fn program_files() -> Option<String> {
    use windows_sys::Win32::System::Com::CoTaskMemFree;
    use windows_sys::Win32::UI::Shell::{
        FOLDERID_ProgramFiles, KF_FLAG_DEFAULT, SHGetKnownFolderPath,
    };
    let mut p: windows_sys::core::PWSTR = std::ptr::null_mut();
    let hr = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_ProgramFiles,
            KF_FLAG_DEFAULT as u32,
            std::ptr::null_mut(),
            &mut p,
        )
    };
    let path = (hr >= 0).then(|| crate::win::pwstr(p));
    unsafe { CoTaskMemFree(p as *const _) };
    path
}

fn serve(
    mut f: File,
    printers: &Arc<Printers>,
    dialogs: &Dialogs,
    host: &Host,
) -> Result<(), String> {
    let pid = pipe_client(&f).map_err(|e| format!("client process: {e}"))?;
    let package = package_of(pid)?;
    let image = || crate::win::image_path(pid).unwrap_or_default();
    match &package {
        Some(p) if p.family == PACKAGE_FAMILY_NAME && installed_package(&p.path) => {}
        Some(p) => {
            return Err(format!(
                "pid {pid} ({}, package {} at {}) refused: not the installed Print Support App",
                image(),
                p.family,
                p.path
            ));
        }
        None => {
            return Err(format!(
                "pid {pid} ({}, not packaged) refused: not the Print Support App",
                image()
            ));
        }
    }
    let req = Request::read(&mut f).map_err(|e| format!("request: {e}"))?;
    // The app of our session's user only: the printers lead to its
    // client. Capabilities are the client printer's and say nothing of the
    // user.
    if !matches!(req, Request::Capabilities { .. }) {
        let mut session = 0u32;
        unsafe {
            windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(pid, &mut session)
        };
        if session != crate::user::current_session() {
            return Err(format!(
                "pid {pid}: Print Support App of session {session} refused"
            ));
        }
    }
    let reply = match req {
        Request::Attach { installed } => {
            printers.attach(f, installed);
            return Ok(());
        }
        Request::DocumentProperties { printer, ticket } => {
            document_properties(printers, dialogs, host, &printer, &ticket)
        }
        Request::Job {
            printer,
            title,
            ticket,
        } => job(&mut f, printers, host, pid, printer, title, ticket),
        Request::Capabilities { printer } => capabilities(printers, &printer),
    };
    reply.write(&mut f).map_err(|e| format!("reply: {e}"))
}

fn job(
    f: &mut File,
    printers: &Printers,
    host: &Host,
    pid: u32,
    uri: String,
    title: String,
    ticket: String,
) -> Reply {
    let mut data = Vec::new();
    loop {
        match proto::print_link::read_chunk(f) {
            Ok(Some(chunk)) => {
                if data.len() + chunk.len() > crate::printer::jobs::MAX_JOB {
                    return Reply::Failed(format!(
                        "job over {} bytes",
                        crate::printer::jobs::MAX_JOB
                    ));
                }
                data.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(e) => return Reply::Failed(format!("job data: {e}")),
        }
    }
    let user = crate::win::process_user(pid).unwrap_or_default();
    let job = crate::printer::jobs::Job {
        uri,
        title,
        user,
        ticket,
        data,
    };
    match crate::printer::jobs::deliver(printers, host, job) {
        Ok(()) => Reply::Done,
        Err(e) => Reply::Failed(e),
    }
}

fn capabilities(printers: &Printers, uri: &str) -> Reply {
    match printers.driver_of(uri) {
        // The client driver's PrintCapabilities: the app makes the printer's
        // PDC out of them (empty: it keeps its package's).
        Some(d) => {
            tracing::info!(
                uri,
                capabilities = d.capabilities.len(),
                default_ticket = d.default_ticket.len(),
                "print capabilities requested"
            );
            Reply::Capabilities {
                capabilities: d.capabilities,
                default_ticket: d.default_ticket,
            }
        }
        None => Reply::Failed(format!("{uri}: not a client printer")),
    }
}

fn document_properties(
    printers: &Printers,
    dialogs: &Dialogs,
    host: &Host,
    name: &str,
    ticket: &str,
) -> Reply {
    let Some(printer) = printers.id_of(name) else {
        tracing::warn!(
            name,
            "preferences of a printer that is not a client printer"
        );
        return Reply::Failed(format!("{name} is not a client printer"));
    };
    // The client driver starts from the user's current settings.
    let ticket = crate::printer::ticket::for_client(ticket).unwrap_or_default();
    let (request, done) = dialogs.open();
    tracing::info!(
        printer,
        request,
        ticket = ticket.len(),
        "client driver dialog requested"
    );
    let what = format!(
        "DocumentProperties printer={printer} request={request} ticket={}",
        ticket.len()
    );
    if let Err(e) = crate::printer::jobs::call_host(host, what, move |p| async move {
        p.document_properties(printer, request, &ticket).await
    }) {
        dialogs.forget(request);
        tracing::warn!(printer, request, "DocumentProperties failed: {e}");
        return Reply::Failed(e);
    }
    match done.recv_timeout(DIALOG_TIMEOUT) {
        Ok(Done {
            cancelled: true, ..
        }) => {
            tracing::info!(printer, request, "client driver dialog cancelled");
            Reply::Cancelled
        }
        Ok(Done { ticket, .. }) => {
            tracing::info!(
                printer,
                request,
                ticket = ticket.len(),
                "client driver dialog closed"
            );
            Reply::Ticket(ticket)
        }
        Err(_) => {
            dialogs.forget(request);
            tracing::warn!(printer, request, "no answer from the client driver dialog");
            Reply::Failed("no answer from the client".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialogs() {
        let d = Dialogs::default();
        let (a, ra) = d.open();
        let (b, rb) = d.open();
        assert_ne!(a, b);
        assert!(d.done(
            a,
            Done {
                ticket: "<t/>".into(),
                cancelled: false
            }
        ));
        assert_eq!(ra.recv().unwrap().ticket, "<t/>");
        assert!(!d.done(
            a,
            Done {
                ticket: String::new(),
                cancelled: true
            }
        ));
        d.clear();
        assert!(rb.recv().is_err());
    }

    #[test]
    fn the_test_process_is_not_packaged() {
        assert!(package_of(std::process::id()).unwrap().is_none());
    }

    #[test]
    fn only_packages_under_windows_apps() {
        let root = program_files().unwrap();
        let installed =
            format!(r"{root}\WindowsApps\Ameleo.DisplayAgent.Printers_0.1.0.0_x64__8ndmhckkynd76");
        assert!(installed_package(&installed));
        assert!(installed_package(&installed.to_uppercase()));
        assert!(!installed_package(r"C:\Users\someone\dev\print-app"));
        assert!(!installed_package(&format!(r"{root}\WindowsAppsX\a")));
        assert!(!installed_package(&format!(r"{root}\WindowsApps\..\a")));
    }
}
