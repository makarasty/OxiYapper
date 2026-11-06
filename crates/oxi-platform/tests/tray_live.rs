//! Live tray tests on the real Explorer notification area; they run only with
//! `OXI_TRAY_TEST=1`. Every icon a test adds is removed by `MainLoop`'s Drop, also on panic.
#![cfg(windows)]

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oxi_core::machine::{Event, Phase};
use oxi_platform::os::tray::{MainLoop, Waker};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Shell::{
    NIM_DELETE, NIM_MODIFY, NOTIFY_ICON_MESSAGE, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, MSG, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PM_REMOVE, PeekMessageW,
    PostMessageW, RegisterWindowMessageW, SendMessageW, WM_POWERBROADCAST, WM_QUIT,
    WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

type Events = Arc<Mutex<Vec<Event>>>;

fn live() -> bool {
    std::env::var_os("OXI_TRAY_TEST").is_some_and(|v| v == "1")
}

fn wparam(v: u32) -> usize {
    usize::try_from(v).expect("wparam")
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

/// Runs the loop while `poke` runs on another thread with the loop window, a waker and the
/// session events so far; `poke` must end the loop with [`Waker::quit`]. Returns the wake
/// count, the session events and what `poke` returned. A watchdog ends a stuck loop and fails.
fn run_with<R: Send + 'static>(
    ml: &mut MainLoop,
    poke: impl FnOnce(HWND, &Waker, &Events) -> R + Send + 'static,
) -> (u32, Vec<Event>, R) {
    let hwnd = ml.hwnd().expose_provenance();
    let waker = ml.waker();
    let events = Events::default();
    let seen = Arc::clone(&events);
    let (done, finished) = mpsc::channel::<()>();
    let poker = std::thread::spawn(move || {
        let hwnd: HWND = std::ptr::with_exposed_provenance_mut(hwnd);
        let out = poke(hwnd, &waker, &seen);
        let stuck =
            finished.recv_timeout(Duration::from_secs(10)) == Err(RecvTimeoutError::Timeout);
        if stuck {
            // SAFETY: posting to our window; WM_QUIT ends GetMessageW on the loop thread.
            unsafe { PostMessageW(hwnd, WM_QUIT, 0, 0) };
        }
        (out, stuck)
    });
    let mut woken = 0;
    let sink = Arc::clone(&events);
    ml.run(
        |_| woken += 1,
        |_, item| panic!("no menu expected, got {item:?}"),
        move |e| sink.lock().expect("events").push(e),
    );
    drop(done);
    let (out, stuck) = poker.join().expect("poke thread");
    assert!(!stuck, "the loop did not end; the watchdog quit it");
    let events = events.lock().expect("events").clone();
    (woken, events, out)
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
fn waker_from_another_thread_reaches_on_wake_and_quit_ends_run() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let (woken, events, ()) = run_with(&mut ml, |_, waker, _| {
        waker.wake().expect("wake");
        waker.quit().expect("quit");
    });
    assert_eq!((woken, events), (1, Vec::new()));
}

#[test]
fn quit_from_another_thread_alone_ends_run() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let (woken, _, ()) = run_with(&mut ml, |_, waker, _| waker.quit().expect("quit"));
    assert_eq!(woken, 0);
}

#[test]
fn posted_power_and_session_messages_give_machine_events() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let messages = [
        (WM_POWERBROADCAST, wparam(PBT_APMSUSPEND)),
        (WM_POWERBROADCAST, wparam(PBT_APMRESUMEAUTOMATIC)),
        (WM_WTSSESSION_CHANGE, wparam(WTS_SESSION_LOCK)),
        (WM_WTSSESSION_CHANGE, wparam(WTS_SESSION_UNLOCK)),
        // Not a session event: ignored.
        (WM_WTSSESSION_CHANGE, 1),
    ];
    let (_, events, ()) = run_with(&mut ml, move |hwnd, waker, _| {
        for (msg, wp) in messages {
            // SAFETY: posting to our live window.
            let posted = unsafe { PostMessageW(hwnd, msg, wp, 0) };
            assert_ne!(posted, 0, "post {msg}");
        }
        waker.quit().expect("quit");
    });
    assert_eq!(
        events,
        [Event::Suspend, Event::Resume, Event::Lock, Event::Unlock]
    );
}

