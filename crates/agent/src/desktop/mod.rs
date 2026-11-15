// SPDX-License-Identifier: GPL-3.0-only

//! The desktop thread: everything that needs a window or the input desktop.
//!
//! One thread owns a hidden window on the current input desktop and listens
//! for session events (lock, unlock, logon).
//!
//! When the input desktop switches (logon screen, Ctrl+Alt+Del, UAC secure
//! desktop), the window is destroyed, the thread moves to the new desktop
//! and the window is recreated.
//!
//! The rest of the agent talks to this thread with [`Cmd`] and gets
//! [`Notify`] back.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows_sys::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop,
    UOI_NAME,
};
use windows_sys::Win32::System::Threading::{GetCurrentThreadId, OpenEventW, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW,
    TranslateMessage, WM_WTSSESSION_CHANGE, WNDCLASSW,
};

use crate::win::{Event, from_wide, wide};

const CLASS_NAME: &str = "AmeleoDisplayAgentDesktop";
const GENERIC_ALL: u32 = 0x1000_0000;
const SYNCHRONIZE: u32 = 0x0010_0000;
// WM_WTSSESSION_CHANGE reasons.
const WTS_SESSION_LOGON: usize = 5;
const WTS_SESSION_LOCK: usize = 7;
const WTS_SESSION_UNLOCK: usize = 8;

/// Requests to the desktop thread.
pub enum Cmd {
    /// New D-Bus session or remote client gone: forget what the previous
    /// client left behind.
    Reset,
}

/// Events from the desktop thread.
pub enum Notify {
    SessionLocked(bool),
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

struct Desktop {
    locked: bool,
    notify: mpsc::UnboundedSender<Notify>,
}

thread_local! {
    static DESKTOP: RefCell<Option<Desktop>> = const { RefCell::new(None) };
}

/// Starts the desktop thread.
pub fn spawn(notify: mpsc::UnboundedSender<Notify>) -> std::io::Result<DesktopHandle> {
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
            .spawn(move || run(rx, &wake, &switched, notify))?;
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
        unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) };

        DESKTOP.with_borrow_mut(|d| {
            let d = d.get_or_insert_with(|| Desktop {
                locked: on_logon_screen,
                notify: notify.clone(),
            });
            if first {
                let _ = d.notify.send(Notify::SessionLocked(d.locked));
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
            WTSUnRegisterSessionNotification(hwnd);
            DestroyWindow(hwnd);
        }
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
    DESKTOP.with_borrow_mut(|d| {
        let Some(_d) = d else { return };
        for cmd in cmds {
            match cmd {
                // Nothing kept per client yet.
                Cmd::Reset => {}
            }
        }
    });
}

impl Desktop {
    fn session_change(&mut self, reason: usize) {
        match reason {
            WTS_SESSION_LOCK => self.set_locked(true),
            WTS_SESSION_UNLOCK => self.set_locked(false),
            WTS_SESSION_LOGON => self.set_locked(false),
            _ => {}
        }
    }

    fn set_locked(&mut self, locked: bool) {
        if self.locked != locked {
            self.locked = locked;
            let _ = self.notify.send(Notify::SessionLocked(locked));
        }
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
        WM_WTSSESSION_CHANGE => with(&mut |d| d.session_change(wparam)),
        _ => return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
    0
}
