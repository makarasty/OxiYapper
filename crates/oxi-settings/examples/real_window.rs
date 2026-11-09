//! The real settings window for `scripts/real-shot.ps1`: the same renderer selection as
//! `OxiYapper settings`, a throwaway data dir, a fake daemon link, placed off every screen.
//!
//! `real_window <page> <width> <height> <light|dark|system> <out.png> [tab]`: once the window has
//! settled it saves the renderer's own snapshot of the client area to `out.png`, prints
//! `real_window: shot <w>x<h>` and quits; after `DEADLINE` it quits on its own if that never happened. The snapshot
//! comes from the runtime renderer, not from the OS: `PrintWindow` of a window kept off every screen
//! came back flat white in about 4 runs of 10 (the frame is only there if it is read early).
//! The window is never activated, so it cannot take the owner's focus. The Audio page still opens
//! the default microphone for its level meter (`pages/audio.rs`, in memory, nothing is written).
//! `tab` opens a sub-tab by name; a name it does not know is an error, never a shot of another tab.

// No console: a console exe started by the script shares the caller's console and was killed
// from outside in about 1 run of 40 (exit 0xC000013A, or 0xC0000142 while attaching to it). The
// script's stderr redirect still works without one.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{env, fs, path::PathBuf, process, time::Duration};

use oxi_core::paths::Layout;
use oxi_settings::{
    AppWindow, AudioState, Ipc, ModelState, Page, SystemState, TestState, TextState, ThemeChoice,
};
use slint::{ComponentHandle, LogicalSize, PhysicalPosition, Timer, TimerMode};

/// Far outside any monitor, but inside the 16-bit range window coordinates keep.
const OFF_SCREEN: i32 = -20_000;

/// Longest this process lives; the script waits for far less.
const DEADLINE: Duration = Duration::from_secs(60);

/// First snapshot attempt, and the gap between attempts: the window is created at a default size
/// and the layout pass and animations settle a moment after the first frame.
const SETTLE: Duration = Duration::from_millis(700);
const RETRY: Duration = Duration::from_millis(200);

/// Win32: a window this thread is about to show gets `WS_EX_NOACTIVATE`. winit shows a new window
/// with `SW_SHOW`, which takes the foreground once the owner has been idle past the foreground
/// lock timeout; Slint has no switch for it. The style is set on `WM_SHOWWINDOW` because winit
/// rewrites the whole extended style before that, and cancelling the activation instead
/// (`HCBT_ACTIVATE`) leaves the owner with no foreground window at all.
#[cfg(windows)]
mod no_activate {
    use std::ffi::c_void;

    const WH_CALLWNDPROC: i32 = 4;
    const WM_SHOWWINDOW: u32 = 0x0018;
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_NOACTIVATE: i32 = 0x0800_0000;

    /// `CWPSTRUCT`.
    #[repr(C)]
    struct CallWndProc {
        l: isize,
        w: usize,
        message: u32,
        hwnd: *mut c_void,
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowsHookExW(
            id: i32,
            proc: unsafe extern "system" fn(i32, usize, isize) -> isize,
            module: *mut c_void,
            thread: u32,
        ) -> *mut c_void;
        fn CallNextHookEx(hook: *mut c_void, code: i32, w: usize, l: isize) -> isize;
        fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
        fn SetWindowLongW(hwnd: *mut c_void, index: i32, value: i32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }

    unsafe extern "system" fn on_message(code: i32, w: usize, l: isize) -> isize {
        // SAFETY: for `HC_ACTION` (0) `l` points at the system's live `CWPSTRUCT`; the style calls
        // take the window the message is for.
        unsafe {
            let msg = &*(l as *const CallWndProc);
            if code == 0 && msg.message == WM_SHOWWINDOW && msg.w != 0 {
                let ex = GetWindowLongW(msg.hwnd, GWL_EXSTYLE);
                SetWindowLongW(msg.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE);
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }

    /// Installs the hook for the calling thread, which must be the one that shows the window.
    pub fn install() -> Result<(), &'static str> {
        // SAFETY: `on_message` matches the HOOKPROC signature and lives for the whole process; a
        // thread hook needs no module handle.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_CALLWNDPROC,
                on_message,
                std::ptr::null_mut(),
                GetCurrentThreadId(),
            )
        };
        if hook.is_null() {
            Err("cannot install the no-activate hook")
        } else {
            Ok(())
        }
    }
}

fn page(name: &str) -> Option<Page> {
    Some(match name {
        "home" => Page::Home,
        "hotkey" => Page::Hotkey,
        "audio" => Page::Audio,
        "model" => Page::Model,
        "text" => Page::Text,
        "overlay" => Page::Overlay,
        "test" => Page::Test,
        "system" => Page::System,
        "about" => Page::About,
        _ => return None,
    })
}

