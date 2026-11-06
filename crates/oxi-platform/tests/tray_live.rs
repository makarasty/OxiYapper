//! Live tray tests on the real Explorer notification area; they run only with
//! `OXI_TRAY_TEST=1`. Every icon a test adds is removed by `MainLoop`'s Drop, also on panic.
#![cfg(windows)]

use std::sync::mpsc;
use std::time::Duration;

use oxi_core::machine::{Event, Phase};
use oxi_platform::os::tray::MainLoop;
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    NIM_DELETE, NIM_MODIFY, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, RegisterWindowMessageW,
    WM_POWERBROADCAST, WM_QUIT, WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

fn live() -> bool {
    std::env::var_os("OXI_TRAY_TEST").is_some_and(|v| v == "1")
}

/// Sends `message` for the loop's icon (id 1); true when the shell accepted it.
fn shell(hwnd: HWND, message: NOTIFY_ICON_MESSAGE) -> bool {
    let data = NOTIFYICONDATAW {
        cbSize: u32::try_from(size_of::<NOTIFYICONDATAW>()).expect("struct size"),
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    // SAFETY: an initialised struct with its own cbSize; no flags, so nothing else is read.
    unsafe { Shell_NotifyIconW(message, &raw const data) != 0 }
}

/// Posts `messages` to the loop window from another thread, then wakes it; `on_wake` quits.
/// Returns the wake count and the session events seen. A watchdog ends a stuck loop.
fn run_after(ml: &mut MainLoop, messages: Vec<(u32, usize)>) -> (u32, Vec<Event>) {
    let hwnd = ml.hwnd().expose_provenance();
    let wake = ml.waker();
    let (done, finished) = mpsc::channel::<()>();
    let poster = std::thread::spawn(move || {
        let hwnd: HWND = std::ptr::with_exposed_provenance_mut(hwnd);
        for (msg, wparam) in messages {
            // SAFETY: posting to a window handle; fails harmlessly if it is gone.
            let posted = unsafe { PostMessageW(hwnd, msg, wparam, 0) };
            assert_ne!(posted, 0, "post {msg}");
        }
        wake.wake().expect("wake");
        if finished.recv_timeout(Duration::from_secs(10)).is_err() {
            // SAFETY: as above; WM_QUIT ends GetMessageW on the loop thread.
            unsafe { PostMessageW(hwnd, WM_QUIT, 0, 0) };
        }
    });
    let (mut woken, mut events) = (0, Vec::new());
    ml.run(
        |_| {
            woken += 1;
            MainLoop::quit();
        },
        |_, item| panic!("no menu expected, got {item:?}"),
        |e| events.push(e),
    );
    drop(done);
    poster.join().expect("poster thread");
    (woken, events)
}

#[test]
fn icon_is_added_set_per_phase_and_removed() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let hwnd = ml.hwnd();
    assert!(shell(hwnd, NIM_MODIFY), "icon present after new");
    for phase in [
        Phase::Recording,
        Phase::Transcribing,
        Phase::Pasting,
        Phase::Idle,
    ] {
        ml.set_phase(phase).expect("set phase");
    }
    drop(ml);
    assert!(!shell(hwnd, NIM_MODIFY), "icon gone after drop");
}

#[test]
fn waker_from_another_thread_reaches_on_wake() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    assert_eq!(run_after(&mut ml, Vec::new()), (1, Vec::new()));
}

#[test]
fn power_and_session_messages_give_machine_events() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let to_usize = |v: u32| usize::try_from(v).expect("wparam");
    let messages = vec![
        (WM_POWERBROADCAST, to_usize(PBT_APMSUSPEND)),
        (WM_POWERBROADCAST, to_usize(PBT_APMRESUMEAUTOMATIC)),
        (WM_WTSSESSION_CHANGE, to_usize(WTS_SESSION_LOCK)),
        (WM_WTSSESSION_CHANGE, to_usize(WTS_SESSION_UNLOCK)),
        // Not a session event: ignored, and the hidden window survives it.
        (WM_WTSSESSION_CHANGE, 1),
    ];
    let (wakes, events) = run_after(&mut ml, messages);
    assert_eq!(wakes, 1);
    assert_eq!(
        events,
        [Event::Suspend, Event::Resume, Event::Lock, Event::Unlock]
    );
}

#[test]
fn taskbar_created_adds_the_icon_again() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let hwnd = ml.hwnd();
    // Explorer restarting loses every icon; then it broadcasts TaskbarCreated.
    assert!(shell(hwnd, NIM_DELETE), "icon removed");
    let name: Vec<u16> = "TaskbarCreated".encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated string that outlives the call.
    let taskbar_created = unsafe { RegisterWindowMessageW(name.as_ptr()) };
    assert_eq!(
        run_after(&mut ml, vec![(taskbar_created, 0)]),
        (1, Vec::new())
    );
    assert!(shell(hwnd, NIM_MODIFY), "icon back");
}
