//! Real Windows clipboard. Runs only with `OXI_CLIPBOARD_TEST=1`. Each test puts back the
//! clipboard it found, and its own data carries the history-exclusion formats (no Win+V entries).
#![cfg(windows)]

use std::ptr::null_mut;
use std::sync::mpsc;
use std::thread::{self, sleep};
use std::time::Duration;

use oxi_core::paste::{ClipError, Clipboard, Snap};
use oxi_platform::os::clipboard::{Saved, WinClipboard};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, HWND_MESSAGE};
use windows_sys::w;

const EXCLUDE: [&str; 3] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
];

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
            WinClipboard::new().unwrap().restore(saved).unwrap();
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

/// Opens the clipboard on this thread; other apps may hold it for a moment.
fn open() {
    open_as(null_mut());
}

fn open_as(owner: HWND) {
    for _ in 0..50 {
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

/// Replaces the clipboard with `formats` written by hand.
fn put_raw(formats: &[(u32, &[u8])]) {
    open();
    // SAFETY: the clipboard is open on this thread.
    assert_ne!(unsafe { EmptyClipboard() }, 0);
    for &(format, bytes) in formats {
        // SAFETY: a fresh block of `len` bytes, filled while locked, then given to the clipboard.
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
    let mut setup = vec![
        (text, original_text.as_slice()),
        (custom, original_custom.as_slice()),
    ];
    setup.extend(EXCLUDE.map(|n| (register(n), zero.as_slice())));
    put_raw(&setup);

    let mut clip = WinClipboard::new().unwrap();
    let Snap::Complete(saved) = clip.snapshot() else {
        panic!("snapshot of text plus a custom format is not complete");
    };
    let before = clip.sequence();
    let seq = clip.set_text("oxi clipboard test: after").unwrap();
    assert_ne!(seq, before, "set_text did not change the sequence number");
    assert_eq!(seq, clip.sequence());
    assert_eq!(read_raw(text), Some(utf16z("oxi clipboard test: after")));
    assert_eq!(read_raw(custom), None);
    assert_excluded();

    clip.restore(saved).unwrap();
    assert_eq!(read_raw(text), Some(original_text));
    assert_eq!(read_raw(custom), Some(original_custom));
    assert_excluded();
}

#[test]
fn clipboard_held_open_by_another_thread_is_locked() {
    if !clipboard_test() {
        return;
    }
    // Put back if the clipboard is not locked after all and `set_text` overwrites it.
    let _user = UserClipboard::save();
    let (opened_tx, opened_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    // Another app holds the clipboard through its window: a null-owner open from a second
    // thread of the same process does not lock it.
    let holder = thread::spawn(move || {
        // SAFETY: a message-only window of the predefined STATIC class, destroyed below.
        let window = unsafe {
            CreateWindowExW(
                0,
                w!("STATIC"),
                null_mut(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                null_mut(),
                null_mut(),
                null_mut(),
            )
        };
        assert!(!window.is_null());
        open_as(window);
        opened_tx.send(()).unwrap();
        let _ = release_rx.recv();
        close();
        // SAFETY: created on this thread above.
        unsafe { DestroyWindow(window) };
    });
    opened_rx.recv().unwrap();
    let mut clip = WinClipboard::new().unwrap();
    let snap = clip.snapshot();
    let set = clip.set_text("oxi clipboard test: locked");
    release_tx.send(()).unwrap();
    holder.join().unwrap();
    assert!(matches!(snap, Snap::Locked), "snapshot gave {snap:?}");
    assert_eq!(set, Err(ClipError::Locked));
}
