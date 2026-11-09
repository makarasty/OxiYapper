//! Real Windows clipboard. Runs only with `OXI_CLIPBOARD_TEST=1`. Each test first saves the
//! clipboard it found and puts it back on drop. Every write here carries the exclusion markers,
//! so a run adds nothing to the user's Win+V history; the history-on markers are unit-tested.
#![cfg(windows)]

use std::ptr::{null, null_mut};
use std::sync::mpsc;
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use oxi_core::paste::{ClipError, Clipboard, Snap};
use oxi_platform::os::clipboard::{EXCLUDE, Saved, WinClipboard};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::Graphics::Gdi::{CloseEnhMetaFile, CreateEnhMetaFileW};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{CF_ENHMETAFILE, CF_UNICODETEXT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, MSG, PM_REMOVE, PeekMessageW,
};
use windows_sys::w;

fn clipboard_test() -> bool {
    let on = std::env::var_os("OXI_CLIPBOARD_TEST").is_some_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set OXI_CLIPBOARD_TEST=1");
    }
    on
}

/// The user's clipboard, put back on drop (also when a test panics).
struct UserClipboard(Option<Saved>);

impl UserClipboard {
    fn save() -> Self {
        match WinClipboard::new().unwrap().snapshot() {
            Snap::Complete(saved) => Self(Some(saved)),
            other => panic!(
                "the current clipboard cannot be saved ({other:?}); copy some plain text and rerun"
            ),
        }
    }
}

impl Drop for UserClipboard {
    fn drop(&mut self) {
        if let Some(saved) = self.0.take() {
            let put_back = WinClipboard::new().and_then(|mut c| c.restore(saved, false));
            if let Err(err) = put_back {
                eprintln!("could not put the user's clipboard back: {err}");
            }
        }
    }
}

fn register(name: &str) -> u32 {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: NUL-terminated UTF-16 that outlives the call.
    let id = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
    assert_ne!(id, 0);
    id
}

fn message_window() -> HWND {
    // SAFETY: a message-only window of the predefined STATIC class; callers destroy it.
    let window = unsafe {
        CreateWindowExW(
            0,
            w!("STATIC"),
            null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            null_mut(),
            null_mut(),
            null(),
        )
    };
    assert!(!window.is_null());
    window
}

/// Opens the clipboard on this thread; other apps may hold it for a moment.
fn open() {
    open_as(null_mut());
}

fn open_as(owner: HWND) {
    for _ in 0..100 {
        // SAFETY: `owner` is null or a live window of this thread; every caller closes it.
        if unsafe { OpenClipboard(owner) } != 0 {
            return;
        }
        sleep(Duration::from_millis(20));
    }
    panic!("clipboard stayed locked");
}

fn close() {
    // SAFETY: this thread opened it.
    unsafe { CloseClipboard() };
}

/// Puts the exclusion markers on the clipboard this thread has open.
fn put_markers() {
    for name in EXCLUDE {
        // SAFETY: a fresh four-byte block, filled while locked, then given to the clipboard.
        unsafe {
            let mem = GlobalAlloc(GMEM_MOVEABLE, 4);
            assert!(!mem.is_null());
            let ptr = GlobalLock(mem).cast::<u32>();
            assert!(!ptr.is_null());
            ptr.write_unaligned(0);
            GlobalUnlock(mem);
            assert!(!SetClipboardData(register(name), mem).is_null());
        }
    }
}

