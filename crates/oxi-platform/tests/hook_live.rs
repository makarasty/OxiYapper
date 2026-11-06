//! Live `WH_KEYBOARD_LL` hook. Runs only with `OXI_HOOK_TEST=1`, one test at a time
//! (`--test-threads=1`): every test injects real keys into the session.
//!
//! Chords are F13-based; Ctrl+Win is never injected (the owner's dictation tool listens on it).
#![cfg(windows)]

use std::ptr;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use oxi_core::hotkey::{Chord, Key};
use oxi_core::machine::{Event, Phase, PhaseCell};
use oxi_platform::os::hook::{Hook, HookHandle};
use oxi_platform::os::keys;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, SendInput,
    VK_ESCAPE, VK_F14,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_KEYDOWN,
    WM_QUIT, WM_SYSKEYDOWN,
};

const WAIT: Duration = Duration::from_secs(2);

fn hook_test() -> bool {
    let on = std::env::var_os("OXI_HOOK_TEST").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set OXI_HOOK_TEST=1");
    }
    on
}

struct Live {
    handle: HookHandle,
    phase: PhaseCell,
    events: Receiver<Event>,
}

fn spawn() -> Live {
    let chord = Chord::parse(&["Ctrl".into(), "Shift".into(), "F13".into()]).unwrap();
    let phase = PhaseCell::default();
    let (tx, events) = mpsc::channel();
    let (captured, _) = mpsc::channel();
    let handle = Hook::spawn(chord, phase.clone(), tx, captured).unwrap();
    Live {
        handle,
        phase,
        events,
    }
}

impl Live {
    fn next(&self) -> Event {
        self.events.recv_timeout(WAIT).expect("hook event")
    }
}

/// Injects keys as another program would: without our tag.
fn press_untagged(keys: &[(Key, bool)]) {
    let inputs: Vec<INPUT> = keys
        .iter()
        .map(|&(key, down)| {
            let (code, extended) = keys::vk(key);
            let mut flags = if extended { KEYEVENTF_EXTENDEDKEY } else { 0 };
            if !down {
                flags |= KEYEVENTF_KEYUP;
            }
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: code,
                        wScan: 0,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            }
        })
        .collect();
    let count = u32::try_from(inputs.len()).unwrap();
    let size = i32::try_from(size_of::<INPUT>()).unwrap();
    // SAFETY: a live slice of `count` initialised INPUT values.
    let sent = unsafe { SendInput(count, inputs.as_ptr(), size) };
    assert_eq!(sent, count, "SendInput blocked");
}

/// Ctrl+Shift+F13 with two F13 auto-repeats and an F14 while held.
const CHORD_WITH_REPEATS: [(Key, bool); 10] = [
    (Key::LCtrl, true),
    (Key::LShift, true),
    (Key::F13, true),
    (Key::F13, true),
    (Key::F13, true),
    (Key::F14, true),
    (Key::F14, false),
    (Key::F13, false),
    (Key::LShift, false),
    (Key::LCtrl, false),
];

const ESC: [(Key, bool); 2] = [(Key::Esc, true), (Key::Esc, false)];

#[test]
fn chord_gives_one_down_and_one_up_in_every_phase() {
    if !hook_test() {
        return;
    }
    let live = spawn();
    for phase in [
        Phase::Idle,
        Phase::Recording,
        Phase::Transcribing,
        Phase::Pasting,
    ] {
        live.phase.store(phase);
        press_untagged(&CHORD_WITH_REPEATS);
        // A repeat that counted as a fresh ChordDown would arrive before OtherKey.
        assert_eq!(live.next(), Event::ChordDown, "{phase:?}");
        assert_eq!(live.next(), Event::OtherKey, "{phase:?}");
        assert_eq!(live.next(), Event::ChordUp, "{phase:?}");
    }
}

#[test]
fn keys_tagged_ours_are_ignored() {
    if !hook_test() {
        return;
    }
    let live = spawn();
    live.phase.store(Phase::Recording);
    keys::send_keys(&CHORD_WITH_REPEATS).unwrap();
    keys::send_keys(&ESC).unwrap();
    // Untagged Esc is the marker: anything the tagged keys caused would arrive first.
    press_untagged(&ESC);
    assert_eq!(live.next(), Event::Esc);
}

#[test]
fn reinstall_keeps_working() {
    if !hook_test() {
        return;
    }
    let live = spawn();
    live.handle.reinstall();
    live.handle.reinstall();
    // Commands run in order: the poll answer means both reinstalls are done.
    live.handle.poll();
    assert_eq!(live.next(), Event::ChordPolled { held: false });
    press_untagged(&CHORD_WITH_REPEATS);
    assert_eq!(live.next(), Event::ChordDown);
    assert_eq!(live.next(), Event::OtherKey);
    assert_eq!(live.next(), Event::ChordUp);
}

/// VK codes of key-downs the observer hook saw.
static OBSERVED: Mutex<Option<Sender<u32>>> = Mutex::new(None);

unsafe extern "system" fn observer_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && matches!(u32::try_from(wparam), Ok(WM_KEYDOWN | WM_SYSKEYDOWN)) {
        // SAFETY: WH_KEYBOARD_LL with code >= 0 passes a valid KBDLLHOOKSTRUCT.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if let Some(tx) = OBSERVED.lock().unwrap().as_ref() {
            let _ = tx.send(event.vkCode);
        }
    }
    // SAFETY: the hook's own arguments, passed on unchanged.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

/// A second low-level hook installed before ours, so Windows calls it after ours.
fn observer() -> (u32, JoinHandle<()>) {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::spawn(move || {
        // SAFETY: own module handle; `observer_proc` lives for the process.
        let hook = unsafe {
            SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(observer_proc),
                GetModuleHandleW(ptr::null()),
                0,
            )
        };
        assert!(!hook.is_null(), "observer hook");
        let mut msg = MSG::default();
        // SAFETY: valid MSG; creates the queue so the caller's WM_QUIT cannot be lost.
        unsafe { PeekMessageW(&raw mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE) };
        // SAFETY: no arguments, no preconditions.
        ready_tx.send(unsafe { GetCurrentThreadId() }).unwrap();
        // SAFETY: valid MSG, null window takes every message of this thread.
        while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {}
        // SAFETY: installed above on this thread.
        unsafe { UnhookWindowsHookEx(hook) };
    });
    (ready_rx.recv().unwrap(), thread)
}

#[test]
fn observer_never_sees_esc_swallowed_while_recording() {
    if !hook_test() {
        return;
    }
    let (tx, observed) = mpsc::channel();
    *OBSERVED.lock().unwrap() = Some(tx);
    let (observer_id, observer_thread) = observer();
    let live = spawn();
    live.phase.store(Phase::Recording);

    press_untagged(&[ESC[0], ESC[1], (Key::F14, true), (Key::F14, false)]);
    assert_eq!(live.next(), Event::Esc);
    // F14 passes through, so the observer seeing it proves it saw everything before it.
    let mut seen = Vec::new();
    while seen.last() != Some(&u32::from(VK_F14)) {
        seen.push(observed.recv_timeout(WAIT).expect("observer saw F14"));
    }
    assert!(
        !seen.contains(&u32::from(VK_ESCAPE)),
        "observer saw Esc: {seen:?}"
    );

    *OBSERVED.lock().unwrap() = None;
    // SAFETY: plain message to the observer thread.
    unsafe { PostThreadMessageW(observer_id, WM_QUIT, 0, 0) };
    observer_thread.join().unwrap();
}
