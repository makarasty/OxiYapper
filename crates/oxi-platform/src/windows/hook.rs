//! `WH_KEYBOARD_LL` hook thread (ARCHITECTURE section 2, section 11 items 6-12).
//!
//! The thread owns its own `GetMessage` loop, so a busy coordinator never pushes the hook past
//! `LowLevelHooksTimeout`. The callback maps the key, asks the core [`Tracker`] and does what the
//! [`oxi_core::hotkey::Verdict`] says; every decision lives in the tracker.

use std::cell::RefCell;
use std::io;
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use oxi_core::hotkey::{Chord, Ctx, Key, KeyInput, Tracker};
use oxi_core::machine::{Event, Phase, PhaseCell};
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, PM_NOREMOVE,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WM_APP, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

use super::keys;

/// `dwExtraInfo` of every key `OxiYapper` injects ("OXIY"): the hook passes such keys untracked.
pub const INJECT_TAG: usize = 0x4F58_4959;

/// Posted to the hook thread when [`Command`]s wait in its channel.
const WM_HOOK_COMMAND: u32 = WM_APP;

enum Command {
    SetChord(Chord),
    SetCapture { on: bool, swallow: bool },
    Reinstall,
    Poll,
}

/// Everything the callback needs; lives in the hook thread's [`STATE`].
struct State {
    tracker: Tracker,
    phase: PhaseCell,
    capture_swallow: bool,
    on_event: Sender<Event>,
    on_captured: Sender<(Key, bool)>,
}

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

/// The global hotkey hook.
pub struct Hook;

impl Hook {
    /// Starts the hook thread and installs the hook. Machine events go to `on_event`, keys seen
    /// in capture mode to `on_captured`. Fails when Windows refuses the hook.
    pub fn spawn(
        chord: Chord,
        phase: PhaseCell,
        on_event: Sender<Event>,
        on_captured: Sender<(Key, bool)>,
    ) -> io::Result<HookHandle> {
        let state = State {
            tracker: Tracker::new(chord),
            phase,
            capture_swallow: false,
            on_event,
            on_captured,
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (commands, commands_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("hook".into())
            .spawn(move || run(state, &commands_rx, &ready_tx))?;
        let ready = ready_rx.recv();
        match ready {
            Ok(Ok(thread_id)) => Ok(HookHandle {
                thread_id,
                commands,
                thread: Some(thread),
            }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other("hook thread ended before installing"))
            }
        }
    }
}

/// Controls the running hook thread; dropping it unhooks and joins the thread.
pub struct HookHandle {
    thread_id: u32,
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl HookHandle {
    /// Replaces the chord (the coordinator calls it when the phase is Idle, item 11).
    pub fn set_chord(&self, chord: Chord) {
        self.send(Command::SetChord(chord));
    }

    /// Hotkey capture on or off; `swallow` while the settings window is foreground.
    pub fn set_capture(&self, on: bool, swallow: bool) {
        self.send(Command::SetCapture { on, swallow });
    }

    /// Installs the hook again (resume, session unlock: item 12) and forgets keys whose
    /// key-up was lost; a chord that is no longer held gives `ChordUp`.
    pub fn reinstall(&self) {
        self.send(Command::Reinstall);
    }

    /// Answers `Effect::PollChord` (item 8): forgets keys whose key-up was lost, then sends
    /// `ChordPolled` with the physical chord state.
    pub fn poll(&self) {
        self.send(Command::Poll);
    }

    /// Unhooks and joins the thread.
    pub fn stop(self) {
        drop(self);
    }

    fn send(&self, command: Command) {
        if self.commands.send(command).is_ok() {
            post(self.thread_id, WM_HOOK_COMMAND);
        }
    }
}

impl Drop for HookHandle {
    fn drop(&mut self) {
        post(self.thread_id, WM_QUIT);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn post(thread_id: u32, message: u32) {
    // SAFETY: posting a message without pointers to a thread id; a dead thread only fails.
    if unsafe { PostThreadMessageW(thread_id, message, 0, 0) } == 0 {
        tracing::warn!(error = %io::Error::last_os_error(), message, "hook thread message lost");
    }
}

fn run(state: State, commands: &Receiver<Command>, ready: &Sender<io::Result<u32>>) {
    let mut msg = MSG::default();
    // SAFETY: `msg` is a valid MSG; this call only creates the thread's message queue, so
    // PostThreadMessageW cannot fail once `ready` is sent.
    unsafe { PeekMessageW(&raw mut msg, ptr::null_mut(), 0, 0, PM_NOREMOVE) };
    STATE.set(Some(state));
    let mut hook = match install() {
        Ok(hook) => hook,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: no arguments, no preconditions.
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));
    // SAFETY: `msg` is a valid MSG; a null window takes every message of this thread.
    while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {
        if msg.message == WM_HOOK_COMMAND {
            for command in commands.try_iter() {
                apply(command, &mut hook);
            }
        }
    }
    uninstall(hook);
}

fn apply(command: Command, hook: &mut HHOOK) {
    if let Command::Reinstall = command {
        uninstall(*hook);
        *hook = install().unwrap_or_else(|e| {
            tracing::error!(error = %e, "hotkey hook reinstall failed");
            ptr::null_mut()
        });
    }
    STATE.with_borrow_mut(|state| {
        let Some(s) = state.as_mut() else { return };
        match command {
            Command::SetChord(chord) => s.tracker.set_chord(chord),
            Command::SetCapture { on, swallow } => {
                s.tracker.set_capture(on);
                s.capture_swallow = swallow;
            }
            Command::Reinstall => {
                if let Some(event) = s.tracker.resync(keys::id_held) {
                    let _ = s.on_event.send(event);
                }
            }
            Command::Poll => {
                // The poll answer says whether the chord is up, so resync's own ChordUp is dropped.
                let _ = s.tracker.resync(keys::id_held);
                let held = keys::chord_held_now(s.tracker.chord());
                let _ = s.on_event.send(Event::ChordPolled { held });
            }
        }
    });
}

fn install() -> io::Result<HHOOK> {
    // SAFETY: a null name asks for the exe's own module handle, valid for the process lifetime.
    let module = unsafe { GetModuleHandleW(ptr::null()) };
    // SAFETY: `keyboard_proc` has the HOOKPROC signature and lives as long as the process; a
    // low-level hook with thread id 0 is global and called on this thread's message loop.
    let hook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), module, 0) };
    if hook.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(hook)
    }
}