/// Replaces the clipboard with `formats` written by hand.
fn put_raw(formats: &[(u32, &[u8])]) {
    open();
    // SAFETY: the clipboard is open on this thread.
    assert_ne!(unsafe { EmptyClipboard() }, 0);
    for &(format, bytes) in formats {
        // SAFETY: a fresh non-empty block, filled while locked, then given to the clipboard.
        unsafe {
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
            assert!(!mem.is_null());
            let ptr = GlobalLock(mem).cast::<u8>();
            assert!(!ptr.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
            GlobalUnlock(mem);
            assert!(!SetClipboardData(format, mem).is_null());
        }
    }
    close();
}

/// The bytes of `format`, or `None` when it is absent.
fn read_raw(format: u32) -> Option<Vec<u8>> {
    open();
    // SAFETY: the clipboard is open; the handle is an HGLOBAL for these formats.
    let bytes = unsafe {
        let handle = GetClipboardData(format);
        if handle.is_null() {
            None
        } else {
            let size = GlobalSize(handle);
            let ptr = GlobalLock(handle).cast::<u8>();
            assert!(!ptr.is_null());
            let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
            GlobalUnlock(handle);
            Some(bytes)
        }
    };
    close();
    bytes
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain([0])
        .flat_map(u16::to_ne_bytes)
        .collect()
}

fn assert_excluded() {
    for name in EXCLUDE {
        assert_eq!(
            read_raw(register(name)),
            Some(0u32.to_ne_bytes().to_vec()),
            "{name}"
        );
    }
}

#[test]
fn set_marks_exclusion_and_restore_brings_back_every_format_byte_for_byte() {
    if !clipboard_test() {
        return;
    }
    let _user = UserClipboard::save();
    let text = u32::from(CF_UNICODETEXT);
    let custom = register("OxiYapper.ClipboardLiveTest");
    let original_text = utf16z("oxi clipboard test: before");
    let original_custom: Vec<u8> = (0..=255).collect();
    let zero = 0u32.to_ne_bytes();
    let one = 1u32.to_ne_bytes();
    // A normal copy: history on. A restore that keeps this saved marker leaves history=1 and
    // the `assert_excluded` after it fails.
    put_raw(&[
        (register("CanIncludeInClipboardHistory"), &one),
        (register("CanUploadToCloudClipboard"), &zero),
        (text, &original_text),
        (custom, &original_custom),
    ]);

    let mut clip = WinClipboard::new().unwrap();
    let Snap::Complete(saved) = clip.snapshot() else {
        panic!("snapshot of text plus custom formats is not complete");
    };
    assert!(!clip.is_excluded(&saved), "history=1 was read as excluded");
    let before = clip.sequence();
    let seq = clip.set_text("oxi clipboard test: after", false).unwrap();
    assert_ne!(seq, before, "set_text did not change the sequence number");
    assert_eq!(
        seq,
        clip.sequence(),
        "the close changed the sequence number"
    );
    assert_eq!(read_raw(text), Some(utf16z("oxi clipboard test: after")));
    assert_eq!(read_raw(custom), None);
    assert_excluded();

    clip.restore(saved, false).unwrap();
    assert_eq!(read_raw(text), Some(original_text));
    assert_eq!(read_raw(custom), Some(original_custom));
    assert_excluded();
}

/// Holds the clipboard open on another thread (with `owner_window` or a null owner) while
/// snapshot and `set_text` run; both must see it locked.
fn locked_while_held(owner_window: bool) {
    let _user = UserClipboard::save();
    let (opened_tx, opened_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let holder = thread::spawn(move || {
        let window = if owner_window {
            message_window()
        } else {
            null_mut()
        };
        open_as(window);
        opened_tx.send(()).unwrap();
        let _ = release_rx.recv();
        close();
        if owner_window {
            // SAFETY: created on this thread above.
            unsafe { DestroyWindow(window) };
        }
    });
    opened_rx.recv().unwrap();
    let mut clip = WinClipboard::new().unwrap();
    let snap = clip.snapshot();
    let set = clip.set_text("oxi clipboard test: locked", false);
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(matches!(snap, Snap::Locked), "snapshot gave {snap:?}");
    assert_eq!(set, Err(ClipError::Locked));
}

#[test]
fn clipboard_held_through_a_window_is_locked() {
    if clipboard_test() {
        locked_while_held(true);
    }
}

#[test]
fn clipboard_held_with_a_null_owner_is_locked() {
    if clipboard_test() {
        locked_while_held(false);
    }
}

#[test]
fn a_lone_metafile_is_incomplete() {
    if !clipboard_test() {
        return;
    }
    let _user = UserClipboard::save();
    // SAFETY: an empty in-memory metafile; the clipboard owns the handle once it is set.
    unsafe {
        let hdc = CreateEnhMetaFileW(null_mut(), null(), null(), null());
        assert!(!hdc.is_null());
        let emf = CloseEnhMetaFile(hdc);
        assert!(!emf.is_null());
        open();
        assert_ne!(EmptyClipboard(), 0);
        put_markers();
        assert!(!SetClipboardData(u32::from(CF_ENHMETAFILE), emf).is_null());
        close();
    }
    let snap = WinClipboard::new().unwrap().snapshot();
    assert!(matches!(snap, Snap::Incomplete), "snapshot gave {snap:?}");
}

/// Answers the messages sent to this thread's windows (here the pending `WM_RENDERFORMAT`).
fn pump() {
    // SAFETY: MSG is plain data; PeekMessageW fills it before DispatchMessageW reads it.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while PeekMessageW(&raw mut msg, null_mut(), 0, 0, PM_REMOVE) != 0 {
            DispatchMessageW(&raw const msg);
        }
    }
}

#[test]
fn a_delayed_render_its_owner_does_not_answer_gives_up_as_incomplete() {
    if !clipboard_test() {
        return;
    }
    let _user = UserClipboard::save();
    let delayed = register("OxiYapper.ClipboardLiveTest.Delayed");
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    // The owner promises a format, then does not pump messages until released.
    let owner = thread::spawn(move || {
        let window = message_window();
        open_as(window);
        // SAFETY: the clipboard is open by `window`; a null handle means delayed render.
        unsafe {
            assert_ne!(EmptyClipboard(), 0);
            put_markers();
            SetClipboardData(delayed, null_mut());
        }
        close();
        ready_tx.send(()).unwrap();
        let _ = release_rx.recv();
        while done_rx.try_recv().is_err() {
            pump();
            sleep(Duration::from_millis(5));
        }
        // SAFETY: created on this thread above.
        unsafe { DestroyWindow(window) };
    });
    ready_rx.recv().unwrap();
    let (snap_tx, snap_rx) = mpsc::channel();
    let start = Instant::now();
    thread::spawn(move || {
        let snap = WinClipboard::new().unwrap().snapshot();
        let _ = snap_tx.send(matches!(snap, Snap::Incomplete));
    });
    let incomplete = snap_rx.recv_timeout(Duration::from_secs(3));
    let took = start.elapsed();
    release_tx.send(()).unwrap();
    // The reader that gave up closes the clipboard once the owner answers.
    open();
    close();
    done_tx.send(()).unwrap();
    owner.join().unwrap();
    assert_eq!(incomplete, Ok(true), "snapshot did not give up");
    assert!(took < Duration::from_secs(2), "gave up after {took:?}");
}
