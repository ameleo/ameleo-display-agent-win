// SPDX-License-Identifier: GPL-3.0-only

//! The virtual file data object in a helper process running as the user.
//!
//! The session agent is SYSTEM, and Explorer (the user) reads the file
//! list from a data object of a SYSTEM process, then silently gives up on
//! its contents. So the data object lives in `ameleo-display-agent
//! clipboard-helper`, started as the logged on user in the session (like
//! rdpclip), which talks to the session agent over two anonymous pipes
//! (its stdin and stdout):
//!
//! * session agent -> helper: `Offer`, `Clear`, and `Reply` to its host
//!   calls;
//! * helper -> session agent: `Call` (list, lock, unlock, file contents),
//!   which the session agent makes to the host.
//!
//! Frames: `len: u32 LE` (kind and payload), `kind: u8`, payload. The
//! helper exits when the session agent closes the pipe.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use proto::{HostClipboardProxy, HostFileClipboardProxy};
use zbus::proxy::CacheProperties;

use super::call::{HostCall, HostReply, ReplyTx, reply_channel};
use super::olecb::{self, HostLink, OleCmd};
use crate::host::Host;
use crate::win::OwnedHandle;

const OFFER: u8 = 1;
const CLEAR: u8 = 2;
const CALL: u8 = 3;
const REPLY: u8 = 4;
/// Larger than any legitimate frame (file list, 4 MiB chunk).
const MAX_FRAME: usize = 64 * 1024 * 1024;

// --- encoding ---------------------------------------------------------------

#[derive(Default)]
struct Out(Vec<u8>);

impl Out {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_le_bytes());
        self
    }
    fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.u32(v.len() as u32);
        self.0.extend_from_slice(v);
        self
    }
    fn str(&mut self, v: &str) -> &mut Self {
        self.bytes(v.as_bytes())
    }
    fn strs(&mut self, v: &[String]) -> &mut Self {
        self.u32(v.len() as u32);
        for s in v {
            self.str(s);
        }
        self
    }
}

struct In<'a>(&'a [u8]);

impl In<'_> {
    fn take(&mut self, n: usize) -> io::Result<&[u8]> {
        if self.0.len() < n {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated frame",
            ));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> io::Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> io::Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn bytes(&mut self) -> io::Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
    fn str(&mut self) -> io::Result<String> {
        String::from_utf8(self.bytes()?).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
    fn strs(&mut self) -> io::Result<Vec<String>> {
        let n = self.u32()? as usize;
        (0..n).map(|_| self.str()).collect()
    }
}

fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut f = Vec::with_capacity(5 + payload.len());
    f.extend_from_slice(&(payload.len() as u32 + 1).to_le_bytes());
    f.push(kind);
    f.extend_from_slice(payload);
    w.write_all(&f)?;
    w.flush()
}

fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("bad frame length {len}"),
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let kind = buf.remove(0);
    Ok((kind, buf))
}

fn encode_call(id: u32, call: &HostCall) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(id);
    match call {
        HostCall::Request { serial, mimes } => o.u8(0).u32(*serial).strs(mimes),
        HostCall::FileContents {
            lindex,
            flags,
            position,
            requested,
            clip_data_id,
        } => o
            .u8(1)
            .u32(*lindex as u32)
            .u32(*flags)
            .u64(*position)
            .u32(*requested)
            .u8(clip_data_id.is_some() as u8)
            .u32(clip_data_id.unwrap_or(0)),
        HostCall::Lock(id) => o.u8(2).u32(*id),
        HostCall::Unlock(id) => o.u8(3).u32(*id),
    };
    o.0
}

fn decode_call(p: &[u8]) -> io::Result<(u32, HostCall)> {
    let mut i = In(p);
    let id = i.u32()?;
    let call = match i.u8()? {
        0 => HostCall::Request {
            serial: i.u32()?,
            mimes: i.strs()?,
        },
        1 => {
            let lindex = i.u32()? as i32;
            let flags = i.u32()?;
            let position = i.u64()?;
            let requested = i.u32()?;
            let has = i.u8()? != 0;
            let clip = i.u32()?;
            HostCall::FileContents {
                lindex,
                flags,
                position,
                requested,
                clip_data_id: has.then_some(clip),
            }
        }
        2 => HostCall::Lock(i.u32()?),
        3 => HostCall::Unlock(i.u32()?),
        t => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown call {t}"),
            ));
        }
    };
    Ok((id, call))
}

