//! Live overlay window. Runs only with `OXI_OVERLAY_TEST=1`: it puts the pill on screen for a
//! moment. With `OXI_SHOTS_DIR` set it also saves what the screen showed.
#![cfg(windows)]
#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // screen pixels and Win32 sizes are small and positive

use std::path::PathBuf;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

use ab_glyph::FontRef;
use oxi_core::config::{Config, OverlayPosition};
use oxi_core::machine::OverlayState;
use oxi_core::overlay::{PILL_PAD, Rect, palette::SURFACE, place};
use oxi_platform::capture::LevelMeter;
use oxi_platform::os::overlay::Overlay;
use tiny_skia::Pixmap;
use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CAPTUREBLT, CreateCompatibleDC, CreateDIBSection,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetMonitorInfoW, MONITOR_DEFAULTTOPRIMARY,
    MONITORINFO, MonitorFromWindow, ReleaseDC, SRCCOPY, SelectObject,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GWL_EXSTYLE, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect,
    IsWindowVisible, MSG, PM_REMOVE, PeekMessageW, WM_TIMER, WS_EX_APPWINDOW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WindowFromPoint,
};

const FONT: &str = "C:/Windows/Fonts/segoeui.ttf";
const RECORDING: OverlayState = OverlayState::Recording {
    locked: false,
    ending_soon: false,
};

#[test]
fn overlay_window() {
    if std::env::var_os("OXI_OVERLAY_TEST").is_none_or(|v| v != "1") {
        eprintln!("skipped: set OXI_OVERLAY_TEST=1");
        return;
    }
    let bytes = std::fs::read(FONT).ok();
    let font = bytes
        .as_deref()
        .map(|b| FontRef::try_from_slice(b).unwrap());
    let mut overlay = Overlay::new(LevelMeter::default(), font).unwrap();
    let hwnd = overlay.hwnd();

    // SAFETY: reads a style of our live window.
    let ex = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    for (bit, name) in [
        (WS_EX_LAYERED, "LAYERED"),
        (WS_EX_TRANSPARENT, "TRANSPARENT"),
        (WS_EX_TOOLWINDOW, "TOOLWINDOW"),
        (WS_EX_TOPMOST, "TOPMOST"),
        (WS_EX_NOACTIVATE, "NOACTIVATE"),
    ] {
        assert_ne!(ex & bit, 0, "WS_EX_{name} missing");
    }
    assert_eq!(ex & WS_EX_APPWINDOW, 0, "APPWINDOW puts it in Alt+Tab");

    let mut cfg = Config::default();
    for position in [OverlayPosition::BottomRight, OverlayPosition::TopCentre] {
        cfg.overlay.position = position;
        // SAFETY: no arguments.
        let before = unsafe { GetForegroundWindow() };
        overlay.show(RECORDING, &cfg).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { GetForegroundWindow() }, before, "show took focus");
        assert!(visible(hwnd));

        let want = place(position, work_area(before).0, work_area(before).1);
        assert_eq!(window_rect(hwnd), want, "{position:?}");
        let centre = POINT {
            x: want.x + want.w / 2,
            y: want.y + want.h / 2,
        };
        // SAFETY: no pointers.
        let hit = unsafe { WindowFromPoint(centre) };
        assert_ne!(hit, hwnd, "the pill takes clicks");
    }

    // The pill reached the screen: its left end, past the edge and before the icon, is surface.
    let r = window_rect(hwnd);
    let (px, py) = ((PILL_PAD * r.h as f32 / 2.0) as u32, r.h.unsigned_abs() / 2);
    let end = Instant::now() + Duration::from_secs(2);
    let shot = loop {
        let shot = grab(r);
        let c = shot.pixel(px, py).unwrap();
        let near = |a: u8, b: u8| a.abs_diff(b) <= 2;
        if near(c.red(), SURFACE.0) && near(c.green(), SURFACE.1) && near(c.blue(), SURFACE.2) {
            break shot;
        }
        assert!(Instant::now() < end, "screen at the pill is {c:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    if let Some(dir) = std::env::var_os("OXI_SHOTS_DIR").map(PathBuf::from) {
        std::fs::create_dir_all(&dir).unwrap();
        shot.save_png(dir.join("overlay-recording.png")).unwrap();
    }

    assert!(
        timer_fires(hwnd, Duration::from_millis(500)),
        "no meter timer while recording"
    );
    overlay.show(OverlayState::Transcribing, &cfg).unwrap();
    while timer_fires(hwnd, Duration::ZERO) {}
    assert!(
        !timer_fires(hwnd, Duration::from_millis(200)),
        "meter timer outside recording"
    );
    assert!(visible(hwnd));

    overlay.hide();
    assert!(!visible(hwnd));
    overlay.show(RECORDING, &cfg).unwrap();
    overlay.show(OverlayState::Hidden, &cfg).unwrap();
    assert!(!visible(hwnd));
}

