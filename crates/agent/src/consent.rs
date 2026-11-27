// SPDX-License-Identifier: GPL-3.0-only

//! The signed-in user's consent before someone else views or controls the
//! session (`fr.ameleo.Agent.Consent1`).
//!
//! The host asks; a window on the user's desktop shows who asks and what
//! for, and answers through `fr.ameleo.Host.Consent1.Answered`. While a
//! consent lasts, a small window at the top of the screen shows who views or
//! controls the session, with buttons to stop (`Stopped`).
//!
//! The windows belong to the session agent (SYSTEM): User Interface
//! Privilege Isolation keeps the user's own programs from clicking their
//! buttons. Their thread owns them and all the consent state, so that
//! requests, answers and stops are handled one at a time. The request
//! defaults to no: it takes no key, and its buttons stay disabled for a
//! second, so that no click meant for something else answers it.
//! Nothing is asked, and every consent stops, while the session is locked or
//! the logon screen shows, or when the D-Bus session or the client changes.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use proto::consent::{CONTROL, TIMEOUT_SECS, VIEW, WHO_MAX};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_DEFPUSHBUTTON, BS_PUSHBUTTON, CreateWindowExW, DefWindowProcW, DestroyWindow,
    DispatchMessageW, GetSystemMetrics, HWND_TOPMOST, KillTimer, MSG, MWMO_INPUTAVAILABLE,
    MsgWaitForMultipleObjectsEx, PM_REMOVE, PeekMessageW, QS_ALLINPUT, RegisterClassW, SM_CXSCREEN,
    SM_CYSCREEN, SW_SHOWNORMAL, SWP_NOMOVE, SWP_NOSIZE, SendMessageW, SetForegroundWindow,
    SetTimer, SetWindowPos, ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND, WM_SETFONT,
    WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};
use zbus::Connection;
use zbus::connection::Builder;

use crate::desktop::input_desktop_name;
use crate::feature::Feature;
use crate::win::{Event, wide};

const PROMPT_CLASS: &str = "AmeleoDisplayAgentConsent";
const INDICATOR_CLASS: &str = "AmeleoDisplayAgentConsentIndicator";
/// The buttons stay disabled this long after the request appears.
const ARM_MS: u32 = 1000;
const TIMER_ARM: usize = 1;
const TIMER_EXPIRE: usize = 2;
// Control ids.
const ID_REFUSE: usize = 101;
const ID_ACCEPT: usize = 102;
const ID_STOP_ALL: usize = 201;
const ID_STOP_CONTROL: usize = 202;
// Static control style: centered text.
const SS_CENTER: u32 = 1;
const LANG_FRENCH: u16 = 0x0c;

/// Requests to the consent thread.
enum Cmd {
    Ask {
        id: u32,
        kind: u32,
        who: String,
    },
    End {
        id: u32,
    },
    /// Every consent stops: tell the host (`Stopped`), or not (a new D-Bus
    /// session, where the host forgot them already).
    StopAll {
        tell: bool,
    },
}

/// What the consent thread tells the host.
enum Out {
    Answered { id: u32, granted: bool },
    Stopped { id: u32 },
}

pub struct Consent {
    tx: std_mpsc::Sender<Cmd>,
    wake: Arc<Event>,
    locked: Arc<AtomicBool>,
    /// The D-Bus session with the host, which the answers go to.
    conn: Arc<Mutex<Option<Connection>>>,
}

impl Consent {
    /// Starts the consent thread. Must run within the tokio runtime.
    pub fn new(locked: Arc<AtomicBool>) -> std::io::Result<Self> {
        let wake = Arc::new(Event::new(false)?);
        let (tx, rx) = std_mpsc::channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let conn: Arc<Mutex<Option<Connection>>> = Arc::default();
        {
            let wake = wake.clone();
            let locked = locked.clone();
            std::thread::Builder::new()
                .name("consent".into())
                .spawn(move || run(rx, &wake, &locked, out_tx))?;
        }
        {
            let conn = conn.clone();
            tokio::spawn(async move {
                while let Some(out) = out_rx.recv().await {
                    let Some(c) = conn.lock().unwrap().clone() else {
                        tracing::debug!("consent answer dropped: no D-Bus session");
                        continue;
                    };
                    let host = match proto::HostConsentProxy::builder(&c)
                        .cache_properties(zbus::proxy::CacheProperties::No)
                        .build()
                        .await
                    {
                        Ok(host) => host,
                        Err(e) => {
                            tracing::warn!("consent not told to the host: {e}");
                            continue;
                        }
                    };
                    let sent = match out {
                        Out::Answered { id, granted } => host.answered(id, granted).await,
                        Out::Stopped { id } => host.stopped(id).await,
                    };
                    if let Err(e) = sent {
                        tracing::warn!("consent not told to the host: {e}");
                    }
                }
            });
        }
        Ok(Self {
            tx,
            wake,
            locked,
            conn,
        })
    }

    fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            self.wake.set();
        }
    }
}

/// The interface the host calls.
struct ConsentIface {
    tx: std_mpsc::Sender<Cmd>,
    wake: Arc<Event>,
}

#[zbus::interface(name = "fr.ameleo.Agent.Consent1")]
impl ConsentIface {
    fn ask(&self, id: u32, kind: u32, who: String) -> zbus::fdo::Result<()> {
        if kind != VIEW && kind != CONTROL {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown kind {kind}"
            )));
        }
        let who: String = who
            .chars()
            .filter(|c| !c.is_control())
            .take(WHO_MAX)
            .collect();
        if self.tx.send(Cmd::Ask { id, kind, who }).is_ok() {
            self.wake.set();
        }
        Ok(())
    }

    fn end(&self, id: u32) {
        if self.tx.send(Cmd::End { id }).is_ok() {
            self.wake.set();
        }
    }
}

impl Feature for Consent {
    fn channels(&self) -> Vec<&'static str> {
        vec![proto::iface::CONSENT]
    }

    fn serve<'a>(&self, builder: Builder<'a>) -> zbus::Result<Builder<'a>> {
        builder.serve_at(
            proto::AGENT_PATH,
            ConsentIface {
                tx: self.tx.clone(),
                wake: self.wake.clone(),
            },
        )
    }

    /// A new D-Bus session, or the client gone (`Agent1.Reset`): every
    /// consent stops, and the host hears it, whichever it is.
    fn reset(&self) {
        self.send(Cmd::StopAll { tell: true });
    }

    fn stop(&self) {
        self.send(Cmd::StopAll { tell: true });
    }

    fn connected(&self, conn: Option<&Connection>) {
        *self.conn.lock().unwrap() = conn.cloned();
        if conn.is_none() {
            self.send(Cmd::StopAll { tell: false });
        }
    }

    fn session_locked(&self, locked: bool) {
        self.locked.store(locked, Ordering::Relaxed);
        if locked {
            self.send(Cmd::StopAll { tell: true });
        }
    }
}

/// A request shown to the user.
struct Prompt {
    hwnd: HWND,
    id: u32,
    kind: u32,
    who: String,
    refuse: HWND,
    accept: HWND,
}

/// The consent thread's state.
struct Ui {
    out: mpsc::UnboundedSender<Out>,
    prompt: Option<Prompt>,
    /// The consents granted: kind and who, by id.
    granted: BTreeMap<u32, (u32, String)>,
    indicator: Option<HWND>,
    french: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn run(
    rx: std_mpsc::Receiver<Cmd>,
    wake: &Event,
    locked: &AtomicBool,
    out: mpsc::UnboundedSender<Out>,
) {
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    for class in [PROMPT_CLASS, INDICATOR_CLASS] {
        let name = wide(class);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: instance,
            lpszClassName: name.as_ptr(),
            hbrBackground: 16 as _, // COLOR_BTNFACE + 1
            ..unsafe { std::mem::zeroed() }
        };
        if unsafe { RegisterClassW(&wc) } == 0 {
            tracing::error!(
                "consent: RegisterClass failed: {}",
                crate::win::last_error()
            );
            return;
        }
    }
    let french = (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == LANG_FRENCH;
    UI.with_borrow_mut(|ui| {
        *ui = Some(Ui {
            out,
            prompt: None,
            granted: BTreeMap::new(),
            indicator: None,
            french,
        })
    });
    loop {
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
        while let Ok(cmd) = rx.try_recv() {
            UI.with_borrow_mut(|ui| {
                if let Some(ui) = ui {
                    ui.command(cmd, locked.load(Ordering::Relaxed));
                }
            });
        }
    }
}

impl Ui {
    fn command(&mut self, cmd: Cmd, locked: bool) {
        match cmd {
            Cmd::Ask { id, kind, who } => {
                if locked || self.prompt.is_some() || !self.on_user_desktop() {
                    tracing::info!(id, kind, locked, "consent refused at once");
                    let _ = self.out.send(Out::Answered { id, granted: false });
                    return;
                }
                tracing::info!(id, kind, "consent asked of the user");
                self.show_prompt(id, kind, who);
            }
            Cmd::End { id } => {
                if self.prompt.as_ref().is_some_and(|p| p.id == id) {
                    self.close_prompt();
                }
                if self.granted.remove(&id).is_some() {
                    tracing::info!(id, "consent ended by the host");
                    self.refresh_indicator();
                }
            }
            Cmd::StopAll { tell } => {
                if let Some(p) = &self.prompt {
                    let id = p.id;
                    self.close_prompt();
                    if tell {
                        let _ = self.out.send(Out::Answered { id, granted: false });
                    }
                }
                for id in std::mem::take(&mut self.granted).into_keys() {
                    tracing::info!(id, "consent stopped");
                    if tell {
                        let _ = self.out.send(Out::Stopped { id });
                    }
                }
                self.refresh_indicator();
            }
        }
    }

