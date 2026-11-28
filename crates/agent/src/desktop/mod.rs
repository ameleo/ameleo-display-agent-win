// SPDX-License-Identifier: GPL-3.0-only

//! The desktop thread: everything that needs a window or the input desktop.
//!
//! One thread owns a hidden window on the current input desktop. It injects
//! the mouse, owns the clipboard for the host (delayed rendering), listens
//! for clipboard changes and session events, and applies desktop effects.
//!
//! When the input desktop switches (logon screen, Ctrl+Alt+Del, UAC secure
//! desktop), the window is destroyed, the thread moves to the new desktop
//! and the window is recreated. Clipboard ownership held for the host is
//! re-announced on the new window.
//!
//! The rest of the agent talks to this thread with [`Cmd`] and gets
//! [`Notify`] back.

pub mod display_settings;
pub mod mouse;

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use proto::HostClipboardProxy;
use tokio::sync::{mpsc, oneshot};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, EmptyClipboard, GetClipboardOwner, RemoveClipboardFormatListener,
    SetClipboardData,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    UOI_NAME,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcessId, GetCurrentThreadId, OpenEventW, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowThreadProcessId,
    MSG, MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT,
    RegisterClassW, TranslateMessage, WM_CLIPBOARDUPDATE, WM_DESTROYCLIPBOARD, WM_DISPLAYCHANGE,
    WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_WTSSESSION_CHANGE, WNDCLASSW,
};
use zbus::proxy::CacheProperties;

use crate::clipboard;
use crate::feature::ClipboardFormats;
use crate::host::{CallFn, EmitFn, Host, HostReply};
use crate::win::{Event, from_wide, wide};
use display_settings::Effects;
use mouse::Mouse;

const CLASS_NAME: &str = "AmeleoDisplayAgentDesktop";
const RENDER_TIMEOUT: Duration = Duration::from_secs(5);
const GENERIC_ALL: u32 = 0x1000_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
// WM_WTSSESSION_CHANGE reasons.
const WTS_SESSION_LOGON: usize = 5;
const WTS_SESSION_LOGOFF: usize = 6;
const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;

/// Requests to the desktop thread.
pub enum Cmd {
    Move {
        head: u32,
        x: u32,
        y: u32,
    },
    Button {
        button: u32,
        down: bool,
    },
    Scroll {
        dx: i32,
        dy: i32,
    },
    /// The host owns the clipboard.
    Grab {
        serial: u32,
        mimes: Vec<String>,
    },
    /// The host gives the clipboard up.
    Release,
    /// Read the guest clipboard. The reply carries the guest clipboard
    /// serial the content belongs to.
    Request {
        mimes: Vec<String>,
        reply: oneshot::Sender<Result<(u32, clipboard::Content), String>>,
    },
    SetEffects(Effects),
    /// New D-Bus session or remote client gone: forget host state, release
    /// buttons left down and re-announce what the guest owns.
    Reset,
    /// A D-Bus session is up: tell the host what the guest clipboard holds.
    Announce,
}

/// Events from the desktop thread.
pub enum Notify {
    /// The guest clipboard holds new data.
    ClipboardGrabbed {
        serial: u32,
        mimes: Vec<String>,
    },
    /// The guest clipboard is empty.
    ClipboardReleased,
    SessionLocked(bool),
    /// A call to the host (see [`Host::start`]), answered on `reply`.
    Call {
        what: String,
        call: CallFn,
        reply: std_mpsc::SyncSender<HostReply>,
    },
    /// A signal to the host (see [`Host::emit`]).
    Emit(EmitFn),
}

#[derive(Clone)]
pub struct DesktopHandle {
    tx: std_mpsc::Sender<Cmd>,
    wake: Arc<Event>,
}

