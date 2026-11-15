// SPDX-License-Identifier: GPL-3.0-only

//! The H.264 decoder of the client cameras, in a sandboxed child process.
//!
//! The host relays the client's H.264 stream as it is: whoever controls the
//! client controls the bytes the decoder parses. The session agent runs as
//! SYSTEM, so the decoder must not run in it. It runs in
//! `ameleo-display-agent camera-decoder`, started by the session agent:
//!
//! - in an AppContainer without capabilities: low integrity, no
//!   privileges, no network, and only the files, registry keys and objects
//!   opened to all app packages (Windows, Program Files). Store apps decode
//!   H.264 with Media Foundation in the same box;
//! - in a job: no child process, a memory limit, no access to the
//!   clipboard, the desktop or the windows of other processes, and killed
//!   with the job when the agent drops it;
//! - with process mitigations: only Microsoft-signed DLLs, none from the
//!   network or of low integrity, no extension points (AppInit DLLs).
//!
//! The agent writes access units to the decoder's input pipe and reads back
//! NV12 pictures. It checks each picture's size before reading it, copies
//! it into the camera's shared memory itself, and kills a decoder that
//! breaks the protocol or takes too long.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::Path;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use windows_sys::Win32::Foundation::{HANDLE, HANDLE_FLAG_INHERIT, SetHandleInformation};
use windows_sys::Win32::Security::Isolation::{
    CreateAppContainerProfile, DeleteAppContainerProfile, DeriveAppContainerSidFromAppContainerName,
};
use windows_sys::Win32::Security::{FreeSid, PSID, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_BASIC_UI_RESTRICTIONS,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicUIRestrictions,
    JobObjectExtendedLimitInformation, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
    PROCESS_INFORMATION, ResumeThread, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
};

use super::h264::{self, Decoded};
use crate::win::{OwnedHandle, last_error, wide};

/// The AppContainer the decoders run in. Its profile (a folder and a
/// registry key of the account the agent runs as) lives while the agent
/// runs.
const APP_CONTAINER: &str = "ameleo.display.agent.camera-decoder";
/// Tells the decoder process its pipe handles: "input:output".
const PIPES_VAR: &str = "AMELEO_DISPLAY_AGENT_DECODER_PIPES";
/// Largest access unit sent to a decoder.
const MAX_ACCESS_UNIT: usize = 16 << 20;
/// Pictures a decoder may return for one access unit.
const MAX_PICTURES: usize = 4;
/// Time a decoder has for one access unit before it is killed.
const DECODE_TIMEOUT: Duration = Duration::from_secs(2);
/// Memory limit of a decoder process.
const MEMORY_LIMIT: usize = 1 << 30;
/// Requests waiting for the decoder to read them.
const QUEUE: usize = 4;

// Requests, agent to decoder: kind (u8), time (i64), length (u32), data.
const REQ_DECODE: u8 = 0;
const REQ_RESET: u8 = 1;
// Replies, decoder to agent.
/// Width (u32), height (u32), then the NV12 picture, tight.
const REP_PICTURE: u8 = 0;
/// Status (u8): 0 decoded, 1 key frame needed. Ends a decode request.
const REP_DONE: u8 = 1;
/// Length (u16), then a UTF-8 message: the decoder failed and exits.
const REP_FATAL: u8 = 2;

/// All the user interface restrictions of a job (winnt.h JOB_OBJECT_UILIMIT_ALL):
/// handles, clipboard, system parameters, display settings, global atoms,
/// desktops, exiting Windows, IME.
const JOB_UI_LIMIT_ALL: u32 = 0x1FF;

// Process mitigation options (winbase.h), not in windows-sys.
const MITIGATION_HEAP_TERMINATE: u64 = 1 << 12;
const MITIGATION_STRICT_HANDLE_CHECKS: u64 = 1 << 24;
const MITIGATION_EXTENSION_POINT_DISABLE: u64 = 1 << 32;
const MITIGATION_BLOCK_NON_MICROSOFT_BINARIES: u64 = 1 << 44;
const MITIGATION_FONT_DISABLE: u64 = 1 << 48;
const MITIGATION_IMAGE_LOAD_NO_REMOTE: u64 = 1 << 52;
const MITIGATION_IMAGE_LOAD_NO_LOW_LABEL: u64 = 1 << 56;
const MITIGATION_IMAGE_LOAD_PREFER_SYSTEM32: u64 = 1 << 60;

