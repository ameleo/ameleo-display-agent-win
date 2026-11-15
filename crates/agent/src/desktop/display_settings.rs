// SPDX-License-Identifier: GPL-3.0-or-later

//! Desktop effects toggled for remote sessions (wallpaper, font smoothing,
//! animations).
//!
//! The choice is persisted machine-wide and re-applied at every logon, like
//! vdagent did. Re-enabling an effect restores the user's own preference
//! from HKCU instead of forcing it on.

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_OPTION_NON_VOLATILE,
    RRF_RT_REG_BINARY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegGetValueW,
    RegOpenCurrentUser, RegSetValueExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ANIMATIONINFO, SPI_GETDESKWALLPAPER, SPI_GETFONTSMOOTHING, SPI_SETACTIVEWINDOWTRACKING,
    SPI_SETACTIVEWNDTRKZORDER, SPI_SETANIMATION, SPI_SETCOMBOBOXANIMATION, SPI_SETCURSORSHADOW,
    SPI_SETDESKWALLPAPER, SPI_SETFONTSMOOTHING, SPI_SETGRADIENTCAPTIONS, SPI_SETHOTTRACKING,
    SPI_SETKEYBOARDCUES, SPI_SETLISTBOXSMOOTHSCROLLING, SPI_SETMENUANIMATION, SPI_SETMENUFADE,
    SPI_SETSELECTIONFADE, SPI_SETTOOLTIPANIMATION, SPI_SETTOOLTIPFADE, SPI_SETUIEFFECTS,
    SYSTEM_PARAMETERS_INFO_ACTION, SystemParametersInfoW,
};

use crate::user::Impersonation;
use crate::win::{from_wide, wide};

pub const REGISTRY_KEY: &str = r"SOFTWARE\ameleo-display-agent";
const MASK_VALUE: &str = "DisplaySettingMask";

const NO_WALLPAPER: u32 = 1 << 0;
const NO_FONT_SMOOTHING: u32 = 1 << 1;
const NO_ANIMATION: u32 = 1 << 2;

/// `true` means the effect is disabled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Effects {
    pub no_wallpaper: bool,
    pub no_font_smoothing: bool,
    pub no_animation: bool,
}

impl Effects {
    fn mask(self) -> u32 {
        (self.no_wallpaper as u32 * NO_WALLPAPER)
            | (self.no_font_smoothing as u32 * NO_FONT_SMOOTHING)
            | (self.no_animation as u32 * NO_ANIMATION)
    }

    fn from_mask(m: u32) -> Self {
        Self {
            no_wallpaper: m & NO_WALLPAPER != 0,
            no_font_smoothing: m & NO_FONT_SMOOTHING != 0,
            no_animation: m & NO_ANIMATION != 0,
        }
    }
}

pub fn save(e: Effects) {
    let key = wide(REGISTRY_KEY);
    let name = wide(MASK_VALUE);
    let mask = e.mask();
    unsafe {
        let mut hkey: HKEY = std::ptr::null_mut();
        let r = RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            key.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            std::ptr::null(),
            &mut hkey,
            std::ptr::null_mut(),
        );
        if r != ERROR_SUCCESS {
            tracing::warn!("cannot create HKLM\\{REGISTRY_KEY}: {r}");
            return;
        }
        RegSetValueExW(
            hkey,
            name.as_ptr(),
            0,
            REG_DWORD,
            (&mask as *const u32).cast(),
            4,
        );
        RegCloseKey(hkey);
    }
}

pub fn load() -> Option<Effects> {
    read_dword(HKEY_LOCAL_MACHINE, REGISTRY_KEY, MASK_VALUE).map(Effects::from_mask)
}

fn read_dword(root: HKEY, key: &str, value: &str) -> Option<u32> {
    let (key, value) = (wide(key), wide(value));
    let mut data = 0u32;
    let mut size = 4u32;
    let r = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            (&mut data as *mut u32).cast(),
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then_some(data)
}

fn read_string(root: HKEY, key: &str, value: &str) -> Option<String> {
    let (key, value) = (wide(key), wide(value));
    let mut buf = vec![0u16; 1024];
    let mut size = (buf.len() * 2) as u32;
    let r = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then(|| from_wide(&buf))
}

fn read_binary(root: HKEY, key: &str, value: &str) -> Option<Vec<u8>> {
    let (key, value) = (wide(key), wide(value));
    let mut buf = vec![0u8; 64];
    let mut size = buf.len() as u32;
    let r = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut size,
        )
    };
    (r == ERROR_SUCCESS).then(|| {
        buf.truncate(size as usize);
        buf
    })
}

fn spi(action: SYSTEM_PARAMETERS_INFO_ACTION, ui: u32, pv: *mut core::ffi::c_void) -> bool {
    let ok = unsafe { SystemParametersInfoW(action, ui, pv, 0) } != 0;
    if !ok {
        tracing::debug!(
            "SystemParametersInfo({action}) failed: {}",
            std::io::Error::last_os_error()
        );
    }
    ok
}

fn spi_bool(action: SYSTEM_PARAMETERS_INFO_ACTION, value: bool) {
    spi(action, 0, value as usize as *mut _);
}

fn set_min_animate(on: bool) {
    let mut ai = ANIMATIONINFO {
        cbSize: std::mem::size_of::<ANIMATIONINFO>() as u32,
        iMinAnimate: on as i32,
    };
    spi(
        SPI_SETANIMATION,
        ai.cbSize,
        (&mut ai as *mut ANIMATIONINFO).cast(),
    );
}

