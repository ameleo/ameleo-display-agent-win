// SPDX-License-Identifier: GPL-3.0-only

//! The signed-in user's consent before someone else views or controls the
//! session (`fr.ameleo.Agent.Consent1`).
//!
//! The host asks; a task dialog (the Windows dialog) on the user's desktop
//! shows who asks and what for, and the agent answers through
//! `fr.ameleo.Host.Consent1.Answered`. While a
//! consent lasts, a panel in the bottom right corner shows who views or
//! controls the session, with buttons to stop (`Stopped`); the user may
//! reduce it to a tab in the corner.
//!
//! The windows belong to the session agent (SYSTEM): User Interface
//! Privilege Isolation keeps the user's own programs from clicking their
//! buttons. Their thread owns them and all the consent state, so that
//! requests, answers and stops are handled one at a time; the dialog's
//! timer handles those that come while it is shown. The request defaults
//! to no: Enter, Esc and closing it refuse, and its buttons stay disabled
//! for a second, so that no click meant for something else answers it.
//! Nothing is asked, and every consent stops, while the session is locked or
//! the logon screen shows, or when the D-Bus session or the client changes.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex};

use proto::consent::{CONTROL, TIMEOUT_SECS, VIEW, WHO_MAX};
use tokio::sync::mpsc;
use windows_sys::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Globalization::GetUserDefaultUILanguage;
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontIndirectW, CreateSolidBrush, DT_CALCRECT, DT_NOPREFIX, DT_SINGLELINE,
    DT_WORDBREAK, DeleteObject, DrawTextW, EndPaint, EnumDisplayMonitors, FW_SEMIBOLD, FillRect,
    GetDC, GetMonitorInfoW, HBRUSH, HDC, HFONT, HMONITOR, LOGFONTW, MONITOR_DEFAULTTOPRIMARY,
    MONITORINFO, MonitorFromPoint, PAINTSTRUCT, ReleaseDC, SelectObject, SetBkMode, SetTextColor,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::{
    TASKDIALOG_BUTTON, TASKDIALOGCONFIG, TD_INFORMATION_ICON, TD_WARNING_ICON, TDE_FOOTER,
    TDF_ALLOW_DIALOG_CANCELLATION, TDF_CALLBACK_TIMER, TDM_ENABLE_BUTTON, TDM_UPDATE_ELEMENT_TEXT,
    TDN_BUTTON_CLICKED, TDN_CREATED, TDN_TIMER, TaskDialogIndirect,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForMonitor, MDT_EFFECTIVE_DPI, SystemParametersInfoForDpi,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    BS_PUSHBUTTON, CS_DROPSHADOW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GetClientRect, GetSystemMetrics, HWND_TOPMOST, IDC_ARROW, LWA_ALPHA, LoadCursorW, MSG,
    MWMO_INPUTAVAILABLE, MsgWaitForMultipleObjectsEx, NONCLIENTMETRICSW, PM_REMOVE, PeekMessageW,
    PostMessageW, QS_ALLINPUT, RegisterClassW, SM_CXSCREEN, SM_CYSCREEN, SPI_GETNONCLIENTMETRICS,
    SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SendMessageW, SetForegroundWindow,
    SetLayeredWindowAttributes, SetWindowPos, ShowWindow, TranslateMessage, WM_CLOSE, WM_COMMAND,
    WM_CTLCOLORBTN, WM_CTLCOLORSTATIC, WM_DISPLAYCHANGE, WM_PAINT, WM_SETFONT, WNDCLASSW, WS_CHILD,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    WS_VISIBLE,
};
use windows_sys::core::BOOL;
use zbus::Connection;
use zbus::connection::Builder;

use crate::desktop::input_desktop_name;
use crate::feature::Feature;
use crate::win::{Event, wide};

const INDICATOR_CLASS: &str = "AmeleoDisplayAgentConsentIndicator";
const EDGE_CLASS: &str = "AmeleoDisplayAgentConsentEdge";
/// The buttons of the request stay disabled this long after it appears.
const ARM_MS: u64 = 1000;
// Button and control ids.
const ID_REFUSE: i32 = 101;
const ID_ACCEPT: i32 = 102;
const ID_STOP_ALL: usize = 201;
const ID_STOP_CONTROL: usize = 202;
const ID_COLLAPSE: usize = 203;
const ID_EXPAND: usize = 204;
// Static control style: `&` is shown as it is (a name may have one).
const SS_NOPREFIX: u32 = 0x80;
// SetBkMode: text drawn over the background as it is.
const BK_TRANSPARENT: i32 = 1;
// TDN_BUTTON_CLICKED: keep the dialog open.
const S_FALSE: i32 = 1;
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
        Ok(builder.serve_at(
            proto::AGENT_PATH,
            ConsentIface {
                tx: self.tx.clone(),
                wake: self.wake.clone(),
            },
        ))
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