fn encode_reply(id: u32, reply: &HostReply) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(id);
    match reply {
        Ok((mime, data)) => o.u8(1).str(mime).bytes(data),
        Err(e) => o.u8(0).str(e),
    };
    o.0
}

fn decode_reply(p: &[u8]) -> io::Result<(u32, HostReply)> {
    let mut i = In(p);
    let id = i.u32()?;
    Ok((
        id,
        if i.u8()? == 1 {
            Ok((i.str()?, i.bytes()?))
        } else {
            Err(i.str()?)
        },
    ))
}

// --- session agent side ---------------------------------------------------

struct Child {
    to_helper: Arc<Mutex<File>>,
    process: OwnedHandle,
    pid: u32,
}

/// The helper process, started on the first offer (a user must be logged
/// on) and again if it died.
pub struct Helper {
    host: Arc<Host>,
    child: Mutex<Option<Child>>,
}

impl Helper {
    pub fn new(host: Arc<Host>) -> Self {
        Helper {
            host,
            child: Mutex::new(None),
        }
    }

    /// The process id of the running helper.
    pub fn pid(&self) -> Option<u32> {
        let child = self.child.lock().unwrap();
        child.as_ref().filter(|c| alive(c)).map(|c| c.pid)
    }

    /// Starts the helper if it is not running: `Some(pid)` when it was
    /// started now.
    pub fn ensure(&self) -> io::Result<Option<u32>> {
        let mut child = self.child.lock().unwrap();
        if child.as_ref().is_some_and(alive) {
            return Ok(None);
        }
        *child = None;
        let c = self.start()?;
        let pid = c.pid;
        *child = Some(c);
        Ok(Some(pid))
    }

    /// Sends `cmd` to the running helper, if any.
    pub fn send(&self, cmd: OleCmd) {
        let payload = match &cmd {
            OleCmd::Offer { serial, mimes } => {
                (OFFER, Out::default().u32(*serial).strs(mimes).0.clone())
            }
            OleCmd::Clear => (CLEAR, Vec::new()),
        };
        let mut child = self.child.lock().unwrap();
        let Some(c) = child.as_ref().filter(|c| alive(c)) else {
            // Nothing on the clipboard from a dead helper.
            *child = None;
            return;
        };
        let res = write_frame(&mut *c.to_helper.lock().unwrap(), payload.0, &payload.1);
        if let Err(e) = res {
            tracing::warn!("clipboard helper: {e}");
            *child = None;
        }
    }

    fn start(&self) -> io::Result<Child> {
        let (process, to_helper, from_helper) = spawn_as_user()?;
        let pid = unsafe { windows_sys::Win32::System::Threading::GetProcessId(process.raw()) };
        let to_helper = Arc::new(Mutex::new(to_helper));
        let writer = to_helper.clone();
        let host = self.host.clone();
        std::thread::Builder::new()
            .name("clipboard-helper-calls".into())
            .spawn(move || relay_calls(from_helper, writer, host))?;
        tracing::info!(pid, "clipboard helper started as the user");
        Ok(Child {
            to_helper,
            process,
            pid,
        })
    }
}

fn alive(c: &Child) -> bool {
    unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(c.process.raw(), 0) != 0 }
}

