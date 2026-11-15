// SPDX-License-Identifier: GPL-3.0-only

//! Camera controls through `IKsControl`: the VideoProcAmp and CameraControl
//! property sets (what `IAMVideoProcAmp`, `IAMCameraControl`, the Windows
//! camera settings and apps use), served from the controls the agent put
//! in the shared memory.
//!
//! MS-RDPECAM and KS number the properties differently, and their manual /
//! auto flags are swapped: the tables below map them.

use vcam_shm::control::{AUTO, CAMERA_CONTROL, MANUAL, VIDEO_PROC_AMP};
use vcam_shm::{ControlInfo, Shm};
use windows::Win32::Foundation::{E_INVALIDARG, E_NOTIMPL, E_POINTER};
use windows_core::{GUID, HRESULT};

/// `HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND)`: a property set we do not have.
pub const E_SET_NOT_FOUND: HRESULT = HRESULT(0x8007_0492_u32 as i32);
/// `HRESULT_FROM_WIN32(ERROR_NOT_FOUND)`: a property of the set we do not have.
const E_NOT_FOUND: HRESULT = HRESULT(0x8007_0490_u32 as i32);
/// `HRESULT_FROM_WIN32(ERROR_MORE_DATA)`: the size needed is in `returned`.
const E_MORE_DATA: HRESULT = HRESULT(0x8007_00EA_u32 as i32);

const PROPSETID_VIDCAP_VIDEOPROCAMP: GUID = GUID::from_u128(0xC6E13360_30AC_11D0_A18C_00A0C9118956);
const PROPSETID_VIDCAP_CAMERACONTROL: GUID =
    GUID::from_u128(0xC6E13370_30AC_11D0_A18C_00A0C9118956);
const KSPROPTYPESETID_GENERAL: GUID = GUID::from_u128(0x97E99BA0_BDEA_11CF_A5D6_28DB04C10000);

const KSPROPERTY_TYPE_GET: u32 = 0x1;
const KSPROPERTY_TYPE_SET: u32 = 0x2;
const KSPROPERTY_TYPE_BASICSUPPORT: u32 = 0x200;
const KSPROPERTY_TYPE_DEFAULTVALUES: u32 = 0x1_0000;
const KSPROPERTY_MEMBER_STEPPEDRANGES: u32 = 2;
const KSPROPERTY_MEMBER_VALUES: u32 = 3;
const KSPROPERTY_MEMBER_FLAG_DEFAULT: u32 = 1;
const VT_I4: u32 = 3;

/// `KSPROPERTY_*_FLAGS_AUTO` / `_MANUAL`, both sets.
const KS_AUTO: u32 = 0x1;
const KS_MANUAL: u32 = 0x2;

/// `(MS-RDPECAM property, KS property)` per set.
const CAMERA_CONTROL_IDS: [(u8, u32); 6] = [
    (0x01, 4), // Exposure
    (0x02, 6), // Focus
    (0x03, 0), // Pan
    (0x04, 2), // Roll
    (0x05, 1), // Tilt
    (0x06, 3), // Zoom
];
const VIDEO_PROC_AMP_IDS: [(u8, u32); 5] = [
    (0x01, 8), // BacklightCompensation
    (0x02, 0), // Brightness
    (0x03, 1), // Contrast
    (0x04, 2), // Hue
    (0x05, 7), // WhiteBalance
];

/// `KSPROPERTY` (`KSIDENTIFIER`).
#[repr(C)]
#[derive(Clone, Copy)]
struct KsProperty {
    set: GUID,
    id: u32,
    flags: u32,
}

/// `KSPROPERTY_VIDEOPROCAMP_S` and `KSPROPERTY_CAMERACONTROL_S`: 40 bytes,
/// `KSPROPERTY` being 8-byte aligned (callers may give the 36 that count).
#[repr(C)]
#[derive(Clone, Copy)]
struct ControlS {
    property: KsProperty,
    value: i32,
    flags: u32,
    capabilities: u32,
    _pad: u32,
}

/// The bytes of [`ControlS`] that carry something.
const CONTROL_S_MIN: usize = 36;

/// `KSPROPERTY_DESCRIPTION`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Description {
    access_flags: u32,
    description_size: u32,
    prop_type_set: KsProperty,
    members_list_count: u32,
    reserved: u32,
}

/// `KSPROPERTY_MEMBERSHEADER`.
#[repr(C)]
#[derive(Clone, Copy)]
struct MembersHeader {
    members_flags: u32,
    members_size: u32,
    members_count: u32,
    flags: u32,
}

/// `KSPROPERTY_STEPPING_LONG`.
#[repr(C)]
#[derive(Clone, Copy)]
struct SteppingLong {
    stepping_delta: u32,
    reserved: u32,
    minimum: i32,
    maximum: i32,
}

const _: () = assert!(std::mem::size_of::<KsProperty>() == 24);
const _: () = assert!(std::mem::size_of::<ControlS>() == 40);
const _: () = assert!(std::mem::size_of::<Description>() == 40);

