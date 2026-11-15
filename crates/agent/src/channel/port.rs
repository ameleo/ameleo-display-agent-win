// SPDX-License-Identifier: GPL-3.0-or-later

//! virtio-serial port I/O and D-Bus session framing.
//!
//! The port is one byte stream that outlives host connections. When the host
//! side reconnects (host process restart, new `Chardev.Register`), a new
//! D-Bus session starts on the same stream with a fresh SASL handshake.
//!
//! The vioserial driver does not complete a pending read when the host goes
//! away, so a disconnect cannot be seen from the read side. Two mechanisms
//! keep the guest in sync:
//!
//! * [`Framer`] follows the D-Bus framing (SASL lines, then message headers).
//!   A NUL byte where a new message would start is the first byte of a new
//!   handshake: that is a new session, whatever happened before.
//! * A monitor thread polls `HostConnected` with `IOCTL_GET_INFORMATION`. On
//!   a disconnect it cancels the pending I/O and resets the framer, so a
//!   message cut in the middle cannot swallow the next handshake.

use std::borrow::Cow;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{
    ERROR_IO_PENDING, ERROR_NO_SYSTEM_RESOURCES, ERROR_OPERATION_ABORTED, GENERIC_READ,
    GENERIC_WRITE, GetLastError,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};

use crate::win::{Event, OwnedHandle, wide};

/// `CTL_CODE(FILE_DEVICE_UNKNOWN, 0x800, METHOD_OUT_DIRECT, FILE_ANY_ACCESS)`
const IOCTL_GET_INFORMATION: u32 = (0x22 << 16) | (0x800 << 2) | 2;
/// Largest D-Bus message (spec limit).
const MAX_MESSAGE: usize = 128 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
const MONITOR_PERIOD: Duration = Duration::from_millis(200);
/// Silence before a read that a new handshake in the middle of a message
/// needs to be taken as one: a message is written at once, the rest of a
/// message cut by a disconnect never comes.
const HANDSHAKE_GAP: Duration = Duration::from_millis(10);

/// `chunk`, a whole read, starts like a client's handshake: the NUL byte
/// followed by `AUTH `. A client that writes the NUL on its own is told
/// from a lone zero byte of a message body by [`Resync`].
fn starts_handshake(chunk: &[u8]) -> bool {
    chunk.starts_with(b"\0AUTH ")
}

/// Spots a host that left in the middle of a message and came back between
/// two polls of the monitor: its new handshake is not the rest of the old
/// message. It must follow a silence and start a read. A read of a lone NUL
/// is held until the next one tells a handshake (`AUTH `) from body bytes.
#[derive(Default)]
struct Resync {
    held_nul: bool,
}

impl Resync {
    /// What to feed `framer` with for `chunk`, read after `gap`; `None`
    /// holds it.
    fn filter<'a>(
        &mut self,
        framer: &mut Framer,
        gap: Duration,
        chunk: &'a [u8],
    ) -> Option<Cow<'a, [u8]>> {
        if std::mem::take(&mut self.held_nul) {
            if chunk.starts_with(b"AUTH ") {
                tracing::warn!(
                    "new D-Bus handshake in the middle of a message: the host reconnected"
                );
                *framer = Framer::default();
            }
            let mut joined = Vec::with_capacity(chunk.len() + 1);
            joined.push(0);
            joined.extend_from_slice(chunk);
            return Some(Cow::Owned(joined));
        }
        if gap >= HANDSHAKE_GAP && !framer.at_boundary() {
            // Held only while the message needs more bytes anyway: its
            // last byte is never delayed.
            if chunk == [0] && framer.needs_more_than(1) {
                self.held_nul = true;
                return None;
            }
            if starts_handshake(chunk) {
                tracing::warn!(
                    "new D-Bus handshake in the middle of a message: the host reconnected"
                );
                *framer = Framer::default();
            }
        }
        Some(Cow::Borrowed(chunk))
    }
}

/// What the reader thread reports to the session manager.
#[derive(Debug)]
pub enum PortEvent {
    /// A new SASL handshake started: drop the previous session.
    NewSession,
    /// Bytes for the current session.
    Data(Vec<u8>),
    /// The host closed its end of the port.
    HostDisconnected,
}

