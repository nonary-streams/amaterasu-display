//! Windows HDR controls with an explicit compatibility path for older systems.
use crate::model::{DisplayError, DisplayResult};
use windows_sys::Win32::Devices::Display::*;
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, LUID,
};

// These Windows 11 packets are missing from windows-sys 0.61. Their fixed
// layouts follow the Windows SDK wingdi.h definitions. The HDR-specific API
// separates HDR from SDR wide-color management, which the legacy API cannot do.
// https://github.com/microsoft/win32metadata/blob/main/generation/WinSDK/RecompiledIdlHeaders/um/wingdi.h
const GET_ADVANCED_COLOR_INFO_2: DISPLAYCONFIG_DEVICE_INFO_TYPE = 15;
const SET_HDR_STATE: DISPLAYCONFIG_DEVICE_INFO_TYPE = 16;

#[repr(C)]
#[derive(Default)]
struct AdvancedColorInfo2 {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    color_encoding: i32,
    bits_per_color_channel: u32,
    active_color_mode: i32,
}

#[repr(C)]
struct HdrState {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
}

fn with_legacy_color_fallback<T>(
    operation: &str,
    modern: impl FnOnce() -> std::result::Result<T, i32>,
    legacy: impl FnOnce() -> std::result::Result<T, i32>,
) -> DisplayResult<T> {
    match modern() {
        Ok(value) => Ok(value),
        Err(code)
            if code == ERROR_INVALID_FUNCTION as i32
                || code == ERROR_INVALID_PARAMETER as i32
                || code == ERROR_NOT_SUPPORTED as i32 =>
        {
            legacy().map_err(|code| {
                DisplayError::new("platform_error", format!("{operation} failed: {code}"))
            })
        }
        Err(code) => Err(DisplayError::new(
            "platform_error",
            format!("{operation} failed: {code}"),
        )),
    }
}

/// Returns None for a target without HDR support. On recent Windows, WCG-only
/// SDR remains distinct from HDR and must not block a recoverable snapshot.
pub(super) fn read(path: &DISPLAYCONFIG_PATH_INFO) -> DisplayResult<Option<bool>> {
    let (modern, flags) = with_legacy_color_fallback(
        "DisplayConfigGetDeviceInfo(HDR)",
        || {
            let mut packet = AdvancedColorInfo2 {
                header: device_header(
                    GET_ADVANCED_COLOR_INFO_2,
                    std::mem::size_of::<AdvancedColorInfo2>(),
                    path.targetInfo.adapterId,
                    path.targetInfo.id,
                ),
                ..Default::default()
            };
            // SAFETY: header starts this repr(C) packet and describes its full
            // initialized writable storage. The synchronous API retains no pointer.
            let code = unsafe { DisplayConfigGetDeviceInfo(&mut packet.header) };
            if code == 0 {
                Ok((true, packet.flags))
            } else {
                Err(code)
            }
        },
        || {
            let mut packet = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
                header: device_header(
                    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                    std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>(),
                    path.targetInfo.adapterId,
                    path.targetInfo.id,
                ),
                ..Default::default()
            };
            // SAFETY: the initialized packet owns the output storage described
            // by its header for the complete synchronous native call.
            let code = unsafe { DisplayConfigGetDeviceInfo(&mut packet.header) };
            if code == ERROR_NOT_SUPPORTED as i32 {
                return Ok((false, 0));
            }
            if code == 0 {
                // SAFETY: value is the documented raw view of this initialized union.
                Ok((false, unsafe { packet.Anonymous.value }))
            } else {
                Err(code)
            }
        },
    )?;
    if modern {
        decode_hdr_flags(flags)
    } else {
        decode_advanced_color_flags(flags)
    }
}

fn decode_hdr_flags(flags: u32) -> DisplayResult<Option<bool>> {
    let supported = flags & (1 << 4) != 0;
    let enabled = flags & (1 << 5) != 0;
    if enabled && !supported {
        return Err(DisplayError::new(
            "platform_error",
            "HDR is enabled without reported target support",
        ));
    }
    Ok(supported.then_some(enabled))
}

fn decode_advanced_color_flags(flags: u32) -> DisplayResult<Option<bool>> {
    if flags & 4 != 0 {
        return Err(DisplayError::new(
            "unsupported_display",
            "wide-color-gamut state cannot be restored by the HDR recovery journal",
        ));
    }
    if flags & 2 != 0 && flags & 1 == 0 {
        return Err(DisplayError::new(
            "platform_error",
            "advanced color is enabled without reported target support",
        ));
    }
    Ok((flags & 1 != 0).then_some(flags & 2 != 0))
}