    /// The windows go on the user's desktop: none while another one (the
    /// logon screen, the secure desktop) is shown. Moves the thread to it
    /// when it has no window.
    fn on_user_desktop(&self) -> bool {
        let free = self.prompt.is_none() && self.indicator.is_none();
        input_desktop_name(free).eq_ignore_ascii_case("Default")
    }

    fn text(&self, en: &'static str, fr: &'static str) -> &'static str {
        if self.french { fr } else { en }
    }

    fn show_prompt(&mut self, id: u32, kind: u32, who: String) {
        let title = self.text("Remote access", "Accès à distance");
        let what = if kind == CONTROL {
            self.text(
                "asks to control your session: their keyboard and mouse would act on your computer, besides yours.",
                "demande à prendre la main sur votre session : son clavier et sa souris agiraient sur votre ordinateur, en plus des vôtres.",
            )
        } else {
            self.text("asks to see your screen.", "demande à voir votre écran.")
        };
        let message = format!(
            "{who} {what}\n\n{}",
            self.text(
                "Without an answer, the request is refused in 30 seconds.",
                "Sans réponse, la demande est refusée dans 30 secondes.",
            )
        );
        let (w, h) = (440, 190);
        let hwnd = create(
            PROMPT_CLASS,
            title,
            WS_POPUP | WS_CAPTION | WS_SYSMENU,
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            centered(w, h),
            (w, h),
            std::ptr::null_mut(),
            0,
        );
        if hwnd.is_null() {
            tracing::warn!("consent window not created: {}", crate::win::last_error());
            let _ = self.out.send(Out::Answered { id, granted: false });
            return;
        }
        child(
            hwnd,
            "STATIC",
            &message,
            SS_CENTER,
            (16, 14),
            (w - 40, 90),
            0,
        );
        let refuse = child(
            hwnd,
            "BUTTON",
            self.text("Refuse", "Refuser"),
            BS_DEFPUSHBUTTON as u32 | WS_TABSTOP,
            (w / 2 - 150, h - 80),
            (130, 30),
            ID_REFUSE,
        );
        let accept = child(
            hwnd,
            "BUTTON",
            self.text("Accept", "Accepter"),
            BS_PUSHBUTTON as u32 | WS_TABSTOP,
            (w / 2 + 10, h - 80),
            (130, 30),
            ID_ACCEPT,
        );
        unsafe {
            EnableWindow(refuse, 0);
            EnableWindow(accept, 0);
            SetTimer(hwnd, TIMER_ARM, ARM_MS, None);
            SetTimer(hwnd, TIMER_EXPIRE, TIMEOUT_SECS * 1000, None);
            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
            SetForegroundWindow(hwnd);
        }
        self.prompt = Some(Prompt {
            hwnd,
            id,
            kind,
            who,
            refuse,
            accept,
        });
    }

    fn close_prompt(&mut self) {
        if let Some(p) = self.prompt.take() {
            unsafe {
                KillTimer(p.hwnd, TIMER_ARM);
                KillTimer(p.hwnd, TIMER_EXPIRE);
                DestroyWindow(p.hwnd);
            }
        }
    }

    /// The user answered the request shown, or let it expire.
    fn answer(&mut self, granted: bool) {
        let Some(p) = self.prompt.as_ref() else {
            return;
        };
        let (id, kind, who) = (p.id, p.kind, p.who.clone());
        self.close_prompt();
        tracing::info!(id, kind, granted, "consent answered");
        let _ = self.out.send(Out::Answered { id, granted });
        if granted {
            self.granted.insert(id, (kind, who));
            self.refresh_indicator();
        }
    }