/// Helper calls to the host: each one made on a thread of its own, the
/// reply written back when it comes (several may be in flight: read-ahead).
fn relay_calls(mut from_helper: File, to_helper: Arc<Mutex<File>>, host: Arc<Host>) {
    loop {
        let (kind, payload) = match read_frame(&mut from_helper) {
            Ok(f) => f,
            Err(e) => {
                tracing::info!("clipboard helper gone: {e}");
                return;
            }
        };
        if kind != CALL {
            continue;
        }
        let Ok((id, call)) = decode_call(&payload) else {
            continue;
        };
        let to_helper = to_helper.clone();
        let host = host.clone();
        let _ = std::thread::Builder::new()
            .name("clipboard-helper-call".into())
            .spawn(move || {
                let what = format!("{call:?}");
                let reply = host_call(&host, call);
                if let Err(e) = &reply {
                    tracing::warn!("host {what} failed: {e}");
                }
                let _ = write_frame(
                    &mut *to_helper.lock().unwrap(),
                    REPLY,
                    &encode_reply(id, &reply),
                );
            });
    }
}

/// Makes `call` to the host and waits for the answer, from a thread of its
/// own.
fn host_call(host: &Host, call: HostCall) -> HostReply {
    let what = format!("{call:?}");
    let rx = host.start(what, move |c| async move {
        let none = || (String::new(), Vec::new());
        let res: zbus::Result<(String, Vec<u8>)> = async {
            match call {
                HostCall::Request { serial, mimes } => {
                    HostClipboardProxy::builder(&c)
                        .cache_properties(CacheProperties::No)
                        .build()
                        .await?
                        .request(serial, mimes)
                        .await
                }
                HostCall::FileContents {
                    lindex,
                    flags,
                    position,
                    requested,
                    clip_data_id,
                } => file_proxy(&c)
                    .await?
                    .file_contents(
                        lindex,
                        flags,
                        position,
                        requested,
                        clip_data_id.unwrap_or(0),
                        clip_data_id.is_some(),
                    )
                    .await
                    .map(|data| (String::new(), data)),
                HostCall::Lock(id) => file_proxy(&c)
                    .await?
                    .lock_clip_data(id)
                    .await
                    .map(|()| none()),
                HostCall::Unlock(id) => file_proxy(&c)
                    .await?
                    .unlock_clip_data(id)
                    .await
                    .map(|()| none()),
            }
        }
        .await;
        res.map_err(|e| e.to_string())
    })?;
    rx.recv()
        .map_err(|_| "the host did not answer".to_string())?
}

async fn file_proxy(c: &zbus::Connection) -> zbus::Result<HostFileClipboardProxy<'_>> {
    HostFileClipboardProxy::builder(c)
        .cache_properties(CacheProperties::No)
        .build()
        .await
}

/// `<exe> clipboard-helper` as the logged on user, stdin / stdout on two pipes.
fn spawn_as_user() -> io::Result<(OwnedHandle, File, File)> {
    use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Environment::{
        CreateEnvironmentBlock, DestroyEnvironmentBlock,
    };
    use windows_sys::Win32::System::Pipes::CreatePipe;
    use windows_sys::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOW,
    };

    use crate::win::wide;

    let token = crate::user::token()?;
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let pipe = || -> io::Result<(HANDLE, HANDLE)> {
        let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
        if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((r, w))
    };
    let (child_in, to_child) = pipe()?;
    let (from_child, child_out) = pipe()?;
    // Only the child's ends are inherited.
    unsafe {
        SetHandleInformation(to_child, HANDLE_FLAG_INHERIT, 0);
        SetHandleInformation(from_child, HANDLE_FLAG_INHERIT, 0);
    }
    let to_child = unsafe { File::from_raw_handle(to_child as RawHandle) };
    let from_child = unsafe { File::from_raw_handle(from_child as RawHandle) };
    let child_in = OwnedHandle(child_in);
    let child_out = OwnedHandle(child_out);

    let exe = std::env::current_exe()?;
    let app = wide(&exe);
    let mut cmd = wide(format!("\"{}\" clipboard-helper", exe.display()));
    let mut desktop = wide(r"winsta0\default");
    let mut si: STARTUPINFOW = unsafe { std::mem::zeroed() };
    si.cb = size_of::<STARTUPINFOW>() as u32;
    si.lpDesktop = desktop.as_mut_ptr();
    si.dwFlags = STARTF_USESTDHANDLES;
    si.hStdInput = child_in.raw();
    si.hStdOutput = child_out.raw();
    si.hStdError = std::ptr::null_mut();
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
    unsafe { CreateEnvironmentBlock(&mut env, token.raw(), 0) };
    let ok = unsafe {
        CreateProcessAsUserW(
            token.raw(),
            app.as_ptr(),
            cmd.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT,
            env,
            std::ptr::null(),
            &si,
            &mut pi,
        )
    };
    let err = io::Error::last_os_error();
    if !env.is_null() {
        unsafe { DestroyEnvironmentBlock(env) };
    }
    if ok == 0 {
        return Err(err);
    }
    drop(OwnedHandle(pi.hThread));
    // child_in / child_out close here: the child has its own copies.
    Ok((OwnedHandle(pi.hProcess), to_child, from_child))
}