pub(super) fn write(path: &DISPLAYCONFIG_PATH_INFO, enabled: bool) -> DisplayResult<()> {
    with_legacy_color_fallback(
        "DisplayConfigSetDeviceInfo(HDR)",
        || {
            let packet = HdrState {
                header: device_header(
                    SET_HDR_STATE,
                    std::mem::size_of::<HdrState>(),
                    path.targetInfo.adapterId,
                    path.targetInfo.id,
                ),
                flags: u32::from(enabled),
            };
            // SAFETY: the repr(C) packet owns the complete initialized input
            // described by header; the synchronous API retains no pointer.
            let code = unsafe { DisplayConfigSetDeviceInfo(&packet.header) };
            if code == 0 { Ok(()) } else { Err(code) }
        },
        || {
            let packet = DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE {
                header: device_header(
                    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
                    std::mem::size_of::<DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE>(),
                    path.targetInfo.adapterId,
                    path.targetInfo.id,
                ),
                Anonymous: DISPLAYCONFIG_SET_ADVANCED_COLOR_STATE_0 {
                    value: u32::from(enabled),
                },
            };
            // SAFETY: this initialized repr(C) packet remains alive for the
            // synchronous native call and no input pointer is retained.
            let code = unsafe { DisplayConfigSetDeviceInfo(&packet.header) };
            if code == 0 { Ok(()) } else { Err(code) }
        },
    )
}

fn device_header(
    kind: DISPLAYCONFIG_DEVICE_INFO_TYPE,
    size: usize,
    adapter: LUID,
    id: u32,
) -> DISPLAYCONFIG_DEVICE_INFO_HEADER {
    DISPLAYCONFIG_DEVICE_INFO_HEADER {
        r#type: kind,
        size: size as u32,
        adapterId: adapter,
        id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modern_hdr_flags_distinguish_wide_color_sdr_and_hdr() {
        assert_eq!(decode_hdr_flags(0).unwrap(), None);
        assert_eq!(decode_hdr_flags(0xc3).unwrap(), None);
        assert_eq!(decode_hdr_flags(0x10).unwrap(), Some(false));
        assert_eq!(decode_hdr_flags(0x30).unwrap(), Some(true));
        assert_eq!(decode_hdr_flags(0xf3).unwrap(), Some(true));
        assert!(decode_hdr_flags(0x20).is_err());
    }

    #[test]
    fn modern_hdr_api_success_does_not_touch_legacy_wide_color_state() {
        let result = with_legacy_color_fallback(
            "HDR",
            || Ok::<_, i32>(0xc3),
            || panic!("modern HDR support must preserve independent WCG state"),
        );
        assert_eq!(result.unwrap(), 0xc3);
    }

    #[test]
    fn unavailable_modern_hdr_api_falls_back_but_other_errors_do_not() {
        for code in [1, 50, 87] {
            assert_eq!(
                with_legacy_color_fallback("HDR", || Err(code), || Ok::<_, i32>(0x3)).unwrap(),
                0x3,
            );
        }
        let error = with_legacy_color_fallback::<u32>(
            "HDR",
            || Err(5),
            || panic!("access denied must not trigger another native API"),
        )
        .unwrap_err();
        assert_eq!(error.code, "platform_error");
        assert!(error.message.contains('5'));
        assert!(with_legacy_color_fallback::<u32>("HDR", || Err(50), || Err(5)).is_err());
    }

    #[test]
    fn modern_hdr_packets_match_sdk_layout() {
        assert_eq!(std::mem::size_of::<super::AdvancedColorInfo2>(), 36);
        assert_eq!(std::mem::offset_of!(super::AdvancedColorInfo2, flags), 20);
        assert_eq!(
            std::mem::offset_of!(super::AdvancedColorInfo2, active_color_mode),
            32
        );
        assert_eq!(std::mem::size_of::<super::HdrState>(), 24);
        assert_eq!(std::mem::offset_of!(super::HdrState, flags), 20);
    }

    #[test]
    fn advanced_color_support_and_enablement_bits_are_distinct() {
        assert_eq!(decode_advanced_color_flags(0).unwrap(), None);
        assert_eq!(decode_advanced_color_flags(1).unwrap(), Some(false));
        assert_eq!(decode_advanced_color_flags(3).unwrap(), Some(true));
        assert!(decode_advanced_color_flags(2).is_err());
        assert!(decode_advanced_color_flags(5).is_err());
    }
}