/// A decoder process, with the same interface as [`h264::Decoder`].
/// Dropping it kills the process.
pub struct Decoder {
    requests: std_mpsc::SyncSender<Vec<u8>>,
    replies: std_mpsc::Receiver<Reply>,
    _child: Child,
}

enum Reply {
    Picture {
        width: u32,
        height: u32,
        nv12: Vec<u8>,
    },
    Done(Decoded),
}

impl Decoder {
    /// Starts `ameleo-display-agent camera-decoder` in the sandbox.
    pub fn new() -> Result<Self> {
        let exe = std::env::current_exe()?;
        Self::spawn(&exe, &["camera-decoder"])
    }

    fn spawn(exe: &Path, args: &[&str]) -> Result<Self> {
        let child = Child::spawn(exe, args).context("cannot start the camera decoder")?;
        let (requests, queued) = std_mpsc::sync_channel::<Vec<u8>>(QUEUE);
        let (replied, replies) = std_mpsc::channel();
        let mut input = BufWriter::new(child.input.try_clone()?);
        std::thread::Builder::new()
            .name("decoder-in".into())
            .spawn(move || {
                for req in queued {
                    if input.write_all(&req).and_then(|()| input.flush()).is_err() {
                        break;
                    }
                }
            })?;
        let output = BufReader::new(child.output.try_clone()?);
        std::thread::Builder::new()
            .name("decoder-out".into())
            .spawn(move || read_replies(output, &replied))?;
        Ok(Self {
            requests,
            replies,
            _child: child,
        })
    }

    /// Back to waiting for an IDR.
    pub fn reset(&mut self) {
        let _ = self.requests.try_send(request(REQ_RESET, 0, &[]));
    }

    /// Decodes one access unit; each picture goes to `emit(width, height,
    /// nv12)` (tight, stride = width). An error means the decoder process
    /// is unusable: drop it.
    pub fn decode(
        &mut self,
        au: &[u8],
        time: i64,
        mut emit: impl FnMut(u32, u32, &[u8]),
    ) -> Result<Decoded> {
        if au.len() > MAX_ACCESS_UNIT {
            tracing::warn!(len = au.len(), "camera frame too large, dropped");
            self.reset();
            return Ok(Decoded::NeedKeyFrame);
        }
        self.requests
            .try_send(request(REQ_DECODE, time, au))
            .map_err(|_| anyhow!("the camera decoder stopped reading"))?;
        let deadline = Instant::now() + DECODE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.replies.recv_timeout(left) {
                Ok(Reply::Picture {
                    width,
                    height,
                    nv12,
                }) => emit(width, height, &nv12),
                Ok(Reply::Done(status)) => return Ok(status),
                Err(std_mpsc::RecvTimeoutError::Timeout) => {
                    bail!("the camera decoder took more than {DECODE_TIMEOUT:?}")
                }
                Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("the camera decoder exited")
                }
            }
        }
    }
}

fn request(kind: u8, time: i64, data: &[u8]) -> Vec<u8> {
    let mut req = Vec::with_capacity(13 + data.len());
    req.push(kind);
    req.extend_from_slice(&time.to_le_bytes());
    req.extend_from_slice(&(data.len() as u32).to_le_bytes());
    req.extend_from_slice(data);
    req
}