fn visible(hwnd: HWND) -> bool {
    // SAFETY: no pointers.
    unsafe { IsWindowVisible(hwnd) != 0 }
}

/// Work area and scale of `window`'s monitor, read the way the overlay must.
fn work_area(window: HWND) -> (Rect, f32) {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let (mut dpi, mut dpi_y) = (0, 0);
    // SAFETY: out pointers are live locals.
    unsafe {
        let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTOPRIMARY);
        assert_ne!(GetMonitorInfoW(monitor, &raw mut info), 0);
        assert_eq!(
            GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut dpi_y),
            0
        );
    }
    (rect(info.rcWork), dpi as f32 / 96.0)
}

fn window_rect(hwnd: HWND) -> Rect {
    let mut r = RECT::default();
    // SAFETY: `r` is a live local.
    assert_ne!(unsafe { GetWindowRect(hwnd, &raw mut r) }, 0);
    rect(r)
}

fn rect(r: RECT) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        w: r.right - r.left,
        h: r.bottom - r.top,
    }
}

/// Takes one `WM_TIMER` for `hwnd` within `within` and dispatches it (one meter redraw).
fn timer_fires(hwnd: HWND, within: Duration) -> bool {
    let end = Instant::now() + within;
    let mut msg = MSG::default();
    loop {
        // SAFETY: `msg` is a live local; the message is our window's.
        if unsafe { PeekMessageW(&raw mut msg, hwnd, WM_TIMER, WM_TIMER, PM_REMOVE) } != 0 {
            // SAFETY: as above.
            unsafe { DispatchMessageW(&raw const msg) };
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The screen inside `r`, opaque RGBA.
fn grab(r: Rect) -> Pixmap {
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: r.w,
            biHeight: -r.h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pm = Pixmap::new(r.w.unsigned_abs(), r.h.unsigned_abs()).unwrap();
    // SAFETY: every handle is made and freed here; `bits` holds w * h * 4 bytes while selected.
    unsafe {
        let screen = GetDC(null_mut());
        let mem = CreateCompatibleDC(screen);
        let mut bits = null_mut();
        let bmp = CreateDIBSection(
            mem,
            &raw const info,
            DIB_RGB_COLORS,
            &raw mut bits,
            null_mut(),
            0,
        );
        assert!(!bmp.is_null());
        let old = SelectObject(mem, bmp);
        assert_ne!(
            BitBlt(mem, 0, 0, r.w, r.h, screen, r.x, r.y, SRCCOPY | CAPTUREBLT),
            0
        );
        let src = std::slice::from_raw_parts(bits.cast::<u8>(), pm.data().len());
        for (d, s) in pm
            .data_mut()
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(src.as_chunks::<4>().0)
        {
            d.copy_from_slice(&[s[2], s[1], s[0], 255]);
        }
        SelectObject(mem, old);
        DeleteObject(bmp);
        DeleteDC(mem);
        ReleaseDC(null_mut(), screen);
    }
    pm
}