/// Windows sends (not posts) `PBT_APMSUSPEND` and `TaskbarCreated`. No wake follows them.
#[test]
fn sent_messages_are_handled_without_a_wake() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    // Explorer restarting loses every icon; then it broadcasts TaskbarCreated.
    assert!(shell(ml.hwnd(), NIM_DELETE), "icon removed");
    let (woken, events, (at_reply, back)) = run_with(&mut ml, |hwnd, waker, seen| {
        // SAFETY: SendMessageW returns once the loop thread's window procedure replied.
        unsafe { SendMessageW(hwnd, WM_POWERBROADCAST, wparam(PBT_APMSUSPEND), 0) };
        let at_reply = seen.lock().expect("events").clone();
        let name: Vec<u16> = "TaskbarCreated".encode_utf16().chain([0]).collect();
        // SAFETY: a NUL-terminated string that outlives the call; then a send to our window.
        unsafe { SendMessageW(hwnd, RegisterWindowMessageW(name.as_ptr()), 0, 0) };
        let deadline = Instant::now() + Duration::from_millis(500);
        let back = loop {
            if shell(hwnd, NIM_MODIFY) {
                break true;
            }
            if Instant::now() > deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        waker.quit().expect("quit");
        (at_reply, back)
    });
    assert_eq!(
        at_reply,
        [Event::Suspend],
        "Suspend delivered before the reply"
    );
    assert!(back, "icon re-added within 500 ms");
    assert_eq!((woken, events), (0, vec![Event::Suspend]));
}

#[test]
fn waker_of_a_dropped_loop_posts_nothing() {
    if !live() {
        return;
    }
    let ml = MainLoop::new().expect("tray window");
    let waker = ml.waker();
    drop(ml);
    for result in [waker.wake(), waker.quit()] {
        assert_eq!(
            result.map_err(|e| e.kind()),
            Err(io::ErrorKind::NotConnected)
        );
    }
}

/// Sends a power or session message to `hwnd`; returns once its window procedure replied.
fn send(hwnd: HWND, msg: u32, wp: u32) {
    // SAFETY: a send to a window handle; Windows checks it.
    unsafe { SendMessageW(hwnd, msg, wparam(wp), 0) };
}

/// Dispatches every queued message of this thread, as a modal loop (menu, message box) does.
fn pump() {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid out-pointer, filled before it is dispatched.
    while unsafe { PeekMessageW(&raw mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        // SAFETY: as above.
        unsafe { DispatchMessageW(&raw const msg) };
    }
}

/// A Suspend seen before `run` is not overtaken by a Resume sent once `run` pumps.
#[test]
fn session_events_seen_before_run_keep_their_order() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let hwnd = ml.hwnd();
    send(hwnd, WM_POWERBROADCAST, PBT_APMSUSPEND);
    let addr = hwnd.expose_provenance();
    let waker = ml.waker();
    let (sending, about_to_send) = mpsc::channel();
    let poke = std::thread::spawn(move || {
        sending.send(()).expect("signal");
        send(
            std::ptr::with_exposed_provenance_mut(addr),
            WM_POWERBROADCAST,
            PBT_APMRESUMEAUTOMATIC,
        );
        waker.quit().expect("quit");
    });
    about_to_send.recv().expect("signal");
    // Setup only: lets the Resume be pending when `run` first pumps, the order that broke.
    std::thread::sleep(Duration::from_millis(50));
    let events = Events::default();
    let sink = Arc::clone(&events);
    ml.run(
        |_| {},
        |_, _| {},
        move |e| sink.lock().expect("events").push(e),
    );
    poke.join().expect("poke thread");
    assert_eq!(
        *events.lock().expect("events"),
        [Event::Suspend, Event::Resume]
    );
}

