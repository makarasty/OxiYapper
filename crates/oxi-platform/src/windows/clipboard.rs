//! Clipboard snapshot and restore (ARCHITECTURE section 10, steps 4, 5 and 7; items 37-40).

use std::marker::PhantomData;
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, sleep};
use std::time::Duration;

use oxi_core::paste::{
    ClipError, Clipboard, DELAYED_RENDER_TIMEOUT, OPEN_RETRIES, OPEN_RETRY_GAP, SNAPSHOT_CAP, Snap,
};
use oxi_core::types::Millis;
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HWND, SetLastError};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData, GetClipboardOwner,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_FIXED, GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
    CF_ENHMETAFILE, CF_GDIOBJLAST, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY, CF_PALETTE,
    CF_PRIVATEFIRST, CF_TEXT, CF_UNICODETEXT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, HWND_MESSAGE, IsHungAppWindow,
};
use windows_sys::w;

use super::wide;

/// Registered formats that keep data out of clipboard managers, Win+V history and cloud sync.
/// Indexes into `WinClipboard::exclude`: monitor processing, history, cloud.
pub const EXCLUDE: [&str; 3] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
];

/// Formats the system synthesises from one another. Owner formats are listed before the
/// synthesised ones, so only the first of a group is copied; restore makes the rest again.
/// `CF_BITMAP` is a GDI handle and never copied, so its group is the two DIB formats.
const SYNTH_GROUPS: [&[u16]; 2] = [&[CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT], &[CF_DIB, CF_DIBV5]];

/// The Windows clipboard. Each call opens it through its own message-only window, which
/// is destroyed right after the close, so no window of ours is left to answer clipboard messages.
#[derive(Debug)]
pub struct WinClipboard {
    exclude: [u32; 3],
}

/// Every copied format with its bytes, in clipboard order.
pub struct Saved(Vec<(u32, Vec<u8>)>);

/// Format ids and sizes only: the bytes are the user's clipboard and never reach a log.
impl std::fmt::Debug for Saved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(self.0.iter().map(|(id, bytes)| (id, bytes.len())))
            .finish()
    }
}

impl WinClipboard {
    pub fn new() -> Result<Self, ClipError> {
        let mut exclude = [0; 3];
        for (id, name) in exclude.iter_mut().zip(EXCLUDE) {
            let name = wide(name);
            // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
            *id = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
            if *id == 0 {
                return Err(os_error("RegisterClipboardFormatW"));
            }
        }
        Ok(Self { exclude })
    }

    /// Empties the clipboard, writes the marker formats first (a later failure never leaves
    /// data on it unmarked), then `formats` minus any saved marker values. With `history` the
    /// markers say "include in history, never upload to the cloud"; without, everything is
    /// excluded. The cloud upload flag is always 0.
    /// Returns the sequence number after the close: the close itself bumps it (by 2 to 3 in
    /// the live test, likely the formats synthesised then), so a number read inside never
    /// matches a later `sequence()`. Another app writing between the close and that read
    /// must empty the clipboard, which takes ownership from our window: then the number read
    /// inside the open is returned, which never matches again, so no restore overwrites
    /// their data (item 39).
    fn replace<'a>(
        &self,
        history: bool,
        formats: impl IntoIterator<Item = (u32, &'a [u8])>,
    ) -> Result<u32, ClipError> {
        let window = Window::new()?;
        let open = Open::new(&window)?;
        // SAFETY: the clipboard is open on this thread (`open`).
        if unsafe { EmptyClipboard() } == 0 {
            return Err(os_error("EmptyClipboard"));
        }
        for (format, value) in markers(self.exclude, history) {
            put(format, &value.to_ne_bytes())?;
        }
        for (format, bytes) in formats {
            if !self.exclude.contains(&format) {
                put(format, bytes)?;
            }
        }
        let inside = self.sequence();
        drop(open);
        let after = self.sequence();
        // SAFETY: no arguments; compared as a value only.
        let ours = unsafe { GetClipboardOwner() } == window.0;
        Ok(if ours { after } else { inside })
    }
}

impl Clipboard for WinClipboard {
    type Snapshot = Saved;