/// The look of the indicator, for the monitor it goes on: its DPI, its work
/// area and the Windows message font at that DPI. The fonts go with it.
struct Look {
    dpi: u32,
    work: RECT,
    text: HFONT,
    strong: HFONT,
    /// Segoe MDL2 Assets, the icons of Windows' own buttons.
    icons: HFONT,
}

/// Glyphs of Segoe MDL2 Assets.
const GLYPH_COLLAPSE: &str = "\u{E921}";
const GLYPH_EXPAND: &str = "\u{E70E}";

impl Look {
    /// For the primary monitor, where the indicator goes.
    fn new() -> Self {
        unsafe {
            let monitor = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let (mut dpi, mut dpi_y) = (96, 96);
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut dpi_y) < 0 || dpi == 0 {
                dpi = 96;
            }
            let mut info: MONITORINFO = std::mem::zeroed();
            info.cbSize = size_of::<MONITORINFO>() as u32;
            let work = if GetMonitorInfoW(monitor, &mut info) != 0 {
                info.rcWork
            } else {
                RECT {
                    left: 0,
                    top: 0,
                    right: GetSystemMetrics(SM_CXSCREEN),
                    bottom: GetSystemMetrics(SM_CYSCREEN),
                }
            };
            let mut metrics: NONCLIENTMETRICSW = std::mem::zeroed();
            metrics.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
            let mut font = if SystemParametersInfoForDpi(
                SPI_GETNONCLIENTMETRICS,
                metrics.cbSize,
                (&raw mut metrics).cast(),
                0,
                dpi,
            ) != 0
            {
                metrics.lfMessageFont
            } else {
                let mut f: LOGFONTW = std::mem::zeroed();
                let face = wide("Segoe UI");
                f.lfFaceName[..face.len()].copy_from_slice(&face);
                f.lfHeight = -(12 * dpi as i32 / 96);
                f
            };
            let text = CreateFontIndirectW(&font);
            let mut icon_font = font;
            font.lfWeight = FW_SEMIBOLD as i32;
            let strong = CreateFontIndirectW(&font);
            let face = wide("Segoe MDL2 Assets");
            icon_font.lfFaceName = [0; 32];
            icon_font.lfFaceName[..face.len()].copy_from_slice(&face);
            let icons = CreateFontIndirectW(&icon_font);
            Self {
                dpi,
                work,
                text,
                strong,
                icons,
            }
        }
    }

    /// `dip` device independent pixels (1/96 inch), in pixels.
    fn px(&self, dip: i32) -> i32 {
        dip * self.dpi as i32 / 96
    }

    /// The size of `text` in `font`: on one line, or wrapped at `width`.
    fn measure(&self, font: HFONT, text: &str, width: Option<i32>) -> (i32, i32) {
        let mut buf = wide(text);
        let mut r = RECT {
            left: 0,
            top: 0,
            right: width.unwrap_or(0),
            bottom: 0,
        };
        let flags = DT_CALCRECT
            | DT_NOPREFIX
            | if width.is_some() {
                DT_WORDBREAK
            } else {
                DT_SINGLELINE
            };
        unsafe {
            let dc = GetDC(std::ptr::null_mut());
            let old = SelectObject(dc, font);
            DrawTextW(dc, buf.as_mut_ptr(), -1, &mut r, flags);
            SelectObject(dc, old);
            ReleaseDC(std::ptr::null_mut(), dc);
        }
        (r.right - r.left, r.bottom - r.top)
    }

    /// The width of a button labelled `label`.
    fn button_width(&self, label: &str) -> i32 {
        (self.measure(self.text, label, None).0 + self.px(32)).max(self.px(96))
    }
}

impl Drop for Look {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.text);
            DeleteObject(self.strong);
            DeleteObject(self.icons);
        }
    }
}