/// Bytes the current session wants to write, tagged with its generation so
/// a late write from a dead session is never sent to the next one.
pub type Outgoing = (u64, Vec<u8>);

pub struct Port {
    handle: OwnedHandle,
    /// Generation of the session allowed to write.
    pub generation: AtomicU64,
    reset_framer: AtomicBool,
}

impl Port {
    /// Opens `\\.\Global\<name>` and starts the reader, writer and monitor
    /// threads.
    pub fn open(
        name: &str,
    ) -> io::Result<(
        Arc<Port>,
        mpsc::UnboundedReceiver<PortEvent>,
        mpsc::Sender<Outgoing>,
    )> {
        let path = wide(format!(r"\\.\Global\{name}"));
        let h = unsafe {
            CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        let port = Arc::new(Port {
            handle: OwnedHandle::new(h)?,
            generation: AtomicU64::new(0),
            reset_framer: AtomicBool::new(false),
        });

        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::channel::<Outgoing>(64);

        {
            let port = port.clone();
            let ev_tx = ev_tx.clone();
            std::thread::Builder::new()
                .name("port-reader".into())
                .spawn(move || port.reader(ev_tx))?;
        }
        {
            let port = port.clone();
            std::thread::Builder::new()
                .name("port-writer".into())
                .spawn(move || port.writer(out_rx))?;
        }
        {
            let port = port.clone();
            std::thread::Builder::new()
                .name("port-monitor".into())
                .spawn(move || port.monitor(ev_tx))?;
        }
        Ok((port, ev_rx, out_tx))
    }

    /// Overlapped I/O helper: issue `op` and wait for completion.
    /// Returns the transferred byte count.
    fn overlapped(
        &self,
        event: &Event,
        op: impl FnOnce(*mut OVERLAPPED) -> i32,
    ) -> io::Result<u32> {
        let mut ov: OVERLAPPED = unsafe { std::mem::zeroed() };
        ov.hEvent = event.raw();
        let ok = op(&mut ov);
        if ok == 0 {
            let err = unsafe { GetLastError() };
            if err != ERROR_IO_PENDING {
                return Err(io::Error::from_raw_os_error(err as i32));
            }
        }
        let mut n = 0u32;
        if unsafe { GetOverlappedResult(self.handle.raw(), &ov, &mut n, 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n)
    }

    fn reader(&self, events: mpsc::UnboundedSender<PortEvent>) {
        let event = Event::new(true).expect("read event");
        let mut buf = vec![0u8; READ_CHUNK];
        let mut framer = Framer::default();
        let mut resync = Resync::default();
        let mut last_read = Instant::now();
        loop {
            let res = self.overlapped(&event, |ov| unsafe {
                ReadFile(
                    self.handle.raw(),
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    std::ptr::null_mut(),
                    ov,
                )
            });
            if self.reset_framer.swap(false, Ordering::AcqRel) {
                framer = Framer::default();
                resync = Resync::default();
            }
            let gap = last_read.elapsed();
            last_read = Instant::now();
            match res {
                Ok(0) => std::thread::sleep(MONITOR_PERIOD),
                Ok(n) => {
                    let Some(chunk) = resync.filter(&mut framer, gap, &buf[..n as usize]) else {
                        continue;
                    };
                    let mut pending: Vec<u8> = Vec::new();
                    framer.feed(&chunk, |out| match out {
                        FrameOut::NewSession => {
                            if !pending.is_empty() {
                                let _ = events.send(PortEvent::Data(std::mem::take(&mut pending)));
                            }
                            let _ = events.send(PortEvent::NewSession);
                        }
                        FrameOut::Bytes(b) => pending.extend_from_slice(b),
                    });
                    if !pending.is_empty() && events.send(PortEvent::Data(pending)).is_err() {
                        return;
                    }
                }
                Err(e) => match e.raw_os_error().map(|c| c as u32) {
                    // Cancelled by the monitor on disconnect, or no host yet.
                    Some(ERROR_OPERATION_ABORTED) | Some(ERROR_NO_SYSTEM_RESOURCES) => {
                        std::thread::sleep(MONITOR_PERIOD)
                    }
                    _ => {
                        tracing::error!("virtio-serial read failed: {e}");
                        std::thread::sleep(Duration::from_secs(1));
                    }
                },
            }
            if events.is_closed() {
                return;
            }
        }
    }

    fn writer(&self, mut out: mpsc::Receiver<Outgoing>) {
        let event = Event::new(true).expect("write event");
        while let Some((generation, data)) = out.blocking_recv() {
            let mut pos = 0;
            while pos < data.len() {
                if generation != self.generation.load(Ordering::Acquire) {
                    break; // the session that queued this is gone
                }
                let chunk = &data[pos..];
                match self.overlapped(&event, |ov| unsafe {
                    WriteFile(
                        self.handle.raw(),
                        chunk.as_ptr(),
                        chunk.len() as u32,
                        std::ptr::null_mut(),
                        ov,
                    )
                }) {
                    Ok(n) => pos += n as usize,
                    Err(e) => {
                        if e.raw_os_error() != Some(ERROR_OPERATION_ABORTED as i32) {
                            tracing::warn!("virtio-serial write failed: {e}");
                        }
                        break;
                    }
                }
            }
        }
    }

    /// `HostConnected` from the vioserial driver.
    fn host_connected(&self, event: &Event) -> io::Result<bool> {
        // VIRTIO_PORT_INFO { UINT Id; BOOLEAN OutVqFull; BOOLEAN HostConnected; ... Name }
        let mut info = [0u8; 512];
        let len = info.len() as u32;
        self.overlapped(event, |ov| unsafe {
            DeviceIoControl(
                self.handle.raw(),
                IOCTL_GET_INFORMATION,
                std::ptr::null(),
                0,
                info.as_mut_ptr().cast(),
                len,
                std::ptr::null_mut(),
                ov,
            )
        })?;
        Ok(info[5] != 0)
    }

    fn monitor(&self, events: mpsc::UnboundedSender<PortEvent>) {
        let event = Event::new(true).expect("ioctl event");
        let mut connected = false;
        loop {
            match self.host_connected(&event) {
                Ok(now) => {
                    if connected && !now {
                        tracing::info!("host disconnected from the port");
                        self.reset_framer.store(true, Ordering::Release);
                        // Kick the pending read and any write blocked on a full queue.
                        unsafe { CancelIoEx(self.handle.raw(), std::ptr::null()) };
                        if events.send(PortEvent::HostDisconnected).is_err() {
                            return;
                        }
                    } else if !connected && now {
                        tracing::info!("host connected to the port");
                    }
                    connected = now;
                }
                Err(e) => tracing::debug!("IOCTL_GET_INFORMATION failed: {e}"),
            }
            if events.is_closed() {
                return;
            }
            std::thread::sleep(MONITOR_PERIOD);
        }
    }
}

pub(crate) enum FrameOut<'a> {
    NewSession,
    Bytes(&'a [u8]),
}

/// Follows the client to server direction of a D-Bus stream.
#[derive(Default)]
pub(crate) enum Framer {
    /// Out of sync: discard until a NUL byte starts a handshake.
    #[default]
    Idle,
    /// SASL lines, until `BEGIN`.
    Auth { line: Vec<u8> },
    /// Collecting the 16 byte fixed header of the next message.
    Header { buf: [u8; 16], have: usize },
    /// Inside a message body.
    Body { remaining: usize },
}

impl Framer {
    pub(crate) fn feed<'a>(&mut self, mut data: &'a [u8], mut out: impl FnMut(FrameOut<'a>)) {
        while !data.is_empty() {
            match self {
                Framer::Idle => match data.iter().position(|&b| b == 0) {
                    Some(i) => {
                        data = &data[i..];
                        self.start_session(&mut data, &mut out);
                    }
                    None => return,
                },
                Framer::Auth { line } => {
                    // Process up to and including the next '\n'.
                    let end = data.iter().position(|&b| b == b'\n').map(|i| i + 1);
                    let take = end.unwrap_or(data.len());
                    if line.is_empty() && data[0] == 0 {
                        self.start_session(&mut data, &mut out);
                        continue;
                    }
                    line.extend_from_slice(&data[..take]);
                    out(FrameOut::Bytes(&data[..take]));
                    data = &data[take..];
                    if end.is_some() {
                        let begin = line.as_slice() == b"BEGIN\r\n";
                        line.clear();
                        if begin {
                            *self = Framer::Header {
                                buf: [0; 16],
                                have: 0,
                            };
                        }
                    } else if line.len() > 4096 {
                        tracing::warn!("SASL line too long, resyncing");
                        *self = Framer::Idle;
                    }
                }
                Framer::Header { buf, have } => {
                    if *have == 0 && data[0] == 0 {
                        self.start_session(&mut data, &mut out);
                        continue;
                    }
                    let take = (16 - *have).min(data.len());
                    buf[*have..*have + take].copy_from_slice(&data[..take]);
                    *have += take;
                    out(FrameOut::Bytes(&data[..take]));
                    data = &data[take..];
                    if *have == 16 {
                        *self = match message_len(buf) {
                            Some(total) => Framer::Body {
                                remaining: total - 16,
                            },
                            None => {
                                tracing::warn!("invalid D-Bus header, resyncing");
                                Framer::Idle
                            }
                        };
                    }
                }
                Framer::Body { remaining } => {
                    let take = (*remaining).min(data.len());
                    out(FrameOut::Bytes(&data[..take]));
                    data = &data[take..];
                    *remaining -= take;
                    if *remaining == 0 {
                        *self = Framer::Header {
                            buf: [0; 16],
                            have: 0,
                        };
                    }
                }
            }
        }
    }

    /// Between two messages or SASL lines, or out of sync: a NUL byte there
    /// is a handshake anyway.
    /// More than `n` bytes are still needed to end the current message or
    /// SASL line.
    fn needs_more_than(&self, n: usize) -> bool {
        match self {
            Framer::Idle => false,
            Framer::Auth { line } => !line.is_empty(), // a line ends with CRLF
            Framer::Header { .. } => true,             // a body follows
            Framer::Body { remaining } => *remaining > n,
        }
    }

    pub(crate) fn at_boundary(&self) -> bool {
        match self {
            Framer::Idle => true,
            Framer::Auth { line } => line.is_empty(),
            Framer::Header { have, .. } => *have == 0,
            Framer::Body { .. } => false,
        }
    }

    /// `data[0]` is the NUL that opens a handshake.
    fn start_session<'a>(&mut self, data: &mut &'a [u8], out: &mut impl FnMut(FrameOut<'a>)) {
        out(FrameOut::NewSession);
        out(FrameOut::Bytes(&data[..1]));
        *data = &data[1..];
        *self = Framer::Auth { line: Vec::new() };
    }
}

/// Total length of a D-Bus message from its fixed header.
fn message_len(h: &[u8; 16]) -> Option<usize> {
    let read = |b: [u8; 4]| match h[0] {
        b'l' => Some(u32::from_le_bytes(b)),
        b'B' => Some(u32::from_be_bytes(b)),
        _ => None,
    };
    if h[3] != 1 {
        return None; // protocol version
    }
    let body = read(h[4..8].try_into().unwrap())? as usize;
    let fields = read(h[12..16].try_into().unwrap())? as usize;
    let header = (16 + fields + 7) & !7;
    let total = header.checked_add(body)?;
    (total <= MAX_MESSAGE).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(framer: &mut Framer, data: &[u8]) -> (usize, Vec<u8>) {
        let mut sessions = 0;
        let mut bytes = Vec::new();
        framer.feed(data, |o| match o {
            FrameOut::NewSession => sessions += 1,
            FrameOut::Bytes(b) => bytes.extend_from_slice(b),
        });
        (sessions, bytes)
    }

    fn message(body: usize) -> Vec<u8> {
        // 'l', type, flags, version, body len, serial, fields len (8)
        let mut m = vec![b'l', 1, 0, 1];
        m.extend_from_slice(&(body as u32).to_le_bytes());
        m.extend_from_slice(&1u32.to_le_bytes());
        m.extend_from_slice(&8u32.to_le_bytes());
        m.extend_from_slice(&[0xAA; 8]); // fields
        m.extend(std::iter::repeat_n(0xBB, body));
        m
    }

    const HELLO: &[u8] = b"\0AUTH ANONYMOUS 6162\r\nBEGIN\r\n";

    #[test]
    fn handshake_then_messages() {
        let mut f = Framer::default();
        let mut stream = b"garbage".to_vec();
        stream.extend_from_slice(HELLO);
        stream.extend(message(5));
        stream.extend(message(0));
        let (sessions, bytes) = run(&mut f, &stream);
        assert_eq!(sessions, 1);
        assert_eq!(bytes, stream[7..]);
        assert!(matches!(f, Framer::Header { have: 0, .. }));
    }

    #[test]
    fn new_handshake_between_messages() {
        let mut f = Framer::default();
        let mut stream = HELLO.to_vec();
        stream.extend(message(3));
        stream.extend_from_slice(HELLO);
        stream.extend(message(1));
        let (sessions, _) = run(&mut f, &stream);
        assert_eq!(sessions, 2);
    }

    #[test]
    fn handshake_after_a_cut_message() {
        // The host left after half of a message and came back: what the
        // reader does with a read that starts a handshake after a silence.
        let mut f = Framer::default();
        let mut stream = HELLO.to_vec();
        let m = message(100);
        stream.extend_from_slice(&m[..60]);
        assert_eq!(run(&mut f, &stream).0, 1);
        assert!(!f.at_boundary());
        assert!(starts_handshake(HELLO));
        assert!(!starts_handshake(b"\0"));
        assert!(!starts_handshake(&m[..10]));
        f = Framer::default();
        let (sessions, bytes) = run(&mut f, HELLO);
        assert_eq!(sessions, 1);
        assert_eq!(bytes, HELLO);
        // Without the reset, the handshake is swallowed by the old body.
        let mut g = Framer::default();
        run(&mut g, &stream);
        assert_eq!(run(&mut g, HELLO).0, 0);
    }

    #[test]
    fn lone_nul_after_a_silence() {
        let silence = HANDSHAKE_GAP * 2;
        let mut m = message(100); // 124 bytes
        m[60] = 0;
        m[123] = 0;
        let mid = |f: &mut Framer| {
            let mut stream = HELLO.to_vec();
            stream.extend_from_slice(&m[..60]);
            run(f, &stream);
        };

        // A zero byte of the body, read alone: held, then fed with the
        // rest of the message, which still ends.
        let mut f = Framer::default();
        let mut r = Resync::default();
        mid(&mut f);
        assert!(r.filter(&mut f, silence, &[0]).is_none());
        let rest = r.filter(&mut f, Duration::ZERO, &m[61..]).unwrap();
        assert_eq!(rest[0], 0);
        assert_eq!(run(&mut f, &rest).0, 0);
        assert!(f.at_boundary());

        // A client writing its NUL alone, then AUTH: a new session.
        let mut f = Framer::default();
        let mut r = Resync::default();
        mid(&mut f);
        assert!(r.filter(&mut f, silence, &HELLO[..1]).is_none());
        let next = r.filter(&mut f, Duration::ZERO, &HELLO[1..]).unwrap();
        assert_eq!(run(&mut f, &next).0, 1);

        // The last byte of a message is never held.
        let mut f = Framer::default();
        let mut r = Resync::default();
        mid(&mut f);
        run(&mut f, &m[60..123]);
        let last = r.filter(&mut f, silence, &m[123..]).unwrap();
        run(&mut f, &last);
        assert!(f.at_boundary());
    }

    #[test]
    fn byte_by_byte() {
        let mut f = Framer::default();
        let mut stream = HELLO.to_vec();
        stream.extend(message(9));
        stream.extend_from_slice(HELLO);
        let mut sessions = 0;
        for b in &stream {
            sessions += run(&mut f, std::slice::from_ref(b)).0;
        }
        assert_eq!(sessions, 2);
    }
}
