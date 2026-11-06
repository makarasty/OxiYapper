//! Live `SendInput`. Runs only with `OXI_INPUT_TEST=1` and `--test-threads=1`.
//!
//! Safety for the desktop it shares: a test hook installed after every other low-level hook
//! sees our tagged events first and swallows them, and each injection happens only while a
//! window this test created is the foreground window. The previous foreground comes back on drop.
#![cfg(windows)]

use std::ptr::null_mut;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use oxi_core::hotkey::Key;
use oxi_core::paste::Input;
use oxi_core::types::Millis;
use oxi_platform::os::hook::INJECT_TAG;
use oxi_platform::os::input::Keyboard;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_LCONTROL, VK_PACKET, VK_V};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, CreateWindowExW, DestroyWindow, DispatchMessageW, GetForegroundWindow,
    GetMessageW, IsWindow, KBDLLHOOKSTRUCT, LLKHF_UP, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
    PostThreadMessageW, SetForegroundWindow, SetWindowsHookExW, UnhookWindowsHookEx,
    WH_KEYBOARD_LL, WM_QUIT, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

/// A tagged event the test hook swallowed: VK code, scan code (the unit for `VK_PACKET`), down.
type Seen = (u32, u32, bool);

static SEEN: Mutex<Option<Sender<Seen>>> = Mutex::new(None);

fn enabled() -> bool {
    let on = std::env::var_os("OXI_INPUT_TEST").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set OXI_INPUT_TEST=1");
    }
    on
}

#[test]
fn type_units_sends_each_unit_as_a_packet_pair_in_order() {
    if !enabled() {
        return;
    }
    let (spy, seen) = Spy::start();
    let window = OurWindow::foreground();
    window.assert_foreground();
    Keyboard
        .type_units(&"a😀".encode_utf16().collect::<Vec<_>>(), Millis(5))
        .unwrap();
    let want: Vec<Seen> = [0x61, 0xD83D, 0xDE00]
        .into_iter()
        .flat_map(|unit| {
            [
                (VK_PACKET.into(), unit, true),
                (VK_PACKET.into(), unit, false),
            ]
        })
        .collect();
    assert_eq!(take(&seen, want.len()), want);
    drop(window);
    drop(spy);
}

#[test]
fn ctrl_v_goes_ctrl_down_v_down_v_up_ctrl_up() {
    if !enabled() {
        return;
    }
    let (spy, seen) = Spy::start();
    let window = OurWindow::foreground();
    window.assert_foreground();
    Keyboard.send_chord(&[Key::LCtrl, Key::V]).unwrap();
    let (ctrl, v) = (u32::from(VK_LCONTROL), u32::from(VK_V));
    let got: Vec<(u32, bool)> = take(&seen, 4)
        .into_iter()
        .map(|(vk, _, down)| (vk, down))
        .collect();
    assert_eq!(got, [(ctrl, true), (v, true), (v, false), (ctrl, false)]);
    drop(window);
    drop(spy);
}

#[test]
fn foreground_is_our_exe_and_not_elevated() {
    if !enabled() {
        return;
    }
    let window = OurWindow::foreground();
    window.assert_foreground();
    let fg = Keyboard.foreground();
    let exe = std::env::current_exe().unwrap();
    let ours = exe.file_name().unwrap().to_str().unwrap();
    assert_ne!(fg.app, "");
    assert!(fg.app.eq_ignore_ascii_case(ours), "{} vs {ours}", fg.app);
    assert!(!fg.elevated);
}

/// The next `n` swallowed events, or a panic after 2 s.
fn take(seen: &Receiver<Seen>, n: usize) -> Vec<Seen> {
    (0..n)
        .map(|i| {
            seen.recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| panic!("event {i} of {n} not seen"))
        })
        .collect()
}

/// A low-level keyboard hook on its own thread that records and swallows our tagged events.
struct Spy {
    thread_id: u32,
    thread: Option<JoinHandle<()>>,
}

impl Spy {
    fn start() -> (Self, Receiver<Seen>) {
        let (tx, rx) = mpsc::channel();
        *SEEN.lock().unwrap() = Some(tx);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut msg = MSG::default();
            // SAFETY: creates this thread's queue so WM_QUIT can be posted to it.
            unsafe { PeekMessageW(&raw mut msg, null_mut(), 0, 0, PM_NOREMOVE) };
            // SAFETY: the exe's own module; `spy_proc` has the HOOKPROC signature and lives as
            // long as the process.
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(spy_proc),
                    GetModuleHandleW(null_mut()),
                    0,
                )
            };
            assert!(!hook.is_null(), "test hook not installed");
            // SAFETY: no arguments.
            ready_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
            // SAFETY: `msg` is a live local; the loop runs the hook callbacks.
            while unsafe { GetMessageW(&raw mut msg, null_mut(), 0, 0) } > 0 {}
            // SAFETY: installed on this thread above, unhooked once.
            unsafe { UnhookWindowsHookEx(hook) };
        });
        let thread_id = ready_rx.recv().expect("test hook thread died");
        let spy = Self {
            thread_id,
            thread: Some(thread),
        };
        (spy, rx)
    }
}

impl Drop for Spy {
    fn drop(&mut self) {
        // SAFETY: a pointer-free message to our own thread.
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        *SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

unsafe extern "system" fn spy_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL with code >= 0, `lparam` points to a live KBDLLHOOKSTRUCT.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if event.dwExtraInfo == INJECT_TAG {
            let seen = (event.vkCode, event.scanCode, event.flags & LLKHF_UP == 0);
            if let Ok(guard) = SEEN.lock()
                && let Some(tx) = guard.as_ref()
            {
                let _ = tx.send(seen);
            }
            return 1;
        }
    }
    // SAFETY: the hook's own arguments, passed on unchanged.
    unsafe { CallNextHookEx(null_mut(), code, wparam, lparam) }
}

/// A plain window of ours made the foreground; drop gives the foreground back.
struct OurWindow {
    hwnd: HWND,
    previous: HWND,
}

impl OurWindow {
    fn foreground() -> Self {
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        let title: Vec<u16> = "oxi input test\0".encode_utf16().collect();
        // SAFETY: no arguments.
        let previous = unsafe { GetForegroundWindow() };
        // SAFETY: a predefined class and null-terminated strings that outlive the call.
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                100,
                100,
                300,
                120,
                null_mut(),
                null_mut(),
                GetModuleHandleW(null_mut()),
                null_mut(),
            )
        };
        assert!(!hwnd.is_null(), "test window not created");
        let window = Self { hwnd, previous };
        // SAFETY: our live window.
        unsafe { SetForegroundWindow(hwnd) };
        window
    }

    /// Fails the test, before anything is injected, when the foreground is not our window.
    fn assert_foreground(&self) {
        pump();
        // SAFETY: no arguments.
        let now = unsafe { GetForegroundWindow() };
        assert_eq!(
            now, self.hwnd,
            "foreground is not the test window; nothing injected"
        );
    }
}

impl Drop for OurWindow {
    fn drop(&mut self) {
        // SAFETY: `previous` is checked live first; our window is destroyed once.
        unsafe {
            if !self.previous.is_null() && IsWindow(self.previous) != 0 {
                SetForegroundWindow(self.previous);
            }
            DestroyWindow(self.hwnd);
        }
    }
}

/// Handles the activation messages our window got.
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a live local; only this thread's messages are taken.
    while unsafe { PeekMessageW(&raw mut msg, null_mut(), 0, 0, PM_REMOVE) } != 0 {
        // SAFETY: as above.
        unsafe { DispatchMessageW(&raw const msg) };
    }
}