    /// Reads on a helper thread: `GetClipboardData` on a delayed-render format waits for its
    /// owner without a time limit, so the wait here gives up after `DELAYED_RENDER_TIMEOUT`.
    /// A helper that gave up keeps the clipboard open until the owner answers or exits (the
    /// thread cannot be cut short); an owner Windows already marks hung is never asked.
    fn snapshot(&mut self) -> Snap<Saved> {
        // SAFETY: no arguments; a null owner is passed on as null.
        if unsafe { IsHungAppWindow(GetClipboardOwner()) } != 0 {
            return Snap::Incomplete;
        }
        let (tx, rx) = mpsc::channel();
        let exclude = self.exclude;
        let spawned = thread::Builder::new()
            .name("clipboard-snapshot".into())
            .spawn(move || {
                let snap = read_all(&tx, exclude);
                let _ = tx.send(Step::Done(snap));
            });
        if spawned.is_err() {
            return Snap::Incomplete;
        }
        // The open retries are bounded by OPEN_RETRIES; only the reads after it are timed.
        let mut opened = false;
        loop {
            let step = if opened {
                rx.recv_timeout(duration(DELAYED_RENDER_TIMEOUT)).ok()
            } else {
                rx.recv().ok()
            };
            match step {
                Some(Step::Reading) => opened = true,
                Some(Step::Done(snap)) => return snap,
                None => return Snap::Incomplete,
            }
        }
    }

    fn is_excluded(&self, snapshot: &Saved) -> bool {
        carries_exclusion(&snapshot.0, self.exclude)
    }

    fn set_text(&mut self, text: &str, in_history: bool) -> Result<u32, ClipError> {
        let bytes: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_ne_bytes)
            .collect();
        self.replace(in_history, [(u32::from(CF_UNICODETEXT), bytes.as_slice())])
    }

    fn sequence(&self) -> u32 {
        // SAFETY: no arguments, no preconditions.
        unsafe { GetClipboardSequenceNumber() }
    }

    fn restore(&mut self, snapshot: Saved, in_history: bool) -> Result<(), ClipError> {
        self.replace(
            in_history,
            snapshot.0.iter().map(|(f, b)| (*f, b.as_slice())),
        )
        .map(drop)
    }
}

/// The marker formats and DWORD values a write puts on the clipboard.
fn markers([monitor, in_history, cloud]: [u32; 3], history: bool) -> Vec<(u32, u32)> {
    if history {
        vec![(in_history, 1), (cloud, 0)]
    } else {
        vec![(monitor, 0), (in_history, 0), (cloud, 0)]
    }
}

/// Saved data its owner kept out of history. Any history marker other than 1 counts: an
/// unreadable marker must not let a password into Win+V.
fn carries_exclusion(saved: &[(u32, Vec<u8>)], [monitor, in_history, _]: [u32; 3]) -> bool {
    saved.iter().any(|(format, bytes)| {
        *format == monitor
            || (*format == in_history && bytes.get(..4) != Some(&1u32.to_ne_bytes()[..]))
    })
}

/// What the snapshot thread reports: about to block on the clipboard, or finished (closed).
enum Step {
    Reading,
    Done(Snap<Saved>),
}

/// Copies every copyable format, sending `Reading` before each call that can block on an owner.
/// The clipboard is closed when this returns.
fn read_all(tx: &Sender<Step>, exclude: [u32; 3]) -> Snap<Saved> {
    let Ok(window) = Window::new() else {
        return Snap::Incomplete;
    };
    let Ok(_open) = Open::new(&window) else {
        return Snap::Locked;
    };
    let mut saved: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut skipped_handle = false;
    let mut room = SNAPSHOT_CAP;
    let mut format = 0;
    loop {
        if tx.send(Step::Reading).is_err() {
            return Snap::Incomplete; // the caller gave up already
        }
        // SAFETY: the clipboard is open on this thread (`_open`).
        format = unsafe { EnumClipboardFormats(format) };
        if format == 0 {
            // Zero is the end with ERROR_SUCCESS, or a failure with its code. A clipboard
            // of handle formats only (a lone metafile, markers aside) would restore as nothing.
            // SAFETY: reads this thread's last-error value, no arguments.
            let failed = unsafe { GetLastError() } != 0;
            let only_markers = saved.iter().all(|(f, _)| exclude.contains(f));
            return if failed || (only_markers && skipped_handle) {
                Snap::Incomplete
            } else {
                Snap::Complete(Saved(saved))
            };
        }
        let kept: Vec<u32> = saved.iter().map(|(f, _)| *f).collect();
        match kind(format, &kept) {
            Kind::Handle => skipped_handle = true,
            Kind::Synthesised => {}
            Kind::Copy => {
                // SAFETY: the clipboard is open on this thread; a delayed render runs in the owner.
                let handle = unsafe { GetClipboardData(format) };
                let Some(bytes) = (!handle.is_null())
                    .then(|| read_global(handle, room))
                    .flatten()
                else {
                    return Snap::Incomplete;
                };
                room -= bytes.len();
                saved.push((format, bytes));
            }
        }
    }
}