impl DesktopHandle {
    pub fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            self.wake.set();
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Owner {
    None,
    Guest {
        mimes: Vec<String>,
    },
    /// `taken`: the format provider that put the host's data on the clipboard
    /// itself (index in `Desktop::formats`); otherwise our window offers it
    /// with delayed rendering.
    Host {
        serial: u32,
        mimes: Vec<String>,
        taken: Option<usize>,
    },
}

struct Desktop {
    hwnd: HWND,
    mouse: Mouse,
    owner: Owner,
    guest_serial: u32,
    png: u32,
    locked: bool,
    logon_pending: bool,
    effects: Effects,
    notify: mpsc::UnboundedSender<Notify>,
    host: Host,
    /// The clipboard formats of the features (the file list).
    formats: Vec<Arc<dyn ClipboardFormats>>,
}

thread_local! {
    static DESKTOP: RefCell<Option<Desktop>> = const { RefCell::new(None) };
}

/// Starts the desktop thread.
pub fn spawn(
    notify: mpsc::UnboundedSender<Notify>,
    formats: Vec<Arc<dyn ClipboardFormats>>,
) -> std::io::Result<DesktopHandle> {
    let wake = Arc::new(Event::new(false)?);
    let (tx, rx) = std_mpsc::channel();
    let switched = Arc::new(AtomicBool::new(false));

    {
        let wake = wake.clone();
        let switched = switched.clone();
        std::thread::Builder::new()
            .name("desktop-switch".into())
            .spawn(move || watch_desktop_switch(&wake, &switched))?;
    }
    {
        let wake = wake.clone();
        std::thread::Builder::new()
            .name("desktop".into())
            .spawn(move || run(rx, &wake, &switched, notify, formats))?;
    }
    Ok(DesktopHandle { tx, wake })
}

/// `WinSta0_DesktopSwitch` is pulsed, so it gets its own waiting thread.
fn watch_desktop_switch(wake: &Event, switched: &AtomicBool) {
    let name = wide("WinSta0_DesktopSwitch");
    let ev = unsafe { OpenEventW(SYNCHRONIZE, 0, name.as_ptr()) };
    if ev.is_null() {
        tracing::warn!(
            "cannot open WinSta0_DesktopSwitch: {}",
            std::io::Error::last_os_error()
        );
        return;
    }
    loop {
        if unsafe { WaitForSingleObject(ev, u32::MAX) } == WAIT_OBJECT_0 {
            switched.store(true, Ordering::Release);
            wake.set();
        }
    }
}

fn run(
    rx: std_mpsc::Receiver<Cmd>,
    wake: &Event,
    switched: &AtomicBool,
    notify: mpsc::UnboundedSender<Notify>,
    formats: Vec<Arc<dyn ClipboardFormats>>,
) {
    let class = wide(CLASS_NAME);
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    if unsafe { RegisterClassW(&wc) } == 0 {
        tracing::error!("RegisterClass failed: {}", std::io::Error::last_os_error());
        return;
    }

    let effects = display_settings::load();
    let mut first = true;
    loop {
        let desktop_name = attach_input_desktop();
        let on_logon_screen = desktop_name.eq_ignore_ascii_case("Winlogon");
        tracing::info!("input desktop: {desktop_name}");

        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                wc.hInstance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            tracing::error!("CreateWindow failed: {}", std::io::Error::last_os_error());
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        unsafe {
            WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION);
            AddClipboardFormatListener(hwnd);
        }

        DESKTOP.with_borrow_mut(|d| {
            let d = d.get_or_insert_with(|| Desktop {
                hwnd,
                mouse: Mouse::new(),
                owner: Owner::None,
                guest_serial: 0,
                png: clipboard::png_format(),
                locked: on_logon_screen,
                logon_pending: false,
                effects: effects.unwrap_or_default(),
                notify: notify.clone(),
                host: Host::new(notify.clone()),
                formats: formats.clone(),
            });
            d.hwnd = hwnd;
            d.mouse.refresh();
            // A lock (or an unlock) the notification missed, the window
            // being replaced as the input desktop switched.
            if let Some(locked) = session_locked_now() {
                d.set_locked(locked);
            }
            if first {
                let _ = d.notify.send(Notify::SessionLocked(d.locked));
            }
            // Effects are per user session: apply once on the user desktop,
            // and again after each logon.
            if !on_logon_screen && (first || d.logon_pending) {
                d.logon_pending = false;
                if effects.is_some() {
                    display_settings::apply(d.effects);
                } else {
                    display_settings::grayscale_font_smoothing();
                }
            }
            // Delayed formats died with the old window. What a feature
            // put on the clipboard does not depend on our window.
            if let Owner::Host {
                serial,
                mimes,
                taken: None,
            } = d.owner.clone()
            {
                d.host_grab(serial, mimes);
            }
        });
        first = false;

        // Message loop until the input desktop changes.
        while !switched.swap(false, Ordering::AcqRel) {
            let handles = [wake.raw()];
            unsafe {
                MsgWaitForMultipleObjectsEx(
                    1,
                    handles.as_ptr(),
                    u32::MAX,
                    QS_ALLINPUT,
                    MWMO_INPUTAVAILABLE,
                );
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            process_commands(&rx);
        }

        unsafe {
            RemoveClipboardFormatListener(hwnd);
            WTSUnRegisterSessionNotification(hwnd);
            DestroyWindow(hwnd);
        }
    }
}

/// The name of the current input desktop; with `attach`, the calling thread
/// moves to it first, which it can only while it has no window.
pub fn input_desktop_name(attach: bool) -> String {
    unsafe {
        let hdesk = OpenInputDesktop(0, 0, GENERIC_ALL);
        if hdesk.is_null() {
            return desktop_name(GetThreadDesktop(GetCurrentThreadId()));
        }
        if attach && SetThreadDesktop(hdesk) == 0 {
            tracing::debug!(
                "SetThreadDesktop failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let name = desktop_name(hdesk);
        CloseDesktop(hdesk);
        name
    }
}

/// Whether Windows says the session is locked: `None` when it does not say.
/// The lock notification alone misses locks: `LockWorkStation` switches the
/// input desktop, and the window that would hear it is being replaced.
fn session_locked_now() -> Option<bool> {
    use windows_sys::Win32::System::RemoteDesktop::{
        WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTSFreeMemory, WTSINFOEXW,
        WTSQuerySessionInformationW, WTSSessionInfoEx,
    };
    // WTSINFOEX_LEVEL1_W.SessionFlags.
    const WTS_SESSIONSTATE_LOCK: i32 = 0;
    const WTS_SESSIONSTATE_UNLOCK: i32 = 1;
    let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut len = 0u32;
    let ok = unsafe {
        WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buf,
            &mut len,
        )
    };
    if ok == 0 || buf.is_null() {
        return None;
    }
    let flags = unsafe {
        let info = &*(buf as *const WTSINFOEXW);
        (info.Level == 1).then_some(info.Data.WTSInfoExLevel1.SessionFlags)
    };
    unsafe { WTSFreeMemory(buf.cast()) };
    match flags? {
        WTS_SESSIONSTATE_LOCK => Some(true),
        WTS_SESSIONSTATE_UNLOCK => Some(false),
        _ => None,
    }
}

/// Moves the thread to the current input desktop, returns its name.
fn attach_input_desktop() -> String {
    unsafe {
        let hdesk = OpenInputDesktop(0, 0, GENERIC_ALL);
        if hdesk.is_null() {
            // Not SYSTEM (debug run): stay where we are.
            tracing::debug!(
                "OpenInputDesktop failed: {}",
                std::io::Error::last_os_error()
            );
            return desktop_name(GetThreadDesktop(GetCurrentThreadId()));
        }
        if SetThreadDesktop(hdesk) == 0 {
            tracing::warn!(
                "SetThreadDesktop failed: {}",
                std::io::Error::last_os_error()
            );
        }
        let name = desktop_name(hdesk);
        CloseDesktop(hdesk);
        name
    }
}

fn desktop_name(hdesk: windows_sys::Win32::System::StationsAndDesktops::HDESK) -> String {
    let mut buf = [0u16; 256];
    let mut needed = 0u32;
    let ok = unsafe {
        GetUserObjectInformationW(
            hdesk,
            UOI_NAME,
            buf.as_mut_ptr().cast(),
            (buf.len() * 2) as u32,
            &mut needed,
        )
    };
    if ok == 0 {
        String::new()
    } else {
        from_wide(&buf)
    }
}

fn process_commands(rx: &std_mpsc::Receiver<Cmd>) {
    let cmds: Vec<Cmd> = rx.try_iter().collect();
    let skip: Vec<bool> = (0..cmds.len()).map(|i| move_superseded(&cmds, i)).collect();
    DESKTOP.with_borrow_mut(|d| {
        let Some(d) = d else { return };
        for (cmd, skip) in cmds.into_iter().zip(skip) {
            match cmd {
                Cmd::Move { .. } if skip => {}
                Cmd::Move { head, x, y } => {
                    tracing::trace!(head, x, y, "mouse move injected");
                    d.mouse.move_to(head, x, y)
                }
                Cmd::Button { button, down } => d.mouse.button(button, down),
                Cmd::Scroll { dx, dy } => d.mouse.scroll(dx, dy),
                Cmd::Grab { serial, mimes } => d.host_grab(serial, mimes),
                Cmd::Release => {
                    tracing::info!("host released the clipboard");
                    d.host_release()
                }
                Cmd::Request { mimes, reply } => {
                    let res = if matches!(d.owner, Owner::Host { .. }) {
                        Err("the host owns the clipboard".to_string())
                    } else {
                        clipboard::read(d.hwnd, &mimes, d.png, &d.formats, d.guest_serial)
                    };
                    match &res {
                        Ok(clipboard::Content::Data(m, data)) => {
                            tracing::info!(?mimes, mime = %m, bytes = data.len(), "host read the guest clipboard")
                        }
                        Ok(clipboard::Content::Deferred(m, _)) => {
                            tracing::info!(?mimes, mime = %m, "host read the guest clipboard (feature)")
                        }
                        Err(e) => tracing::warn!(?mimes, "host read of the guest clipboard failed: {e}"),
                    }
                    let _ = reply.send(res.map(|c| (d.guest_serial, c)));
                }
                Cmd::SetEffects(e) => {
                    d.effects = e;
                    display_settings::save(e);
                    display_settings::apply(e);
                }
                Cmd::Reset => {
                    d.mouse.release_all();
                    d.host_release();
                    d.announce_guest();
                }
                Cmd::Announce => d.announce_guest(),
            }
        }
    });
}

/// A move followed directly by another move is never seen: only the last
/// position of a run of moves matters. A move before a button or wheel
/// event is kept, the event happens where the host saw the pointer.
fn move_superseded(cmds: &[Cmd], i: usize) -> bool {
    matches!(cmds[i], Cmd::Move { .. }) && matches!(cmds.get(i + 1), Some(Cmd::Move { .. }))
}

impl Desktop {
    /// Re-announces guest clipboard ownership under a new serial.
    fn announce_guest(&mut self) {
        if let Owner::Guest { mimes } = self.owner.clone() {
            self.guest_grabbed(mimes);
        }
    }

    /// Announces the guest clipboard to the host under a new serial.
    fn guest_grabbed(&mut self, mimes: Vec<String>) {
        self.guest_serial += 1;
        for f in &self.formats {
            f.guest_changed(self.guest_serial);
        }
        let _ = self.notify.send(Notify::ClipboardGrabbed {
            serial: self.guest_serial,
            mimes,
        });
    }

    /// Offers the host's data: through a feature that takes it,
    /// otherwise with delayed rendering on our window.
    fn host_grab(&mut self, serial: u32, mimes: Vec<String>) {
        let previous = match self.owner {
            Owner::Host { taken, .. } => taken,
            _ => None,
        };
        if let Some(i) = self
            .formats
            .iter()
            .position(|f| f.host_grab(serial, &mimes))
        {
            // It takes the clipboard over from our window if we had it.
            tracing::info!(serial, ?mimes, "host owns the clipboard, through a feature");
            if let Some(p) = previous.filter(|&p| p != i) {
                self.formats[p].host_release();
            }
            self.owner = Owner::Host {
                serial,
                mimes,
                taken: Some(i),
            };
            return;
        }
        if let Some(p) = previous {
            self.formats[p].host_release();
        }
        let formats = clipboard::formats_for(&mimes, self.png);
        if formats.is_empty() {
            tracing::warn!(serial, ?mimes, "host grab without supported types, ignored");
            return;
        }
        let Some(_guard) = clipboard::OpenGuard::open(self.hwnd) else {
            tracing::warn!("cannot open the clipboard for a host grab");
            return;
        };
        tracing::info!(serial, ?mimes, ?formats, "host owns the clipboard");
        unsafe {
            EmptyClipboard();
            for f in formats {
                SetClipboardData(f, std::ptr::null_mut());
            }
        }
        self.owner = Owner::Host {
            serial,
            mimes,
            taken: None,
        };
    }

    fn host_release(&mut self) {
        let Owner::Host { taken, .. } = self.owner else {
            return;
        };
        self.owner = Owner::None;
        if let Some(i) = taken {
            self.formats[i].host_release();
            return;
        }
        if let Some(_guard) = clipboard::OpenGuard::open(self.hwnd)
            && unsafe { GetClipboardOwner() } == self.hwnd
        {
            unsafe { EmptyClipboard() };
        }
    }

    fn clipboard_changed(&mut self) {
        if self.owned_by_us() {
            return; // our own grab, on our window or a feature's
        }
        let mut mimes: Vec<String> = self.formats.iter().flat_map(|f| f.guest_mimes()).collect();
        mimes.extend(clipboard::available_mimes(self.png));
        tracing::info!(?mimes, "guest clipboard changed");
        if !mimes.is_empty() {
            self.owner = Owner::Guest {
                mimes: mimes.clone(),
            };
            self.guest_grabbed(mimes);
        } else if self.owner != Owner::None {
            if matches!(self.owner, Owner::Guest { .. }) {
                let _ = self.notify.send(Notify::ClipboardReleased);
            }
            self.owner = Owner::None;
        }
    }

    fn session_change(&mut self, reason: usize) {
        match reason {
            WTS_SESSION_LOCK => self.set_locked(true),
            WTS_SESSION_UNLOCK => self.set_locked(false),
            WTS_SESSION_LOGON => {
                self.logon_pending = true;
                self.set_locked(false);
            }
            // The logon screen shows: as locked, for what it gives out.
            WTS_SESSION_LOGOFF => self.set_locked(true),
            _ => {}
        }
    }

    fn set_locked(&mut self, locked: bool) {
        if self.locked != locked {
            self.locked = locked;
            let _ = self.notify.send(Notify::SessionLocked(locked));
        }
    }

    /// The clipboard owner window belongs to this process, or to
    /// a feature's helper.
    fn owned_by_us(&self) -> bool {
        let owner = unsafe { GetClipboardOwner() };
        if owner.is_null() {
            return false;
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(owner, &mut pid) };
        pid == unsafe { GetCurrentProcessId() } || self.formats.iter().any(|f| f.owns(pid))
    }
}

/// Fetches host data for `format` (called inside WM_RENDERFORMAT, so the
/// pasting app is blocked until we answer).
fn render(format: u32) {
    let request = DESKTOP.with(|cell| {
        let d = cell.try_borrow().ok()?;
        let d = d.as_ref()?;
        let Owner::Host {
            serial,
            mimes: host_mimes,
            ..
        } = &d.owner
        else {
            tracing::warn!(
                format,
                "paste of a host format while the host does not own the clipboard"
            );
            return None;
        };
        // Ask with the host's own spelling of the type: the grab accepted
        // several text aliases.
        let mimes = clipboard::host_mimes_for(format, d.png, host_mimes);
        if mimes.is_empty() {
            tracing::warn!(format, ?host_mimes, "no host type for the pasted format");
            return None;
        }
        Some((*serial, mimes, d.png, d.host.clone()))
    });
    let Some((serial, mimes, png, host)) = request else {
        return;
    };

    tracing::info!(serial, format, ?mimes, "guest paste: asking the host");
    let what = format!("Request {mimes:?}");
    let rx = host.start(what, move |conn| async move {
        let p = HostClipboardProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|e| e.to_string())?;
        p.request(serial, mimes).await.map_err(|e| e.to_string())
    });
    // The pasting app waits on us: give up well before it looks hung.
    // Failures are logged by the session.
    let data = match rx.map(|rx| rx.recv_timeout(RENDER_TIMEOUT)) {
        Ok(Ok(Ok(data))) => data,
        Ok(Ok(Err(_))) | Err(_) => return,
        Ok(Err(_)) => {
            tracing::warn!(
                serial,
                "guest paste: no host answer within {RENDER_TIMEOUT:?}"
            );
            return;
        }
    };
    tracing::info!(serial, mime = %data.0, bytes = data.1.len(), "guest paste: host data received");
    match clipboard::to_handle(format, png, &data.0, &data.1) {
        Ok(h) => unsafe {
            if SetClipboardData(format, h).is_null() {
                tracing::warn!(
                    "SetClipboardData({format}) failed: {}",
                    std::io::Error::last_os_error()
                );
            }
        },
        Err(e) => tracing::warn!("cannot render clipboard format {format}: {e}"),
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Calls made while handling a command may send messages back to this
    // window synchronously, so the state may already be borrowed: such
    // re-entrant messages are skipped.
    let with = |f: &mut dyn FnMut(&mut Desktop)| {
        DESKTOP.with(|cell| {
            if let Ok(mut d) = cell.try_borrow_mut()
                && let Some(d) = d.as_mut()
            {
                f(d);
            }
        })
    };
    match msg {
        WM_CLIPBOARDUPDATE => with(&mut |d| d.clipboard_changed()),
        WM_RENDERFORMAT => render(wparam as u32),
        // The window goes away with a host grab: the formats are lost, the
        // grab is replayed on the next window.
        WM_RENDERALLFORMATS => {}
        // Only about our window: what a feature put on the clipboard is
        // not ours to forget here, a foreign copy shows up as
        // WM_CLIPBOARDUPDATE.
        WM_DESTROYCLIPBOARD => with(&mut |d| {
            if matches!(d.owner, Owner::Host { taken: None, .. }) {
                d.owner = Owner::None;
            }
        }),
        WM_DISPLAYCHANGE => with(&mut |d| d.mouse.refresh()),
        WM_WTSSESSION_CHANGE => with(&mut |d| d.session_change(wparam)),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mv(x: u32) -> Cmd {
        Cmd::Move { head: 0, x, y: 0 }
    }

    fn kept_moves(cmds: &[Cmd]) -> Vec<u32> {
        (0..cmds.len())
            .filter(|&i| !move_superseded(cmds, i))
            .filter_map(|i| match cmds[i] {
                Cmd::Move { x, .. } => Some(x),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn moves_coalesce_only_between_other_events() {
        let press = || Cmd::Button {
            button: 0,
            down: true,
        };
        let release = || Cmd::Button {
            button: 0,
            down: false,
        };
        // The press must happen at 2, the release at 4.
        let cmds = [mv(1), mv(2), press(), mv(3), mv(4), release(), mv(5)];
        assert_eq!(kept_moves(&cmds), [2, 4, 5]);
        assert_eq!(kept_moves(&[mv(1), mv(2), mv(3)]), [3]);
        assert_eq!(kept_moves(&[press(), mv(7)]), [7]);
    }
}