fn ks_flags(rdp: u8) -> u32 {
    (if rdp & MANUAL != 0 { KS_MANUAL } else { 0 }) | (if rdp & AUTO != 0 { KS_AUTO } else { 0 })
}

/// The MS-RDPECAM `(set, property)` of a KS property.
fn rdp_property(set: &GUID, id: u32) -> Option<Option<(u8, u8)>> {
    let (rdp_set, table): (u8, &[(u8, u32)]) = if *set == PROPSETID_VIDCAP_VIDEOPROCAMP {
        (VIDEO_PROC_AMP, &VIDEO_PROC_AMP_IDS)
    } else if *set == PROPSETID_VIDCAP_CAMERACONTROL {
        (CAMERA_CONTROL, &CAMERA_CONTROL_IDS)
    } else {
        return None; // not a set of ours
    };
    Some(
        table
            .iter()
            .find(|(_, ks)| *ks == id)
            .map(|(rdp, _)| (rdp_set, *rdp)),
    )
}

/// Writes the structs in `parts` one after another into `data`.
unsafe fn write(data: *mut u8, parts: &[&[u8]]) -> u32 {
    let mut at = 0;
    for p in parts {
        unsafe { std::ptr::copy_nonoverlapping(p.as_ptr(), data.add(at), p.len()) };
        at += p.len();
    }
    at as u32
}

fn bytes<T>(v: &T) -> &[u8] {
    unsafe { std::slice::from_raw_parts((v as *const T).cast(), std::mem::size_of::<T>()) }
}

/// `IKsControl::KsProperty` for the camera control sets, logged.
///
/// # Safety
/// The pointers and lengths are the caller's buffers.
pub unsafe fn ks_property(
    shm: Option<&Shm>,
    property: *const u8,
    property_len: u32,
    data: *mut u8,
    data_len: u32,
    returned: *mut u32,
) -> windows_core::Result<()> {
    let r = unsafe { ks_property_inner(shm, property, property_len, data, data_len, returned) };
    if !property.is_null() && property_len as usize >= std::mem::size_of::<KsProperty>() {
        let p: KsProperty = unsafe { property.cast::<KsProperty>().read_unaligned() };
        let set = if p.set == PROPSETID_VIDCAP_VIDEOPROCAMP {
            "VideoProcAmp".to_string()
        } else if p.set == PROPSETID_VIDCAP_CAMERACONTROL {
            "CameraControl".to_string()
        } else {
            format!("{:?}", p.set)
        };
        let res = match &r {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("{:#010x}", e.code().0),
        };
        crate::log(&format!(
            "KsProperty {set} id {} flags {:#x} property {property_len} data {data_len}: {res}",
            p.id, p.flags
        ));
    }
    r
}