fn uninstall(hook: HHOOK) {
    if !hook.is_null() {
        // SAFETY: `hook` came from SetWindowsHookExW on this thread and is unhooked once.
        unsafe { UnhookWindowsHookEx(hook) };
    }
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        // SAFETY: for WH_KEYBOARD_LL with code >= 0, `lparam` points to a KBDLLHOOKSTRUCT that
        // is valid for the duration of this call.
        let event = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if on_key(event, wparam) {
            return 1;
        }
    }
    // SAFETY: the hook's own arguments, passed on unchanged.
    unsafe { CallNextHookEx(ptr::null_mut(), code, wparam, lparam) }
}

/// Runs one key event through the tracker; true means swallow it.
fn on_key(event: &KBDLLHOOKSTRUCT, wparam: WPARAM) -> bool {
    let input = KeyInput {
        // VK codes are below 256; anything else is an untracked Other.
        key: keys::key_id(
            u16::try_from(event.vkCode).unwrap_or(u16::MAX),
            event.flags & LLKHF_EXTENDED != 0,
        ),
        down: matches!(u32::try_from(wparam), Ok(WM_KEYDOWN | WM_SYSKEYDOWN)),
        ours: event.dwExtraInfo == INJECT_TAG,
    };
    STATE.with_borrow_mut(|state| {
        let Some(s) = state.as_mut() else {
            return false;
        };
        let ctx = Ctx {
            recording: s.phase.load() == Phase::Recording,
            capture_swallow: s.capture_swallow,
        };
        let verdict = s.tracker.on_key(input, ctx);
        if let Some(event) = verdict.event {
            let _ = s.on_event.send(event);
        }
        if let Some(captured) = verdict.captured {
            let _ = s.on_captured.send(captured);
        }
        if verdict.suppress_start
            && let Err(e) = keys::send_start_blocker()
        {
            tracing::warn!(error = %e, "Start menu blocker not sent");
        }
        verdict.swallow
    })
}