// --- helper side ------------------------------------------------------------

/// The helper's host calls, sent to the session agent over stdout.
pub struct Calls {
    to_service: Mutex<File>,
    next: AtomicU32,
    pending: Mutex<HashMap<u32, ReplyTx>>,
}

impl Calls {
    pub fn start(&self, call: HostCall) -> Result<std_mpsc::Receiver<HostReply>, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let payload = encode_call(id, &call);
        let (tx, rx) = reply_channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = write_frame(&mut *self.to_service.lock().unwrap(), CALL, &payload) {
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("service gone: {e}"));
        }
        Ok(rx)
    }
}

/// `ameleo-display-agent clipboard-helper`: runs the virtual file data object
/// for the session agent. Exits when the session agent closes the pipe.
pub fn run_helper() -> anyhow::Result<()> {
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    let mut from_service =
        unsafe { File::from_raw_handle(GetStdHandle(STD_INPUT_HANDLE) as RawHandle) };
    let to_service = unsafe { File::from_raw_handle(GetStdHandle(STD_OUTPUT_HANDLE) as RawHandle) };
    let calls = Arc::new(Calls {
        to_service: Mutex::new(to_service),
        next: AtomicU32::new(1),
        pending: Mutex::new(HashMap::new()),
    });
    let ole = olecb::spawn(HostLink(calls.clone()))?;
    tracing::info!("clipboard helper running");
    loop {
        let (kind, payload) = match read_frame(&mut from_service) {
            Ok(f) => f,
            Err(e) => {
                tracing::info!("service gone, clipboard helper exiting: {e}");
                ole.send(OleCmd::Clear);
                std::thread::sleep(Duration::from_millis(200));
                return Ok(());
            }
        };
        let mut i = In(&payload);
        match kind {
            OFFER => {
                let serial = i.u32()?;
                let mimes = i.strs()?;
                ole.send(OleCmd::Offer { serial, mimes });
            }
            CLEAR => ole.send(OleCmd::Clear),
            REPLY => {
                let (id, reply) = decode_reply(&payload)?;
                if let Some(tx) = calls.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(reply);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_and_replies_round_trip() {
        let calls = [
            HostCall::Request {
                serial: 7,
                mimes: vec!["FileGroupDescriptorW".into()],
            },
            HostCall::FileContents {
                lindex: 3,
                flags: 2,
                position: 1 << 33,
                requested: 65536,
                clip_data_id: Some(9),
            },
            HostCall::FileContents {
                lindex: -1,
                flags: 1,
                position: 0,
                requested: 0,
                clip_data_id: None,
            },
            HostCall::Lock(4),
            HostCall::Unlock(4),
        ];
        for (n, c) in calls.iter().enumerate() {
            let mut wire = Vec::new();
            write_frame(&mut wire, CALL, &encode_call(n as u32, c)).unwrap();
            let (kind, p) = read_frame(&mut wire.as_slice()).unwrap();
            assert_eq!(kind, CALL);
            let (id, back) = decode_call(&p).unwrap();
            assert_eq!(id, n as u32);
            assert_eq!(format!("{back:?}"), format!("{c:?}"));
        }
        for reply in [Ok(("".to_string(), vec![1, 2, 3])), Err("no".to_string())] {
            let (id, back) = decode_reply(&encode_reply(5, &reply)).unwrap();
            assert_eq!((id, back), (5, reply));
        }
    }
}