/// Reads the decoder's replies until it exits or breaks the protocol.
/// Nothing in them is trusted: sizes are checked before anything is read.
fn read_replies(mut output: impl Read, replies: &std_mpsc::Sender<Reply>) {
    let mut pictures = 0;
    loop {
        let mut kind = [0u8];
        if output.read_exact(&mut kind).is_err() {
            return;
        }
        let reply = match kind[0] {
            REP_PICTURE => {
                let mut size = [0u8; 8];
                if output.read_exact(&mut size).is_err() {
                    return;
                }
                let width = u32::from_le_bytes(size[..4].try_into().expect("4 bytes"));
                let height = u32::from_le_bytes(size[4..].try_into().expect("4 bytes"));
                let format = vcam_shm::Format {
                    width,
                    height,
                    fps_num: 1,
                    fps_den: 1,
                };
                pictures += 1;
                if !format.fits() || pictures > MAX_PICTURES {
                    tracing::warn!(width, height, pictures, "camera decoder: bad picture");
                    return;
                }
                let mut nv12 = vec![0; format.nv12_bytes()];
                if output.read_exact(&mut nv12).is_err() {
                    return;
                }
                Reply::Picture {
                    width,
                    height,
                    nv12,
                }
            }
            REP_DONE => {
                let mut status = [0u8];
                if output.read_exact(&mut status).is_err() {
                    return;
                }
                pictures = 0;
                Reply::Done(match status[0] {
                    0 => Decoded::Ok,
                    _ => Decoded::NeedKeyFrame,
                })
            }
            REP_FATAL => {
                let mut len = [0u8; 2];
                let mut msg = vec![0; 0];
                if output.read_exact(&mut len).is_ok() {
                    msg.resize(usize::from(u16::from_le_bytes(len)), 0);
                    let _ = output.read_exact(&mut msg);
                }
                tracing::warn!("camera decoder failed: {}", String::from_utf8_lossy(&msg));
                return;
            }
            other => {
                tracing::warn!("camera decoder: unknown reply {other}");
                return;
            }
        };
        if replies.send(reply).is_err() {
            return;
        }
    }
}

/// `camera-decoder`: the decoder process. Serves the agent on the pipes it
/// was given until the agent closes them.
pub fn run_child() -> Result<()> {
    let (input, output) = child_pipes()?;
    let mut output = BufWriter::new(output);
    let res = serve(BufReader::new(input), &mut output);
    if let Err(e) = &res {
        let msg = format!("{e:#}");
        let msg = &msg.as_bytes()[..msg.len().min(1024)];
        let mut fatal = vec![REP_FATAL];
        fatal.extend_from_slice(&(msg.len() as u16).to_le_bytes());
        fatal.extend_from_slice(msg);
        let _ = output.write_all(&fatal).and_then(|()| output.flush());
    }
    res
}

/// The pipe handles of [`PIPES_VAR`], inherited from the agent.
fn child_pipes() -> Result<(File, File)> {
    let var = std::env::var(PIPES_VAR).context("not started by the agent")?;
    let (input, output) = var
        .split_once(':')
        .and_then(|(i, o)| Some((i.parse::<usize>().ok()?, o.parse::<usize>().ok()?)))
        .context("bad pipe handles")?;
    // Inherited handles: this process owns them.
    unsafe {
        Ok((
            File::from_raw_handle(input as RawHandle),
            File::from_raw_handle(output as RawHandle),
        ))
    }
}