/// A message-only window, destroyed on drop. It owns the clipboard while one call has it open.
struct Window(HWND);

impl Window {
    fn new() -> Result<Self, ClipError> {
        // SAFETY: the predefined STATIC class needs no registration; HWND_MESSAGE makes the
        // window invisible and message-only; `Drop` destroys it on this same thread.
        let hwnd = unsafe {
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
        if hwnd.is_null() {
            return Err(os_error("CreateWindowExW"));
        }
        Ok(Self(hwnd))
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        // SAFETY: created by `Window::new` on this thread and not destroyed yet.
        unsafe { DestroyWindow(self.0) };
    }
}

/// The clipboard, open on this thread until dropped. A null-owner open does not exclude
/// other null-owner opens, so the open goes through a window: then another app holding the
/// clipboard (with or without a window) makes this fail as Locked (item 37).
/// Borrows the window so the window cannot be destroyed while the clipboard is open.
struct Open<'w>(PhantomData<&'w Window>);

impl<'w> Open<'w> {
    fn new(window: &'w Window) -> Result<Self, ClipError> {
        for attempt in 0..OPEN_RETRIES {
            if attempt > 0 {
                sleep(duration(OPEN_RETRY_GAP));
            }
            // SAFETY: `window` is a live window of this thread; `Drop` closes what this opens.
            if unsafe { OpenClipboard(window.0) } != 0 {
                return Ok(Self(PhantomData));
            }
        }
        Err(ClipError::Locked)
    }
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `Open::new`.
        unsafe { CloseClipboard() };
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    /// An HGLOBAL the owner placed: copied.
    Copy,
    /// Made by the system from a format already copied: made again on restore.
    Synthesised,
    /// A GDI handle, owner-drawn or private format: bytes cannot be copied.
    Handle,
}

fn kind(format: u32, kept: &[u32]) -> Kind {
    let Ok(id) = u16::try_from(format) else {
        return Kind::Copy;
    };
    let handle = matches!(
        id,
        CF_BITMAP
            | CF_METAFILEPICT
            | CF_PALETTE
            | CF_ENHMETAFILE
            | CF_OWNERDISPLAY
            | CF_DSPBITMAP
            | CF_DSPMETAFILEPICT
            | CF_DSPENHMETAFILE
    ) || (CF_PRIVATEFIRST..=CF_GDIOBJLAST).contains(&id);
    if handle {
        Kind::Handle
    } else if SYNTH_GROUPS
        .iter()
        .any(|g| g.contains(&id) && g.iter().any(|f| kept.contains(&u32::from(*f))))
    {
        Kind::Synthesised
    } else {
        Kind::Copy
    }
}

/// Copies an HGLOBAL's bytes, or `None` when it is bigger than `room` or cannot be read.
fn read_global(handle: HANDLE, room: usize) -> Option<Vec<u8>> {
    // GlobalSize gives 0 both for an empty block and on failure; the last error tells them apart.
    // SAFETY: `handle` came from `GetClipboardData` for an HGLOBAL format, clipboard still open.
    let size = unsafe {
        SetLastError(0);
        GlobalSize(handle)
    };
    if size == 0 {
        // SAFETY: reads this thread's last-error value, no arguments.
        return (unsafe { GetLastError() } == 0).then(Vec::new);
    }
    if size > room {
        return None;
    }
    // SAFETY: as above; unlocked below.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        return None;
    }
    // SAFETY: a locked block of `size` bytes stays valid until `GlobalUnlock`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) }.to_vec();
    // SAFETY: balances the `GlobalLock` above.
    unsafe { GlobalUnlock(handle) };
    Some(bytes)
}