    /// The user stopped every consent, or control alone.
    fn stop(&mut self, control_only: bool) {
        let ids: Vec<u32> = self
            .granted
            .iter()
            .filter(|(_, (kind, _))| !control_only || *kind == CONTROL)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.granted.remove(&id);
            tracing::info!(id, "consent stopped by the user");
            let _ = self.out.send(Out::Stopped { id });
        }
        self.refresh_indicator();
    }

    /// The window at the top of the screen, rebuilt to show the consents
    /// granted; none without any.
    fn refresh_indicator(&mut self) {
        if let Some(hwnd) = self.indicator.take() {
            unsafe { DestroyWindow(hwnd) };
        }
        if self.granted.is_empty() {
            return;
        }
        let controlled = self.granted.values().any(|(kind, _)| *kind == CONTROL);
        let who = self
            .granted
            .values()
            .map(|(_, who)| who.as_str())
            .next()
            .unwrap_or_default()
            .to_owned();
        let message = if controlled {
            format!(
                "{who} {}",
                self.text("controls your session", "a la main sur votre session")
            )
        } else {
            format!(
                "{who} {}",
                self.text("sees your screen", "voit votre écran")
            )
        };
        let (w, h) = (if controlled { 560 } else { 440 }, 44);
        let x = (unsafe { GetSystemMetrics(SM_CXSCREEN) } - w) / 2;
        let hwnd = create(
            INDICATOR_CLASS,
            self.text("Remote access", "Accès à distance"),
            WS_POPUP,
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            (x, 0),
            (w, h),
            std::ptr::null_mut(),
            0,
        );
        if hwnd.is_null() {
            // Nothing tells the user: the consents stop.
            tracing::warn!(
                "consent indicator not created: {}",
                crate::win::last_error()
            );
            self.stop(false);
            return;
        }
        let buttons = if controlled { 2 } else { 1 };
        let text_w = w - 16 - buttons * 128;
        child(hwnd, "STATIC", &message, 0, (12, 13), (text_w, 20), 0);
        child(
            hwnd,
            "BUTTON",
            self.text("Stop", "Arrêter"),
            BS_PUSHBUTTON as u32,
            (w - 8 - buttons * 128 + if controlled { 128 } else { 0 }, 7),
            (120, 30),
            ID_STOP_ALL,
        );
        if controlled {
            child(
                hwnd,
                "BUTTON",
                self.text("Take control back", "Reprendre la main"),
                BS_PUSHBUTTON as u32,
                (w - 8 - 2 * 128, 7),
                (120, 30),
                ID_STOP_CONTROL,
            );
        }
        unsafe {
            ShowWindow(hwnd, SW_SHOWNORMAL);
            SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
        }
        self.indicator = Some(hwnd);
    }
}

fn centered(w: i32, h: i32) -> (i32, i32) {
    let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
    ((sw - w) / 2, (sh - h) / 2)
}

#[allow(clippy::too_many_arguments)]
fn create(
    class: &str,
    title: &str,
    style: u32,
    ex_style: u32,
    (x, y): (i32, i32),
    (w, h): (i32, i32),
    parent: HWND,
    id: usize,
) -> HWND {
    let class = wide(class);
    let title = wide(title);
    unsafe {
        CreateWindowExW(
            ex_style,
            class.as_ptr(),
            title.as_ptr(),
            style,
            x,
            y,
            w,
            h,
            parent,
            id as _,
            GetModuleHandleW(std::ptr::null()),
            std::ptr::null(),
        )
    }
}

fn child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    at: (i32, i32),
    size: (i32, i32),
    id: usize,
) -> HWND {
    let hwnd = create(
        class,
        text,
        WS_CHILD | WS_VISIBLE | style,
        0,
        at,
        size,
        parent,
        id,
    );
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            GetStockObject(DEFAULT_GUI_FONT) as WPARAM,
            1,
        );
    }
    hwnd
}

/// Runs `f` on the thread's state, unless it is in use: a message sent while
/// the state handles another (creating or destroying a window) is not ours to
/// act on then.
fn with_ui(f: impl FnOnce(&mut Ui)) {
    UI.with(|cell| {
        if let Ok(mut ui) = cell.try_borrow_mut()
            && let Some(ui) = ui.as_mut()
        {
            f(ui);
        }
    });
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_TIMER => {
            with_ui(|ui| match wparam {
                TIMER_ARM => {
                    if let Some(p) = &ui.prompt {
                        unsafe {
                            KillTimer(hwnd, TIMER_ARM);
                            EnableWindow(p.refuse, 1);
                            EnableWindow(p.accept, 1);
                        }
                    }
                }
                TIMER_EXPIRE => ui.answer(false),
                _ => {}
            });
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            with_ui(|ui| match id {
                ID_ACCEPT => ui.answer(true),
                ID_REFUSE => ui.answer(false),
                ID_STOP_ALL => ui.stop(false),
                ID_STOP_CONTROL => ui.stop(true),
                _ => {}
            });
            0
        }
        // Closing a window refuses, or stops everything.
        WM_CLOSE => {
            with_ui(|ui| {
                if ui.prompt.as_ref().is_some_and(|p| p.hwnd == hwnd) {
                    ui.answer(false);
                } else if ui.indicator == Some(hwnd) {
                    ui.stop(false);
                }
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}