/// A session event sent from inside `on_session` comes after the one being handled.
#[test]
fn session_event_sent_from_on_session_comes_next() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let addr = ml.hwnd().expose_provenance();
    let waker = ml.waker();
    let events = Events::default();
    let sink = Arc::clone(&events);
    ml.waker().wake().expect("wake");
    ml.run(
        |ml| send(ml.hwnd(), WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK),
        |_, _| {},
        move |e| {
            sink.lock().expect("events").push(e);
            if e == Event::Lock {
                send(
                    std::ptr::with_exposed_provenance_mut(addr),
                    WM_WTSSESSION_CHANGE,
                    WTS_SESSION_UNLOCK,
                );
                waker.quit().expect("quit");
            }
        },
    );
    assert_eq!(
        *events.lock().expect("events"),
        [Event::Lock, Event::Unlock]
    );
}

/// A wake left behind by quit is not replayed by a later `run`.
#[test]
fn quit_drops_the_wakes_behind_it() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let waker = ml.waker();
    waker.wake().expect("wake");
    let mut first = 0;
    ml.run(
        |ml| {
            first += 1;
            let w = ml.waker();
            w.quit().expect("quit");
            w.wake().expect("wake");
            pump();
        },
        |_, _| {},
        |_| {},
    );
    assert_eq!(first, 1);
    let (woken, _, ()) = run_with(&mut ml, |_, waker, _| waker.quit().expect("quit"));
    assert_eq!(woken, 0, "no wake replayed");
}

/// A panic in `on_session` leaves `run` as a panic, so Drop still removes the icon.
#[test]
fn panic_in_on_session_unwinds_out_of_run() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let hwnd = ml.hwnd();
    let addr = hwnd.expose_provenance();
    let waker = ml.waker();
    let poke = std::thread::spawn(move || {
        send(
            std::ptr::with_exposed_provenance_mut(addr),
            WM_WTSSESSION_CHANGE,
            WTS_SESSION_LOCK,
        );
        // Ends the loop if the panic was swallowed.
        let _ = waker.quit();
    });
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ml.run(|_| {}, |_, _| {}, |_| panic!("on_session test panic"));
    }));
    poke.join().expect("poke thread");
    assert!(ran.is_err(), "run must rethrow the panic");
    drop(ml);
    assert!(!shell(hwnd, NIM_MODIFY), "icon gone after drop");
}

/// A later `run` shows the icon even when the `TaskbarCreated` that would re-add it was dropped.
#[test]
fn run_after_quit_with_taskbar_created_queued_shows_the_icon() {
    if !live() {
        return;
    }
    let mut ml = MainLoop::new().expect("tray window");
    let hwnd = ml.hwnd();
    ml.waker().wake().expect("wake");
    ml.run(
        |ml| {
            assert!(shell(ml.hwnd(), NIM_DELETE), "icon removed");
            ml.waker().quit().expect("quit");
            pump();
            let name: Vec<u16> = "TaskbarCreated".encode_utf16().chain([0]).collect();
            // SAFETY: a NUL-terminated string that outlives the call; then a send to our window.
            unsafe { SendMessageW(ml.hwnd(), RegisterWindowMessageW(name.as_ptr()), 0, 0) };
        },
        |_, _| {},
        |_| {},
    );
    assert!(!shell(hwnd, NIM_MODIFY), "the quit dropped the re-add");
    let (woken, _, ()) = run_with(&mut ml, |_, waker, _| waker.quit().expect("quit"));
    assert_eq!(woken, 0);
    assert!(shell(hwnd, NIM_MODIFY), "icon back after the second run");
}