/// The indicator's colors, for its window procedure.
#[derive(Clone, Copy)]
struct Paint {
    back: HBRUSH,
    fore: COLORREF,
}

thread_local! {
    static PAINT: RefCell<Vec<(HWND, Paint)>> = const { RefCell::new(Vec::new()) };
}

fn paint_of(hwnd: HWND) -> Option<Paint> {
    PAINT.with_borrow(|p| p.iter().find(|(h, _)| *h == hwnd).map(|(_, p)| *p))
}

fn set_paint(hwnd: HWND, paint: Option<Paint>) {
    PAINT.with_borrow_mut(|p| {
        p.retain(|(h, _)| *h != hwnd);
        if let Some(paint) = paint {
            p.push((hwnd, paint));
        }
    });
}

/// A request to the user, shown in a task dialog.
struct Prompt {
    id: u32,
    kind: u32,
    who: String,
    /// The dialog, once Windows has made it; null before.
    hwnd: HWND,
    /// Taken back by the host or stopped: the dialog closes without an
    /// answer of the user.
    withdrawn: bool,
}

/// A control of the indicator: class, text, style, position, size, id and
/// font.
type Child = (
    &'static str,
    String,
    u32,
    (i32, i32),
    (i32, i32),
    usize,
    HFONT,
);

/// The panel in the bottom right corner. Dropping it closes the window.
struct Indicator {
    hwnd: HWND,
    /// The colored bands around the screens.
    edges: Vec<HWND>,
    brush: HBRUSH,
    _look: Look,
}

impl Drop for Indicator {
    fn drop(&mut self) {
        for &hwnd in self.edges.iter().chain([&self.hwnd]) {
            unsafe { DestroyWindow(hwnd) };
            set_paint(hwnd, None);
        }
        unsafe { DeleteObject(self.brush) };
    }
}

/// Bands of `brush`, `thickness` pixels wide, along the edges of every
/// screen, as Teams shows while it shares the screen. Clicks go through
/// them, and they never take the focus.
fn edges(thickness: i32, brush: HBRUSH) -> Vec<HWND> {
    unsafe extern "system" fn add(_: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
        // EnumDisplayMonitors passes our Vec back, and the monitor's rect.
        unsafe { (*(data as *mut Vec<RECT>)).push(*r) };
        1
    }
    let mut monitors: Vec<RECT> = Vec::new();
    unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(add),
            (&raw mut monitors) as LPARAM,
        )
    };
    let t = thickness;
    let mut bands = Vec::new();
    for m in monitors {
        let (w, h) = (m.right - m.left, m.bottom - m.top);
        for (at, size) in [
            ((m.left, m.top), (w, t)),
            ((m.left, m.bottom - t), (w, t)),
            ((m.left, m.top + t), (t, h - 2 * t)),
            ((m.right - t, m.top + t), (t, h - 2 * t)),
        ] {
            let hwnd = create(
                EDGE_CLASS,
                "",
                WS_POPUP,
                WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT,
                at,
                size,
                std::ptr::null_mut(),
                0,
            );
            if hwnd.is_null() {
                continue;
            }
            set_paint(
                hwnd,
                Some(Paint {
                    back: brush,
                    fore: 0,
                }),
            );
            unsafe {
                // Opaque, but layered: WS_EX_TRANSPARENT lets clicks through.
                SetLayeredWindowAttributes(hwnd, 0, 255, LWA_ALPHA);
                ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            bands.push(hwnd);
        }
    }
    bands
}

/// The consent thread's state.
struct Ui {
    out: mpsc::UnboundedSender<Out>,
    prompt: Option<Prompt>,
    /// The consents granted: kind and who, by id.
    granted: BTreeMap<u32, (u32, String)>,
    indicator: Option<Indicator>,
    /// The user reduced the indicator to its corner.
    collapsed: bool,
    french: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

/// What the task dialog's callback needs: the requests keep coming while
/// the dialog runs its own message loop.
struct Dialog<'a> {
    rx: &'a std_mpsc::Receiver<Cmd>,
    locked: &'a AtomicBool,
    french: bool,
    /// The buttons are enabled.
    armed: Cell<bool>,
    /// The seconds left shown in the footer.
    shown: Cell<u64>,
}