fn serve(mut input: impl Read, output: &mut impl Write) -> Result<()> {
    super::vcam::init_mf()?;
    let mut decoder = h264::Decoder::new()?;
    let mut header = [0u8; 13];
    let mut data = Vec::new();
    loop {
        match input.read_exact(&mut header) {
            Ok(()) => {}
            // The agent closed the pipe: done.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let time = i64::from_le_bytes(header[1..9].try_into().expect("8 bytes"));
        let len = u32::from_le_bytes(header[9..].try_into().expect("4 bytes")) as usize;
        if len > MAX_ACCESS_UNIT {
            bail!("request of {len} bytes");
        }
        data.resize(len, 0);
        input.read_exact(&mut data)?;
        match header[0] {
            REQ_RESET => decoder.reset(),
            REQ_DECODE => {
                let mut sent = Ok(());
                let status = decoder.decode(&data, time, |width, height, nv12| {
                    if sent.is_ok() {
                        sent = write_picture(output, width, height, nv12);
                    }
                });
                sent?;
                let status = status.unwrap_or_else(|_| {
                    decoder.reset();
                    Decoded::NeedKeyFrame
                });
                output.write_all(&[REP_DONE, u8::from(status == Decoded::NeedKeyFrame)])?;
                output.flush()?;
            }
            other => bail!("unknown request {other}"),
        }
    }
}

fn write_picture(output: &mut impl Write, width: u32, height: u32, nv12: &[u8]) -> io::Result<()> {
    output.write_all(&[REP_PICTURE])?;
    output.write_all(&width.to_le_bytes())?;
    output.write_all(&height.to_le_bytes())?;
    output.write_all(nv12)
}

/// A sandboxed child process and the agent's ends of its pipes.
struct Child {
    /// Kills the process when closed.
    _job: OwnedHandle,
    input: File,
    output: File,
}

impl Child {
    fn spawn(exe: &Path, args: &[&str]) -> Result<Self> {
        // The child's ends are inherited, through the handle list only.
        let (child_in, input) = pipe()?;
        let (output, child_out) = pipe()?;
        for parent_end in [&input, &output] {
            unsafe { SetHandleInformation(parent_end.raw(), HANDLE_FLAG_INHERIT, 0) };
        }
        let job = job()?;
        let sid = AppContainerSid::create()?;
        let capabilities = SECURITY_CAPABILITIES {
            AppContainerSid: sid.0,
            Capabilities: std::ptr::null_mut(),
            CapabilityCount: 0,
            Reserved: 0,
        };
        let handles = [child_in.raw(), child_out.raw()];
        let mitigations: u64 = MITIGATION_HEAP_TERMINATE
            | MITIGATION_STRICT_HANDLE_CHECKS
            | MITIGATION_EXTENSION_POINT_DISABLE
            | MITIGATION_BLOCK_NON_MICROSOFT_BINARIES
            | MITIGATION_FONT_DISABLE
            | MITIGATION_IMAGE_LOAD_NO_REMOTE
            | MITIGATION_IMAGE_LOAD_NO_LOW_LABEL
            | MITIGATION_IMAGE_LOAD_PREFER_SYSTEM32;
        let mut attrs = AttributeList::new(3)?;
        attrs.set(PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, &capabilities)?;
        attrs.set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, &handles)?;
        attrs.set(PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY, &mitigations)?;

        let pipes = format!("{}:{}", child_in.raw() as usize, child_out.raw() as usize);
        let env = environment(&pipes);
        let app = wide(exe);
        let mut cmd = format!("\"{}\"", exe.display());
        for a in args {
            cmd.push(' ');
            cmd.push_str(a);
        }
        let mut cmd = wide(cmd);
        let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = attrs.ptr();
        let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            CreateProcessW(
                app.as_ptr(),
                cmd.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT
                    | CREATE_SUSPENDED
                    | CREATE_NO_WINDOW
                    | CREATE_UNICODE_ENVIRONMENT,
                env.as_ptr().cast(),
                std::ptr::null(),
                &si.StartupInfo,
                &mut pi,
            )
        };
        if ok == 0 {
            return Err(last_error()).context("CreateProcess");
        }
        let process = OwnedHandle(pi.hProcess);
        let thread = OwnedHandle(pi.hThread);
        // In the job before it runs a single instruction.
        if unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } == 0 {
            let err = last_error();
            unsafe { TerminateProcess(process.raw(), 1) };
            return Err(err).context("AssignProcessToJobObject");
        }
        if unsafe { ResumeThread(thread.raw()) } == u32::MAX {
            return Err(last_error()).context("ResumeThread");
        }
        // child_in and child_out close here: the child has its copies.
        Ok(Self {
            _job: job,
            input: unsafe { File::from_raw_handle(into_raw(input)) },
            output: unsafe { File::from_raw_handle(into_raw(output)) },
        })
    }
}

fn into_raw(h: OwnedHandle) -> RawHandle {
    let raw = h.raw();
    std::mem::forget(h);
    raw as RawHandle
}

/// An anonymous pipe, (read end, write end), both inheritable.
fn pipe() -> Result<(OwnedHandle, OwnedHandle)> {
    let sa = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let (mut r, mut w): (HANDLE, HANDLE) = (std::ptr::null_mut(), std::ptr::null_mut());
    if unsafe { CreatePipe(&mut r, &mut w, &sa, 0) } == 0 {
        return Err(last_error()).context("CreatePipe");
    }
    Ok((OwnedHandle(r), OwnedHandle(w)))
}

