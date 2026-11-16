//! Monitor geometry for placing windows.

use std::io;

use oxi_core::overlay::Rect;
use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

/// The work area (taskbar excluded, physical pixels) and the scale factor (1.0 at 96 DPI) of the
/// foreground window's monitor; the primary monitor when nothing is in the foreground.
pub fn work_area() -> io::Result<(Rect, f32)> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>().try_into().unwrap_or(u32::MAX),
        ..Default::default()
    };
    let (mut dpi, mut dpi_y) = (96_u32, 96_u32);
    // SAFETY: out pointers are live locals; MonitorFromWindow accepts a null window.
    unsafe {
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        if GetMonitorInfoW(monitor, &raw mut info) == 0 {
            return Err(io::Error::last_os_error());
        }
        // A failure leaves 96, the unscaled size.
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut dpi_y);
    }
    let RECT {
        left,
        top,
        right,
        bottom,
    } = info.rcWork;
    let work = Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    };
    #[allow(clippy::cast_precision_loss)] // a DPI is a few hundred
    Ok((work, dpi as f32 / 96.0))
}

#[cfg(test)]
mod tests {
    #[test]
    fn work_area_is_a_real_rectangle() {
        let (area, scale) = super::work_area().unwrap();
        assert!(area.w > 0 && area.h > 0, "{area:?}");
        assert!((0.5..=5.0).contains(&scale), "{scale}");
    }
}