fn run(
    rx: std_mpsc::Receiver<Cmd>,
    wake: &Event,
    locked: &AtomicBool,
    out: mpsc::UnboundedSender<Out>,
) {
    if !register_classes() {
        return;
    }
    let french = (unsafe { GetUserDefaultUILanguage() } & 0x3ff) == LANG_FRENCH;
    UI.with_borrow_mut(|ui| {
        *ui = Some(Ui {
            out,
            prompt: None,
            granted: BTreeMap::new(),
            indicator: None,
            collapsed: false,
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
        drain(&rx, locked);
        // A request to show: the dialog returns once answered, refused,
        // expired or withdrawn.
        let ask = UI.with_borrow(|ui| {
            ui.as_ref()?
                .prompt
                .as_ref()
                .filter(|p| p.hwnd.is_null() && !p.withdrawn)
                .map(|p| (p.kind, p.who.clone()))
        });
        if let Some((kind, who)) = ask {
            let dialog = Dialog {
                rx: &rx,
                locked,
                french,
                armed: Cell::new(false),
                shown: Cell::new(u64::from(TIMEOUT_SECS)),
            };
            show_prompt(kind, &who, &dialog);
        }
    }
}

/// Handles the requests waiting.
fn drain(rx: &std_mpsc::Receiver<Cmd>, locked: &AtomicBool) {
    while let Ok(cmd) = rx.try_recv() {
        UI.with_borrow_mut(|ui| {
            if let Some(ui) = ui {
                ui.command(cmd, locked.load(Ordering::Relaxed));
            }
        });
    }
}

/// The window class of the indicator.
fn register_classes() -> bool {
    // The indicator has a shadow, so that it stands out from the windows
    // under it; the bands around the screens have none.
    for (class, style) in [(INDICATOR_CLASS, CS_DROPSHADOW), (EDGE_CLASS, 0)] {
        let name = wide(class);
        let wc = WNDCLASSW {
            style,
            lpfnWndProc: Some(wnd_proc),
            hInstance: unsafe { GetModuleHandleW(std::ptr::null()) },
            lpszClassName: name.as_ptr(),
            hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
            ..unsafe { std::mem::zeroed() }
        };
        if unsafe { RegisterClassW(&wc) } == 0 {
            tracing::error!(
                "consent: RegisterClass failed: {}",
                crate::win::last_error()
            );
            return false;
        }
    }
    true
}

fn text(french: bool, en: &'static str, fr: &'static str) -> &'static str {
    if french { fr } else { en }
}

fn countdown_text(french: bool, secs: u64) -> String {
    if french {
        format!("Sans réponse, la demande sera refusée dans {secs} s.")
    } else {
        format!("Without an answer, the request is refused in {secs} s.")
    }
}

/// Shows the request in a task dialog, the dialog of Windows itself, and
/// answers the host when the user does (or lets it expire).
fn show_prompt(kind: u32, who: &str, dialog: &Dialog) {
    let french = dialog.french;
    let (heading, what) = if kind == CONTROL {
        (
            text(
                french,
                "Request to control your session",
                "Demande de prise de main",
            ),
            text(
                french,
                "asks to control your session: their keyboard and mouse would act on your computer, besides yours.",
                "demande à prendre la main sur votre session : son clavier et sa souris agiraient sur votre ordinateur, en plus des vôtres.",
            ),
        )
    } else {
        (
            text(
                french,
                "Request to see your screen",
                "Demande de partage d'écran",
            ),
            text(
                french,
                "asks to see your screen.",
                "demande à voir votre écran.",
            ),
        )
    };
    let title = wide(text(french, "Remote access", "Accès à distance"));
    let heading = wide(heading);
    let content = wide(format!("{who} {what}"));
    let footer = wide(countdown_text(french, u64::from(TIMEOUT_SECS)));
    let accept = wide(text(french, "Accept", "Accepter"));
    let refuse = wide(text(french, "Refuse", "Refuser"));
    let buttons = [
        TASKDIALOG_BUTTON {
            nButtonID: ID_ACCEPT,
            pszButtonText: accept.as_ptr(),
        },
        TASKDIALOG_BUTTON {
            nButtonID: ID_REFUSE,
            pszButtonText: refuse.as_ptr(),
        },
    ];
    let mut config: TASKDIALOGCONFIG = unsafe { std::mem::zeroed() };
    config.cbSize = size_of::<TASKDIALOGCONFIG>() as u32;
    // Closing the dialog (its close button, Esc, the expiry) refuses.
    config.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION | TDF_CALLBACK_TIMER;
    config.pszWindowTitle = title.as_ptr();
    config.Anonymous1.pszMainIcon = if kind == CONTROL {
        TD_WARNING_ICON
    } else {
        TD_INFORMATION_ICON
    };
    config.pszMainInstruction = heading.as_ptr();
    config.pszContent = content.as_ptr();
    config.cButtons = buttons.len() as u32;
    config.pButtons = buttons.as_ptr();
    // Enter refuses.
    config.nDefaultButton = ID_REFUSE;
    config.pszFooter = footer.as_ptr();
    config.pfCallback = Some(dialog_callback);
    config.lpCallbackData = std::ptr::from_ref(dialog) as isize;
    let mut button = 0;
    let hr = unsafe {
        TaskDialogIndirect(
            &config,
            &mut button,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if hr < 0 {
        tracing::warn!("consent dialog not shown: {hr:#010x}");
    }
    UI.with_borrow_mut(|ui| {
        let Some(ui) = ui else { return };
        let Some(p) = ui.prompt.take() else { return };
        if !p.withdrawn {
            ui.answer(p, hr >= 0 && button == ID_ACCEPT);
        }
    });
}

/// The task dialog's notifications: it appears, a click, and its timer,
/// which arms the buttons, counts down, expires, and handles the requests
/// that came meanwhile.
unsafe extern "system" fn dialog_callback(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    _lparam: LPARAM,
    data: isize,
) -> i32 {
    // The dialog lives within show_prompt, which owns `data`.
    let dialog = unsafe { &*(data as *const Dialog) };
    match msg as i32 {
        TDN_CREATED => {
            with_ui(|ui| {
                if let Some(p) = &mut ui.prompt {
                    p.hwnd = hwnd;
                }
            });
            unsafe {
                // No answer until the user has seen it.
                SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, ID_ACCEPT as WPARAM, 0);
                SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, ID_REFUSE as WPARAM, 0);
                SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
                SetForegroundWindow(hwnd);
            }
            0
        }
        // Accept only once armed; refusing, any time.
        TDN_BUTTON_CLICKED => {
            if wparam as i32 == ID_ACCEPT && !dialog.armed.get() {
                S_FALSE
            } else {
                0
            }
        }
        TDN_TIMER => {
            // Milliseconds since the dialog appeared.
            let elapsed = wparam as u64;
            if !dialog.armed.get() && elapsed >= ARM_MS {
                dialog.armed.set(true);
                unsafe {
                    SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, ID_ACCEPT as WPARAM, 1);
                    SendMessageW(hwnd, TDM_ENABLE_BUTTON as u32, ID_REFUSE as WPARAM, 1);
                }
            }
            let total = u64::from(TIMEOUT_SECS);
            let left = total.saturating_sub(elapsed / 1000);
            if left != dialog.shown.get() {
                dialog.shown.set(left);
                let footer = wide(countdown_text(dialog.french, left));
                unsafe {
                    SendMessageW(
                        hwnd,
                        TDM_UPDATE_ELEMENT_TEXT as u32,
                        TDE_FOOTER as WPARAM,
                        footer.as_ptr() as LPARAM,
                    )
                };
            }
            if elapsed >= total * 1000 {
                unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
            }
            drain(dialog.rx, dialog.locked);
            0
        }
        _ => 0,
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
                // The thread's loop shows it.
                self.prompt = Some(Prompt {
                    id,
                    kind,
                    who,
                    hwnd: std::ptr::null_mut(),
                    withdrawn: false,
                });
            }
            Cmd::End { id } => {
                if self.prompt.as_ref().is_some_and(|p| p.id == id) {
                    self.withdraw_prompt();
                }
                if self.granted.remove(&id).is_some() {
                    tracing::info!(id, "consent ended by the host");
                    self.refresh_indicator();
                }
            }
            Cmd::StopAll { tell } => {
                if let Some(p) = &self.prompt
                    && !p.withdrawn
                {
                    if tell {
                        let _ = self.out.send(Out::Answered {
                            id: p.id,
                            granted: false,
                        });
                    }
                    self.withdraw_prompt();
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

    /// The request is no longer to be answered: its dialog closes.
    fn withdraw_prompt(&mut self) {
        let Some(p) = &mut self.prompt else { return };
        if p.hwnd.is_null() {
            self.prompt = None;
        } else {
            p.withdrawn = true;
            unsafe { PostMessageW(p.hwnd, WM_CLOSE, 0, 0) };
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
        text(self.french, en, fr)
    }

    /// The user answered the request, or let it expire.
    fn answer(&mut self, p: Prompt, granted: bool) {
        tracing::info!(id = p.id, kind = p.kind, granted, "consent answered");
        let _ = self.out.send(Out::Answered { id: p.id, granted });
        if granted {
            self.granted.insert(p.id, (p.kind, p.who));
            // A new consent shows in full, even when the user had reduced
            // the indicator.
            self.collapsed = false;
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

    /// The panel in the bottom right corner, rebuilt to show the consents
    /// granted; none without any. Orange under control, blue when the
    /// screen is only seen, so that it stands out from the user's windows.
    /// The user may reduce it to a small tab in the corner, never hide it.
    fn refresh_indicator(&mut self) {
        self.indicator = None;
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
        let look = Look::new();
        let px = |dip| look.px(dip);
        let (pad, gap, button_h) = (px(14), px(8), px(28));
        let icon_w = px(32);
        let mut children: Vec<Child> = Vec::new();
        let (w, h);
        if self.collapsed {
            let label = if controlled {
                self.text("Remote control", "Prise de main en cours")
            } else {
                self.text("Screen shared", "Écran partagé")
            };
            let (text_w, text_h) = look.measure(look.strong, label, None);
            h = button_h + 2 * px(6);
            w = pad + text_w + px(16) + icon_w + px(6);
            children.push((
                "STATIC",
                label.to_owned(),
                SS_NOPREFIX,
                (pad, (h - text_h) / 2),
                (text_w, text_h),
                0,
                look.strong,
            ));
            children.push((
                "BUTTON",
                GLYPH_EXPAND.to_owned(),
                BS_PUSHBUTTON as u32,
                (w - px(6) - icon_w, px(6)),
                (icon_w, button_h),
                ID_EXPAND,
                look.icons,
            ));
        } else {
            let title = self.text("Remote access", "Accès à distance");
            let message = if controlled {
                format!(
                    "{who} {}",
                    self.text("controls your session.", "a la main sur votre session.")
                )
            } else {
                format!(
                    "{who} {}",
                    self.text("sees your screen.", "voit votre écran.")
                )
            };
            let stop_label = self.text("Stop", "Arrêter");
            let back_label = self.text("Take control back", "Reprendre la main");
            let stop_w = look.button_width(stop_label);
            let back_w = if controlled {
                look.button_width(back_label)
            } else {
                0
            };
            let buttons_w = stop_w + if controlled { gap + back_w } else { 0 };
            w = (2 * pad + buttons_w).max(px(320));
            let inner = w - 2 * pad;
            let title_h = look.measure(look.strong, title, None).1;
            let message_h = look.measure(look.text, &message, Some(inner)).1;
            // Title row, with the reduce button on the right.
            let header_h = button_h;
            children.push((
                "STATIC",
                title.to_owned(),
                SS_NOPREFIX,
                (pad, pad + (header_h - title_h) / 2),
                (inner - icon_w - gap, title_h),
                0,
                look.strong,
            ));
            children.push((
                "BUTTON",
                GLYPH_COLLAPSE.to_owned(),
                BS_PUSHBUTTON as u32,
                (w - pad - icon_w, pad),
                (icon_w, button_h),
                ID_COLLAPSE,
                look.icons,
            ));
            let message_y = pad + header_h + px(8);
            children.push((
                "STATIC",
                message,
                SS_NOPREFIX,
                (pad, message_y),
                (inner, message_h),
                0,
                look.text,
            ));
            let button_y = message_y + message_h + px(14);
            h = button_y + button_h + pad;
            let mut x = w - pad - buttons_w;
            if controlled {
                children.push((
                    "BUTTON",
                    back_label.to_owned(),
                    BS_PUSHBUTTON as u32,
                    (x, button_y),
                    (back_w, button_h),
                    ID_STOP_CONTROL,
                    look.text,
                ));
                x += back_w + gap;
            }
            children.push((
                "BUTTON",
                stop_label.to_owned(),
                BS_PUSHBUTTON as u32,
                (x, button_y),
                (stop_w, button_h),
                ID_STOP_ALL,
                look.text,
            ));
        }
        let work = look.work;
        let margin = px(12);
        let at = (work.right - w - margin, work.bottom - h - margin);
        let hwnd = create(
            INDICATOR_CLASS,
            self.text("Remote access", "Accès à distance"),
            WS_POPUP,
            // It never takes the focus from the user's apps.
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            at,
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
        let color = if controlled {
            rgb(0xC4, 0x4F, 0x00)
        } else {
            rgb(0x00, 0x5F, 0xB8)
        };
        let brush = unsafe { CreateSolidBrush(color) };
        set_paint(
            hwnd,
            Some(Paint {
                back: brush,
                fore: rgb(0xFF, 0xFF, 0xFF),
            }),
        );
        // Rounded corners on Windows 11; Windows 10 refuses, and keeps them
        // square.
        let round = DWMWCP_ROUND;
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE as u32,
                (&raw const round).cast(),
                size_of_val(&round) as u32,
            )
        };
        for (class, text, style, at, size, id, font) in children {
            child(hwnd, class, &text, style, at, size, id, font);
        }
        unsafe {
            ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowPos(
                hwnd,
                HWND_TOPMOST,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        self.indicator = Some(Indicator {
            hwnd,
            edges: edges(px(3), brush),
            brush,
            _look: look,
        });
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    r as u32 | (g as u32) << 8 | (b as u32) << 16
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

#[allow(clippy::too_many_arguments)]
fn child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    at: (i32, i32),
    size: (i32, i32),
    id: usize,
    font: HFONT,
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
    unsafe { SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1) };
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

/// The indicator's window procedure.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => match paint_of(hwnd) {
            Some(p) => {
                unsafe {
                    let mut ps: PAINTSTRUCT = std::mem::zeroed();
                    let dc = BeginPaint(hwnd, &mut ps);
                    let mut client: RECT = std::mem::zeroed();
                    GetClientRect(hwnd, &mut client);
                    FillRect(dc, &client, p.back);
                    EndPaint(hwnd, &ps);
                }
                0
            }
            None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        },
        // The label on the indicator's color; the buttons' corners on it too.
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => match paint_of(hwnd) {
            Some(p) => {
                let dc = wparam as HDC;
                unsafe {
                    SetBkMode(dc, BK_TRANSPARENT);
                    SetTextColor(dc, p.fore);
                }
                p.back as LRESULT
            }
            None => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        },
        WM_COMMAND => {
            let id = wparam & 0xffff;
            with_ui(|ui| match id {
                ID_STOP_ALL => ui.stop(false),
                ID_STOP_CONTROL => ui.stop(true),
                ID_COLLAPSE | ID_EXPAND => {
                    ui.collapsed = id == ID_COLLAPSE;
                    ui.refresh_indicator();
                }
                _ => {}
            });
            0
        }
        // A screen added, removed or resized: the panel and the bands
        // follow. Every top-level window hears it: the panel answers.
        WM_DISPLAYCHANGE => {
            with_ui(|ui| {
                if ui.indicator.as_ref().is_some_and(|i| i.hwnd == hwnd) {
                    ui.refresh_indicator();
                }
            });
            0
        }
        // Closing the indicator stops everything.
        WM_CLOSE => {
            with_ui(|ui| {
                if ui.indicator.as_ref().is_some_and(|i| i.hwnd == hwnd) {
                    ui.stop(false);
                }
            });
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute};
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, GetDIBits, SRCCOPY,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::FindWindowW;

    fn pump(for_ms: u64) {
        let until = Instant::now() + Duration::from_millis(for_ms);
        while Instant::now() < until {
            unsafe {
                let mut msg: MSG = std::mem::zeroed();
                while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                    TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Saves what the screen shows of `hwnd` as a PNG file.
    fn capture(hwnd: HWND, path: &std::path::Path) {
        let mut r: RECT = unsafe { std::mem::zeroed() };
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS as u32,
                (&raw mut r).cast(),
                size_of::<RECT>() as u32,
            )
        };
        capture_rect(r, path);
    }

    /// Saves what the screen shows in `r` as a PNG file.
    fn capture_rect(r: RECT, path: &std::path::Path) {
        unsafe {
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let screen = GetDC(std::ptr::null_mut());
            let mem = CreateCompatibleDC(screen);
            let bitmap = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bitmap);
            BitBlt(mem, 0, 0, w, h, screen, r.left, r.top, SRCCOPY);
            SelectObject(mem, old);
            let mut info: BITMAPINFO = std::mem::zeroed();
            info.bmiHeader = BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            };
            let mut pixels = vec![0u8; (w * h * 4) as usize];
            GetDIBits(
                mem,
                bitmap,
                0,
                h as u32,
                pixels.as_mut_ptr().cast(),
                &mut info,
                DIB_RGB_COLORS,
            );
            DeleteObject(bitmap);
            DeleteDC(mem);
            ReleaseDC(std::ptr::null_mut(), screen);
            for p in pixels.as_chunks_mut::<4>().0 {
                p.swap(0, 2);
                p[3] = 255;
            }
            let file = std::fs::File::create(path).unwrap();
            let mut enc = png::Encoder::new(file, w as u32, h as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.write_header()
                .unwrap()
                .write_image_data(&pixels)
                .unwrap();
        }
    }

    /// Shows the request and the indicators on this desktop, and saves
    /// pictures of them in the temp directory (`consent-*.png`), to check
    /// their look: `cargo test -p ameleo-display-agent consent::tests::preview
    /// -- --ignored`.
    #[test]
    #[ignore = "shows windows on the desktop"]
    fn preview() {
        assert!(register_classes());
        let dir = std::env::temp_dir();
        let who = "Jeanne Martin (support)";
        for french in [true, false] {
            let lang = if french { "fr" } else { "en" };
            let (out, _answers) = mpsc::unbounded_channel();
            UI.with_borrow_mut(|ui| {
                *ui = Some(Ui {
                    out,
                    prompt: None,
                    granted: BTreeMap::new(),
                    indicator: None,
                    collapsed: false,
                    french,
                })
            });
            for (kind, name) in [(CONTROL, "control"), (VIEW, "view")] {
                UI.with_borrow_mut(|ui| {
                    ui.as_mut().unwrap().prompt = Some(Prompt {
                        id: 1,
                        kind,
                        who: who.into(),
                        hwnd: std::ptr::null_mut(),
                        withdrawn: false,
                    })
                });
                // Another thread takes the picture, then closes the dialog.
                let title = text(french, "Remote access", "Accès à distance");
                let path = dir.join(format!("consent-prompt-{name}-{lang}.png"));
                let shooter = std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(1500));
                    let hwnd = unsafe { FindWindowW(std::ptr::null(), wide(title).as_ptr()) };
                    assert!(!hwnd.is_null(), "no dialog");
                    capture(hwnd, &path);
                    unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
                });
                let (_tx, rx) = std_mpsc::channel();
                let locked = AtomicBool::new(false);
                let dialog = Dialog {
                    rx: &rx,
                    locked: &locked,
                    french,
                    armed: Cell::new(false),
                    shown: Cell::new(u64::from(TIMEOUT_SECS)),
                };
                show_prompt(kind, who, &dialog);
                shooter.join().unwrap();
                UI.with_borrow_mut(|ui| {
                    let ui = ui.as_mut().unwrap();
                    ui.granted.insert(1, (kind, who.into()));
                    ui.refresh_indicator();
                });
                pump(500);
                let hwnd =
                    UI.with_borrow(|ui| ui.as_ref().unwrap().indicator.as_ref().unwrap().hwnd);
                capture(hwnd, &dir.join(format!("consent-panel-{name}-{lang}.png")));
                // The whole primary screen: the bands around it.
                let screen = RECT {
                    left: 0,
                    top: 0,
                    right: unsafe { GetSystemMetrics(SM_CXSCREEN) },
                    bottom: unsafe { GetSystemMetrics(SM_CYSCREEN) },
                };
                capture_rect(
                    screen,
                    &dir.join(format!("consent-screen-{name}-{lang}.png")),
                );
                // Reduced to its tab.
                UI.with_borrow_mut(|ui| {
                    let ui = ui.as_mut().unwrap();
                    ui.collapsed = true;
                    ui.refresh_indicator();
                });
                pump(500);
                let hwnd =
                    UI.with_borrow(|ui| ui.as_ref().unwrap().indicator.as_ref().unwrap().hwnd);
                capture(hwnd, &dir.join(format!("consent-tab-{name}-{lang}.png")));
                UI.with_borrow_mut(|ui| {
                    let ui = ui.as_mut().unwrap();
                    ui.granted.clear();
                    ui.collapsed = false;
                    ui.refresh_indicator();
                });
            }
        }
    }
}