/// A job that kills its process when closed and holds it to one process,
/// [`MEMORY_LIMIT`], and no access to the user interface of others.
fn job() -> Result<OwnedHandle> {
    let job = OwnedHandle::new(unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) })
        .context("CreateJobObject")?;
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.ProcessMemoryLimit = MEMORY_LIMIT;
    let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
        UIRestrictionsClass: JOB_UI_LIMIT_ALL,
    };
    unsafe {
        if SetInformationJobObject(
            job.raw(),
            JobObjectExtendedLimitInformation,
            (&raw const limits).cast(),
            size_of_val(&limits) as u32,
        ) == 0
            || SetInformationJobObject(
                job.raw(),
                JobObjectBasicUIRestrictions,
                (&raw const ui).cast(),
                size_of_val(&ui) as u32,
            ) == 0
        {
            return Err(last_error()).context("SetInformationJobObject");
        }
    }
    Ok(job)
}

/// Our environment plus [`PIPES_VAR`], as a UTF-16 environment block.
fn environment(pipes: &str) -> Vec<u16> {
    let mut block = Vec::new();
    for (k, v) in std::env::vars_os().filter(|(k, _)| k != PIPES_VAR) {
        let mut entry = k;
        entry.push("=");
        entry.push(v);
        block.extend(wide(entry));
    }
    block.extend(wide(format!("{PIPES_VAR}={pipes}")));
    block.push(0);
    block
}

/// The SID of [`APP_CONTAINER`].
struct AppContainerSid(PSID);

impl AppContainerSid {
    /// Creates the AppContainer profile of the account we run as, or takes
    /// the existing one: Windows starts no process in an AppContainer
    /// without a profile.
    fn create() -> Result<Self> {
        // HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)
        const ALREADY_EXISTS: i32 = 0x800700B7_u32 as i32;
        let name = wide(APP_CONTAINER);
        let display = wide("Ameleo Display Agent camera decoder");
        let mut sid: PSID = std::ptr::null_mut();
        let hr = unsafe {
            CreateAppContainerProfile(
                name.as_ptr(),
                display.as_ptr(),
                display.as_ptr(),
                std::ptr::null(),
                0,
                &mut sid,
            )
        };
        if hr >= 0 {
            return Ok(Self(sid));
        }
        if hr != ALREADY_EXISTS {
            bail!("CreateAppContainerProfile failed: {hr:#010x}");
        }
        let hr = unsafe { DeriveAppContainerSidFromAppContainerName(name.as_ptr(), &mut sid) };
        if hr < 0 {
            bail!("DeriveAppContainerSidFromAppContainerName failed: {hr:#010x}");
        }
        Ok(Self(sid))
    }
}

/// Removes the AppContainer profile of the decoders, for the account we run
/// as. The agent does it when it stops; the next decoder creates it again.
pub fn delete_profile() {
    unsafe { DeleteAppContainerProfile(wide(APP_CONTAINER).as_ptr()) };
}

impl Drop for AppContainerSid {
    fn drop(&mut self) {
        unsafe { FreeSid(self.0) };
    }
}

/// A process creation attribute list. The values it points to must outlive
/// it.
struct AttributeList(Vec<usize>);

impl AttributeList {
    fn new(count: u32) -> Result<Self> {
        let mut size = 0;
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        let mut buf = vec![0usize; size.div_ceil(size_of::<usize>())];
        if unsafe {
            InitializeProcThreadAttributeList(buf.as_mut_ptr().cast(), count, 0, &mut size)
        } == 0
        {
            return Err(last_error()).context("InitializeProcThreadAttributeList");
        }
        Ok(Self(buf))
    }