unsafe fn ks_property_inner(
    shm: Option<&Shm>,
    property: *const u8,
    property_len: u32,
    data: *mut u8,
    data_len: u32,
    returned: *mut u32,
) -> windows_core::Result<()> {
    if property.is_null() || (property_len as usize) < std::mem::size_of::<KsProperty>() {
        return Err(E_INVALIDARG.into());
    }
    let p: KsProperty = unsafe { property.cast::<KsProperty>().read_unaligned() };
    let Some(target) = rdp_property(&p.set, p.id) else {
        return Err(E_SET_NOT_FOUND.into());
    };
    let Some(shm) = shm else {
        return Err(E_SET_NOT_FOUND.into());
    };
    let rdp_set = if p.set == PROPSETID_VIDCAP_VIDEOPROCAMP {
        VIDEO_PROC_AMP
    } else {
        CAMERA_CONTROL
    };
    if !shm.has_control_set(rdp_set) {
        return Err(E_SET_NOT_FOUND.into());
    }
    let Some((info, mode, value)) = target.and_then(|(s, id)| shm.control(s, id)) else {
        return Err(E_NOT_FOUND.into());
    };
    let set_returned = |n: u32| {
        if !returned.is_null() {
            unsafe { *returned = n };
        }
    };
    // The request type, whatever other flags come with it (topology,
    // priority...).
    let kind = [
        KSPROPERTY_TYPE_BASICSUPPORT,
        KSPROPERTY_TYPE_DEFAULTVALUES,
        KSPROPERTY_TYPE_SET,
        KSPROPERTY_TYPE_GET,
    ]
    .into_iter()
    .find(|k| p.flags & k != 0)
    .unwrap_or(0);
    match kind {
        KSPROPERTY_TYPE_GET => {
            let size = std::mem::size_of::<ControlS>();
            if (data_len as usize) < CONTROL_S_MIN || data.is_null() {
                set_returned(size as u32);
                return Err(if data_len == 0 {
                    E_MORE_DATA
                } else {
                    E_INVALIDARG
                }
                .into());
            }
            let s = ControlS {
                property: p,
                value,
                flags: ks_flags(mode),
                capabilities: ks_flags(info.caps),
                _pad: 0,
            };
            let n = size.min(data_len as usize);
            set_returned(unsafe { write(data, &[&bytes(&s)[..n]]) });
            Ok(())
        }
        KSPROPERTY_TYPE_SET => {
            // The value is in the data buffer, or in the property one.
            let (src, len) = if !data.is_null() && data_len as usize >= CONTROL_S_MIN {
                (data as *const u8, data_len as usize)
            } else if property_len as usize >= CONTROL_S_MIN {
                (property, property_len as usize)
            } else {
                return Err(E_INVALIDARG.into());
            };
            let mut s = ControlS {
                property: p,
                value: 0,
                flags: 0,
                capabilities: 0,
                _pad: 0,
            };
            let n = std::mem::size_of::<ControlS>().min(len);
            unsafe {
                std::ptr::copy_nonoverlapping(src, (&mut s as *mut ControlS).cast::<u8>(), n)
            };
            let (mode, value) = if s.flags & KS_AUTO != 0 {
                (AUTO, value)
            } else {
                (MANUAL, s.value)
            };
            if mode & info.caps == 0 || (mode == MANUAL && !(info.min..=info.max).contains(&value))
            {
                return Err(E_INVALIDARG.into());
            }
            shm.request_control(info.set, info.property, mode, value);
            set_returned(0);
            Ok(())
        }
        KSPROPERTY_TYPE_BASICSUPPORT => {
            let members = MembersHeader {
                members_flags: KSPROPERTY_MEMBER_STEPPEDRANGES,
                members_size: std::mem::size_of::<SteppingLong>() as u32,
                members_count: 1,
                flags: 0,
            };
            let stepping = SteppingLong {
                stepping_delta: info.step.max(1) as u32,
                reserved: 0,
                minimum: info.min,
                maximum: info.max,
            };
            describe(
                &info,
                data,
                data_len,
                set_returned,
                &[bytes(&members), bytes(&stepping)],
            )
        }
        KSPROPERTY_TYPE_DEFAULTVALUES => {
            let members = MembersHeader {
                members_flags: KSPROPERTY_MEMBER_VALUES,
                members_size: 4,
                members_count: 1,
                flags: KSPROPERTY_MEMBER_FLAG_DEFAULT,
            };
            describe(
                &info,
                data,
                data_len,
                set_returned,
                &[bytes(&members), bytes(&info.default)],
            )
        }
        _ => Err(E_NOTIMPL.into()),
    }
}

/// A `KSPROPERTY_DESCRIPTION` followed by `members`, or as much of it as the
/// caller asks for (the access flags alone, the description alone).
fn describe(
    _info: &ControlInfo,
    data: *mut u8,
    data_len: u32,
    set_returned: impl Fn(u32),
    members: &[&[u8]],
) -> windows_core::Result<()> {
    let total = std::mem::size_of::<Description>() + members.iter().map(|m| m.len()).sum::<usize>();
    let d = Description {
        access_flags: KSPROPERTY_TYPE_GET
            | KSPROPERTY_TYPE_SET
            | KSPROPERTY_TYPE_BASICSUPPORT
            | KSPROPERTY_TYPE_DEFAULTVALUES,
        description_size: total as u32,
        prop_type_set: KsProperty {
            set: KSPROPTYPESETID_GENERAL,
            id: VT_I4,
            flags: 0,
        },
        members_list_count: 1,
        reserved: 0,
    };
    let len = data_len as usize;
    if data.is_null() && len > 0 {
        return Err(E_POINTER.into());
    }
    if len >= total {
        let mut parts = vec![bytes(&d)];
        parts.extend_from_slice(members);
        set_returned(unsafe { write(data, &parts) });
    } else if len >= std::mem::size_of::<Description>() {
        set_returned(unsafe { write(data, &[bytes(&d)]) });
    } else if len >= 4 {
        set_returned(unsafe { write(data, &[bytes(&d.access_flags)]) });
    } else {
        set_returned(total as u32);
        return Err(E_MORE_DATA.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps() {
        // Brightness is VideoProcAmp 0x02 in MS-RDPECAM, KS property 0.
        assert_eq!(
            rdp_property(&PROPSETID_VIDCAP_VIDEOPROCAMP, 0),
            Some(Some((VIDEO_PROC_AMP, 0x02)))
        );
        // Focus: CameraControl 0x02, KS 6.
        assert_eq!(
            rdp_property(&PROPSETID_VIDCAP_CAMERACONTROL, 6),
            Some(Some((CAMERA_CONTROL, 0x02)))
        );
        // Iris has no MS-RDPECAM equivalent.
        assert_eq!(rdp_property(&PROPSETID_VIDCAP_CAMERACONTROL, 5), Some(None));
        assert_eq!(rdp_property(&GUID::zeroed(), 0), None);
        // Manual and auto flags are swapped between the two.
        assert_eq!(ks_flags(MANUAL), KS_MANUAL);
        assert_eq!(ks_flags(AUTO), KS_AUTO);
        assert_eq!(ks_flags(MANUAL | AUTO), KS_MANUAL | KS_AUTO);
    }
}