/// Font smoothing in grayscale rather than ClearType, for this session only
/// (not written to the profile). ClearType's colored subpixels are made for
/// the panel they are drawn on, not the client's unknown screen, and they
/// do not survive the video encoding: fringes, and more bits to send. A
/// session with smoothing off keeps it off.
///
/// Apps get `WM_SETTINGCHANGE`, but browsers often read the setting only
/// at start: done at logon, before the user starts them.
pub fn grayscale_font_smoothing() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FE_FONTSMOOTHINGSTANDARD, SPI_GETFONTSMOOTHINGTYPE, SPI_SETFONTSMOOTHINGTYPE,
        SPIF_SENDCHANGE,
    };
    let mut on = 0i32;
    if !spi(SPI_GETFONTSMOOTHING, 0, (&mut on as *mut i32).cast()) || on == 0 {
        return;
    }
    let mut kind = 0u32;
    if spi(SPI_GETFONTSMOOTHINGTYPE, 0, (&mut kind as *mut u32).cast())
        && kind == FE_FONTSMOOTHINGSTANDARD
    {
        return;
    }
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_SETFONTSMOOTHINGTYPE,
            0,
            FE_FONTSMOOTHINGSTANDARD as usize as *mut _,
            SPIF_SENDCHANGE,
        )
    } != 0;
    if ok {
        tracing::info!("font smoothing set to grayscale for the session");
    } else {
        tracing::warn!(
            "cannot set grayscale font smoothing: {}",
            std::io::Error::last_os_error()
        );
    }
}

/// Applies `e` to the current session. Must run on a thread attached to the
/// user desktop.
pub fn apply(e: Effects) {
    tracing::info!(?e, "applying desktop effects");
    if e.no_wallpaper {
        let mut empty = wide("");
        spi(SPI_SETDESKWALLPAPER, 0, empty.as_mut_ptr().cast());
    }
    if e.no_font_smoothing {
        spi_bool(SPI_SETFONTSMOOTHING, false);
    }
    if e.no_animation {
        spi_bool(SPI_SETUIEFFECTS, false);
        set_min_animate(false);
    }
    if !(e.no_wallpaper && e.no_font_smoothing && e.no_animation) {
        restore_user_preferences(e);
    }
    // Smoothing turned back on comes with the profile's type.
    grayscale_font_smoothing();
}

/// Re-reads the user's own settings for every effect that is not disabled.
fn restore_user_preferences(e: Effects) {
    let _as_user = match Impersonation::begin() {
        Ok(i) => i,
        Err(err) => {
            tracing::debug!("no logged on user to restore preferences from: {err}");
            return;
        }
    };
    let mut hkcu: HKEY = std::ptr::null_mut();
    if unsafe { RegOpenCurrentUser(KEY_READ, &mut hkcu) } != ERROR_SUCCESS {
        return;
    }
    const DESKTOP: &str = r"Control Panel\Desktop";

    if !e.no_wallpaper {
        let mut current = vec![0u16; 1024];
        let is_empty = spi(
            SPI_GETDESKWALLPAPER,
            current.len() as u32,
            current.as_mut_ptr().cast(),
        ) && from_wide(&current).is_empty();
        if is_empty && let Some(path) = read_string(hkcu, DESKTOP, "Wallpaper") {
            let mut path = wide(path);
            spi(SPI_SETDESKWALLPAPER, 0, path.as_mut_ptr().cast());
        }
    }
    if !e.no_font_smoothing && read_string(hkcu, DESKTOP, "FontSmoothing").as_deref() == Some("2") {
        let mut on = 0i32;
        spi(SPI_GETFONTSMOOTHING, 0, (&mut on as *mut i32).cast());
        if on == 0 {
            spi_bool(SPI_SETFONTSMOOTHING, true);
        }
    }
    if !e.no_animation {
        if read_string(hkcu, &format!(r"{DESKTOP}\WindowMetrics"), "MinAnimate").as_deref()
            == Some("1")
        {
            set_min_animate(true);
        }
        if let Some(mask) = read_binary(hkcu, DESKTOP, "UserPreferencesMask") {
            let mut first = [0u8; 4];
            for (d, s) in first.iter_mut().zip(&mask) {
                *d = *s;
            }
            let m = u32::from_le_bytes(first);
            for (action, bit) in [
                (SPI_SETUIEFFECTS, 0x8000_0000),
                (SPI_SETACTIVEWINDOWTRACKING, 0x01),
                (SPI_SETMENUANIMATION, 0x02),
                (SPI_SETCOMBOBOXANIMATION, 0x04),
                (SPI_SETLISTBOXSMOOTHSCROLLING, 0x08),
                (SPI_SETGRADIENTCAPTIONS, 0x10),
                (SPI_SETKEYBOARDCUES, 0x20),
                (SPI_SETACTIVEWNDTRKZORDER, 0x40),
                (SPI_SETHOTTRACKING, 0x80),
                (SPI_SETMENUFADE, 0x200),
                (SPI_SETSELECTIONFADE, 0x400),
                (SPI_SETTOOLTIPANIMATION, 0x800),
                (SPI_SETTOOLTIPFADE, 0x1000),
                (SPI_SETCURSORSHADOW, 0x2000),
            ] {
                spi_bool(action, m & bit != 0);
            }
        }
    }
    unsafe { RegCloseKey(hkcu) };
}