/// Puts one format on the open clipboard; the system owns the block once it is set.
fn put(format: u32, bytes: &[u8]) -> Result<(), ClipError> {
    // A zero-byte movable block is allocated discarded and SetClipboardData refuses it; a
    // zero-byte fixed one is accepted (Windows hands it back to readers as one byte).
    let flags = if bytes.is_empty() {
        GMEM_FIXED
    } else {
        GMEM_MOVEABLE
    };
    // SAFETY: plain allocation, freed below on every failure path.
    let mem = unsafe { GlobalAlloc(flags, bytes.len()) };
    if mem.is_null() {
        return Err(os_error("GlobalAlloc"));
    }
    if !bytes.is_empty() {
        // SAFETY: `mem` is a live movable block from `GlobalAlloc`.
        let ptr = unsafe { GlobalLock(mem) };
        if ptr.is_null() {
            let err = os_error("GlobalLock");
            // SAFETY: not handed to the clipboard, so it is still ours to free.
            unsafe { GlobalFree(mem) };
            return Err(err);
        }
        // SAFETY: the block holds `bytes.len()` bytes and does not overlap `bytes`; the
        // unlock balances the lock above.
        unsafe {
            copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            GlobalUnlock(mem);
        }
    }
    // SAFETY: the clipboard is open on this thread (the caller holds `Open`), `mem` is unlocked.
    if unsafe { SetClipboardData(format, mem) }.is_null() {
        let err = os_error("SetClipboardData");
        // SAFETY: a failed set leaves the block with us.
        unsafe { GlobalFree(mem) };
        return Err(err);
    }
    Ok(())
}

fn os_error(call: &'static str) -> ClipError {
    // SAFETY: reads this thread's last-error value, no arguments.
    let code = unsafe { GetLastError() };
    ClipError::Os { call, code }
}

fn duration(ms: Millis) -> Duration {
    Duration::from_millis(ms.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IDS: [u32; 3] = [0xC001, 0xC002, 0xC003];
    /// A clipboard format id and its bytes.
    type Format = (u32, Vec<u8>);

    #[test]
    fn markers_put_history_on_or_exclude_everything_and_never_allow_the_cloud() {
        assert_eq!(markers(IDS, true), [(0xC002, 1), (0xC003, 0)]);
        assert_eq!(markers(IDS, false), [(0xC001, 0), (0xC002, 0), (0xC003, 0)]);
    }

    #[test]
    fn exclusion_is_read_from_the_saved_markers() {
        let d = |v: u32| v.to_ne_bytes().to_vec();
        let text = (u32::from(CF_UNICODETEXT), vec![104, 0, 0, 0]);
        let cases: [(&str, Vec<Format>, bool); 7] = [
            ("no markers", vec![text.clone()], false),
            ("history 1", vec![(0xC002, d(1)), text.clone()], false),
            ("cloud 0 only", vec![(0xC003, d(0)), text.clone()], false),
            ("history 0", vec![(0xC002, d(0)), text.clone()], true),
            (
                "monitor, any value",
                vec![(0xC001, d(1)), text.clone()],
                true,
            ),
            (
                "monitor, no data",
                vec![(0xC001, Vec::new()), text.clone()],
                true,
            ),
            ("history unreadable", vec![(0xC002, vec![1]), text], true),
        ];
        for (name, saved, want) in cases {
            assert_eq!(carries_exclusion(&saved, IDS), want, "{name}");
        }
    }

    #[test]
    fn copies_owner_hglobal_formats_only() {
        let text = u32::from(CF_UNICODETEXT);
        let dib = u32::from(CF_DIB);
        let custom = 0xC123;
        let cases: [(u32, &[u32], Kind); 11] = [
            (text, &[], Kind::Copy),
            (u32::from(CF_TEXT), &[text], Kind::Synthesised),
            (u32::from(CF_TEXT), &[], Kind::Copy),
            (text, &[u32::from(CF_TEXT)], Kind::Synthesised),
            (u32::from(CF_DIBV5), &[dib], Kind::Synthesised),
            (dib, &[text], Kind::Copy),
            (u32::from(CF_BITMAP), &[], Kind::Handle),
            (u32::from(CF_ENHMETAFILE), &[], Kind::Handle),
            (u32::from(CF_METAFILEPICT), &[text], Kind::Handle),
            (0x0300, &[], Kind::Handle),
            (custom, &[text, dib], Kind::Copy),
        ];
        for (format, kept, want) in cases {
            assert_eq!(kind(format, kept), want, "format {format:#x} kept {kept:?}");
        }
    }
}