fn open_tab(window: &AppWindow, page: Page, name: &str) -> Result<(), String> {
    let tabs: &[&str] = match page {
        Page::Test => &["record", "results"],
        Page::Audio => &["microphone", "recording", "start", "stop"],
        Page::Model => &["models", "settings", "advanced"],
        Page::Text => &["dictionary", "replacements", "paste", "rules"],
        Page::System => &["general", "recordings"],
        _ => &[],
    };
    let Some(view) = tabs.iter().position(|t| *t == name) else {
        return Err(format!("no sub-tab \"{name}\" on this page: {tabs:?}"));
    };
    let view = i32::try_from(view).map_err(|e| e.to_string())?;
    match page {
        Page::Test => window.global::<TestState>().set_view(view),
        Page::Audio => window.global::<AudioState>().set_view(view),
        Page::Model => window.global::<ModelState>().set_tab(view),
        Page::Text => window.global::<TextState>().set_tab(view),
        Page::System => window.global::<SystemState>().set_tab(view),
        _ => {}
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let usage = "usage: real_window <page> <width> <height> <light|dark|system> <out.png> [tab]";
    let (Some(name), Some(w), Some(h), Some(theme), Some(out)) = (
        args.next(),
        args.next(),
        args.next(),
        args.next(),
        args.next(),
    ) else {
        return Err(usage.into());
    };
    let (Some(page), Ok(w), Ok(h)) = (page(&name), w.parse::<f32>(), h.parse::<f32>()) else {
        return Err(usage.into());
    };
    let theme = match theme.as_str() {
        "light" => ThemeChoice::Light,
        "dark" => ThemeChoice::Dark,
        // Follows the OS scheme of this machine, as Appearance = System does.
        "system" => ThemeChoice::System,
        _ => return Err(usage.into()),
    };
    let out = PathBuf::from(out);
    let tab = args.next();

    #[cfg(windows)]
    no_activate::install()?;

    let dir = env::temp_dir().join(format!("oxi-real-shot-{}", process::id()));
    fs::create_dir_all(&dir)?;
    let layout = Layout {
        config_dir: dir.clone(),
        data_dir: dir.clone(),
        logs_dir: dir.clone(),
    };

    let skia = oxi_settings::use_skia();
    let (ipc, _daemon_side) = Ipc::fake();
    let window = oxi_settings::open_with(layout, ipc)?;
    // The window never grows past its maximum, so a larger shot would wait for a size it never gets.
    let (most_w, most_h) = (window.get_largest_width(), window.get_largest_height());
    if w > most_w || h > most_h {
        let _ = fs::remove_dir_all(&dir);
        return Err(format!("{w}x{h} is above the window's largest size {most_w}x{most_h}").into());
    }
    window.set_page(page);
    if let Some(tab) = &tab
        && let Err(e) = open_tab(&window, page, tab)
    {
        let _ = fs::remove_dir_all(&dir);
        return Err(e.into());
    }
    window.set_theme(theme);
    window.window().set_size(LogicalSize::new(w, h));
    window
        .window()
        .set_position(PhysicalPosition::new(OFF_SCREEN, OFF_SCREEN));
    eprintln!(
        "real_window: renderer {}",
        if skia { "skia-software" } else { "software" }
    );
    // Like `run`: the backend writes the system accent when it shows the window, so put ours back
    // before the first frame can be captured.
    let weak = window.as_weak();
    slint::invoke_from_event_loop(move || {
        if let Some(window) = weak.upgrade() {
            oxi_settings::own_accent(&window);
            oxi_settings::caption_theme(&window, window.get_theme());
        }
    })?;
    Timer::single_shot(DEADLINE, || {
        let _ = slint::quit_event_loop();
    });
    let shooter = Timer::default();
    let weak = window.as_weak();
    let want = (w, h);
    shooter.start(TimerMode::Repeated, RETRY, {
        let started = std::time::Instant::now();
        move || {
            if started.elapsed() < SETTLE {
                return;
            }
            let Some(window) = weak.upgrade() else { return };
            let Ok(shot) = window.window().take_snapshot() else {
                return;
            };
            let scale = window.window().scale_factor();
            // The window is still at its default size until the resize lands.
            let off = |got: u32, want: f32| (f64::from(got) - f64::from(want * scale)).abs() > 1.0;
            if off(shot.width(), want.0) || off(shot.height(), want.1) {
                return;
            }
            let size = tiny_skia::IntSize::from_wh(shot.width(), shot.height());
            let Some(pixmap) =
                size.and_then(|size| tiny_skia::Pixmap::from_vec(shot.as_bytes().to_vec(), size))
            else {
                return;
            };
            if let Err(e) = pixmap.save_png(&out) {
                eprintln!("real_window: cannot save {}: {e}", out.display());
                process::exit(2);
            }
            eprintln!("real_window: shot {}x{}", shot.width(), shot.height());
            let _ = slint::quit_event_loop();
        }
    });
    let result = window.run();
    let _ = fs::remove_dir_all(&dir);
    Ok(result?)
}
