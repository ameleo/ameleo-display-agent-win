// SPDX-License-Identifier: GPL-3.0-only

//! Absolute pointer injection over the whole virtual desktop.
//!
//! The host addresses a head (virtio-gpu scanout index). The display driver
//! uses the scanout index as `VidPnTargetId`, which `QueryDisplayConfig`
//! reports as the path target id. The head origin comes from the source
//! mode of that path, so the mapping follows any layout the user picks.

use proto::Button;
use windows_sys::Win32::Devices::Display::{
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
    GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
};
use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_SUCCESS, GetLastError};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, SendInput,
};

const WHEEL_DELTA: i32 = 120;
const XBUTTON1: i32 = 1;
const XBUTTON2: i32 = 2;

#[derive(Debug, Clone, Copy)]
struct Head {
    target_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Default)]
pub struct Mouse {
    heads: Vec<Head>,
    /// Virtual desktop bounds (union of the active sources).
    origin: (i32, i32),
    size: (u32, u32),
    /// Buttons currently held down, by `Button` number.
    pressed: u16,
}

impl Mouse {
    pub fn new() -> Self {
        let mut m = Self::default();
        m.refresh();
        m
    }

    /// Re-reads the display layout (call on WM_DISPLAYCHANGE).
    pub fn refresh(&mut self) {
        match query_heads() {
            Ok(heads) => {
                let left = heads.iter().map(|h| h.x).min().unwrap_or(0);
                let top = heads.iter().map(|h| h.y).min().unwrap_or(0);
                let right = heads
                    .iter()
                    .map(|h| h.x + h.width as i32)
                    .max()
                    .unwrap_or(1);
                let bottom = heads
                    .iter()
                    .map(|h| h.y + h.height as i32)
                    .max()
                    .unwrap_or(1);
                self.origin = (left, top);
                self.size = ((right - left).max(1) as u32, (bottom - top).max(1) as u32);
                tracing::debug!(?heads, "display layout");
                self.heads = heads;
            }
            Err(e) => tracing::warn!("QueryDisplayConfig failed: {e}"),
        }
    }

    fn head(&self, head: u32) -> Option<Head> {
        self.heads
            .iter()
            .find(|h| h.target_id == head)
            // Unknown target ids (other display driver): fall back to path order.
            .or_else(|| self.heads.get(head as usize))
            .copied()
    }

    pub fn move_to(&mut self, head: u32, x: u32, y: u32) {
        let h = match self.head(head) {
            Some(h) => h,
            None => {
                self.refresh();
                match self.head(head) {
                    Some(h) => h,
                    None => return,
                }
            }
        };
        let px = (h.x + x.min(h.width.saturating_sub(1)) as i32 - self.origin.0) as i64;
        let py = (h.y + y.min(h.height.saturating_sub(1)) as i32 - self.origin.1) as i64;
        let w = (self.size.0 as i64 - 1).max(1);
        let hh = (self.size.1 as i64 - 1).max(1);
        let dx = (px * 65535 + w / 2) / w;
        let dy = (py * 65535 + hh / 2) / hh;
        send(
            dx as i32,
            dy as i32,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        );
    }

    pub fn button(&mut self, button: u32, down: bool) {
        let Ok(button) = Button::try_from(button) else {
            return;
        };
        if !is_wheel(button) {
            let bit = 1u16 << button as u32;
            if down {
                self.pressed |= bit;
            } else {
                self.pressed &= !bit;
            }
        }
        let (flags, data) = match (button, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Side, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
            (Button::Side, false) => (MOUSEEVENTF_XUP, XBUTTON1),
            (Button::Extra, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
            (Button::Extra, false) => (MOUSEEVENTF_XUP, XBUTTON2),
            // Wheel "buttons" fire on press only.
            (Button::WheelUp, true) => (MOUSEEVENTF_WHEEL, WHEEL_DELTA),
            (Button::WheelDown, true) => (MOUSEEVENTF_WHEEL, -WHEEL_DELTA),
            (Button::WheelLeft, true) => (MOUSEEVENTF_HWHEEL, -WHEEL_DELTA),
            (Button::WheelRight, true) => (MOUSEEVENTF_HWHEEL, WHEEL_DELTA),
            _ => return,
        };
        send(0, 0, data, flags);
    }

    /// Releases every button the host left down (host gone mid-drag).
    pub fn release_all(&mut self) {
        for n in 0..16u32 {
            if self.pressed & (1 << n) != 0 {
                tracing::debug!(button = n, "releasing button left down");
                self.button(n, false);
            }
        }
    }

    /// Windows convention: `dy > 0` scrolls up, `dx > 0` scrolls right.
    pub fn scroll(&mut self, dx: i32, dy: i32) {
        if dy != 0 {
            send(0, 0, dy, MOUSEEVENTF_WHEEL);
        }
        if dx != 0 {
            send(0, 0, dx, MOUSEEVENTF_HWHEEL);
        }
    }
}

fn is_wheel(b: Button) -> bool {
    matches!(
        b,
        Button::WheelUp | Button::WheelDown | Button::WheelLeft | Button::WheelRight
    )
}

fn send(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) {
    let mut input: INPUT = unsafe { std::mem::zeroed() };
    input.r#type = INPUT_MOUSE;
    input.Anonymous.mi.dx = dx;
    input.Anonymous.mi.dy = dy;
    input.Anonymous.mi.mouseData = data as _;
    input.Anonymous.mi.dwFlags = flags;
    if unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) } != 1 {
        let err = unsafe { GetLastError() };
        // UIPI blocks injection into some elevated windows: not fatal.
        if err != ERROR_SUCCESS && err != ERROR_ACCESS_DENIED {
            tracing::warn!("SendInput failed: {err}");
        }
    }
}

fn query_heads() -> std::io::Result<Vec<Head>> {
    loop {
        let (mut np, mut nm) = (0u32, 0u32);
        let r = unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut np, &mut nm) };
        if r != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(r as i32));
        }
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            vec![unsafe { std::mem::zeroed() }; np as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            vec![unsafe { std::mem::zeroed() }; nm as usize];
        let r = unsafe {
            QueryDisplayConfig(
                QDC_ONLY_ACTIVE_PATHS,
                &mut np,
                paths.as_mut_ptr(),
                &mut nm,
                modes.as_mut_ptr(),
                std::ptr::null_mut(),
            )
        };
        if r == windows_sys::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER {
            continue; // layout changed between the two calls
        }
        if r != ERROR_SUCCESS {
            return Err(std::io::Error::from_raw_os_error(r as i32));
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);

        let mut heads = Vec::new();
        for p in &paths {
            let idx = unsafe { p.sourceInfo.Anonymous.modeInfoIdx } as usize;
            let Some(m) = modes.get(idx) else { continue };
            if m.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                continue;
            }
            let s = unsafe { m.Anonymous.sourceMode };
            heads.push(Head {
                target_id: p.targetInfo.id,
                x: s.position.x,
                y: s.position.y,
                width: s.width,
                height: s.height,
            });
        }
        return Ok(heads);
    }
}