    fn ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    fn set<T: ?Sized>(&mut self, attribute: u32, value: &T) -> Result<()> {
        if unsafe {
            UpdateProcThreadAttribute(
                self.ptr(),
                0,
                attribute as usize,
                (value as *const T).cast(),
                size_of_val(value),
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        } == 0
        {
            return Err(last_error()).context("UpdateProcThreadAttribute");
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::h264::{Encoder, is_key_frame};
    use crate::camera::vcam::test_pattern;
    use vcam_shm::Format;

    /// Runs this test binary as the sandboxed child, in `test`.
    fn sandboxed(test: &str) -> Vec<&str> {
        vec![test, "--exact", "--test-threads=1"]
    }

    fn test_exe() -> std::path::PathBuf {
        std::env::current_exe().unwrap()
    }

    /// The decoder process of the tests: this test, run by
    /// [`Decoder::spawn`] in the sandbox. Run by the harness, it does
    /// nothing.
    #[test]
    fn decoder_child() {
        if std::env::var_os(PIPES_VAR).is_some() {
            std::process::exit(i32::from(run_child().is_err()));
        }
    }

    #[test]
    fn decodes_in_the_sandbox() {
        super::super::vcam::init_mf().unwrap();
        let f = Format {
            width: 320,
            height: 240,
            fps_num: 30,
            fps_den: 1,
        };
        let mut enc = Encoder::new(f.width, f.height, 30, 1, 1_000_000).unwrap();
        let args = sandboxed("camera::sandbox::tests::decoder_child");
        let mut dec = Decoder::spawn(&test_exe(), &args).unwrap();
        let mut frame = vec![0u8; f.nv12_bytes()];
        let mut aus = Vec::new();
        for t in 0..20 {
            test_pattern(&f, t, &mut frame);
            aus.extend(enc.encode(&frame).unwrap());
        }
        // Joining on a P frame: dropped, a key frame is needed.
        let p = aus.iter().find(|au| !is_key_frame(au)).expect("a P frame");
        assert_eq!(
            dec.decode(p, 0, |_, _, _| panic!("decoded a P frame first"))
                .unwrap(),
            Decoded::NeedKeyFrame
        );
        let mut decoded = 0;
        for au in &aus {
            let st = dec
                .decode(au, 0, |w, h, nv12| {
                    assert_eq!((w, h), (f.width, f.height));
                    assert_eq!(nv12.len(), f.nv12_bytes());
                    decoded += 1;
                })
                .unwrap();
            assert_eq!(st, Decoded::Ok);
        }
        assert!(decoded >= 10, "decoded {decoded} of {}", aus.len());
        // Garbage: the decoder process survives it.
        let garbage = [0, 0, 0, 1, 0x65, 0xFF, 0x00, 0x13, 0x37, 0x42];
        dec.decode(&garbage, 0, |_, _, _| {}).unwrap();
        dec.reset();
        assert_eq!(
            dec.decode(&aus[0], 0, |_, _, _| {}).unwrap(),
            Decoded::Ok,
            "decodes again after a reset"
        );
    }

    /// What the sandbox denies, tried by [`probe_child`].
    const PROBE_FILE: &str = "ameleo-display-agent-sandbox-probe.txt";

    /// Tries what a compromised decoder would, and writes one byte per try
    /// to its output pipe: 1 when denied.
    #[test]
    fn probe_child() {
        if std::env::var_os(PIPES_VAR).is_none() {
            return;
        }
        let (_input, mut output) = child_pipes().unwrap();
        // Not the temp directory: Windows gives an AppContainer its own.
        let dir = test_exe().with_file_name("");
        let read = std::fs::read(dir.join(PROBE_FILE)).is_err();
        let write = std::fs::write(dir.join("sandbox-probe-write.txt"), b"x").is_err();
        let spawn = match std::process::Command::new(test_exe()).arg("--list").spawn() {
            Ok(mut c) => {
                let _ = c.kill();
                false
            }
            Err(_) => true,
        };
        let net = std::net::TcpStream::connect_timeout(
            &"1.1.1.1:443".parse().unwrap(),
            Duration::from_secs(2),
        )
        .is_err();
        output
            .write_all(&[
                u8::from(read),
                u8::from(write),
                u8::from(spawn),
                u8::from(net),
            ])
            .unwrap();
        std::process::exit(0);
    }

    #[test]
    fn sandbox_denies() {
        let file = test_exe().with_file_name(PROBE_FILE);
        std::fs::write(&file, b"secret").unwrap();
        let mut child = Child::spawn(
            &test_exe(),
            &sandboxed("camera::sandbox::tests::probe_child"),
        )
        .unwrap();
        let mut denied = [0u8; 4];
        child.output.read_exact(&mut denied).unwrap();
        let _ = std::fs::remove_file(&file);
        assert_eq!(
            denied,
            [1, 1, 1, 1],
            "read a user file, write a user file, spawn, network"
        );
    }
}
