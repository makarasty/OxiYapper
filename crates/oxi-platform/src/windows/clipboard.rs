//! Clipboard snapshot and restore (ARCHITECTURE section 10, steps 4, 5 and 7; items 37-40).

use std::ptr::{copy_nonoverlapping, null_mut};
use std::thread::sleep;
use std::time::{Duration, Instant};

use oxi_core::paste::{
    ClipError, Clipboard, DELAYED_RENDER_TIMEOUT, OPEN_RETRIES, OPEN_RETRY_GAP, SNAPSHOT_CAP, Snap,
};
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardSequenceNumber, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
    CF_ENHMETAFILE, CF_GDIOBJLAST, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY, CF_PALETTE,
    CF_PRIVATEFIRST, CF_TEXT, CF_UNICODETEXT,
};

/// Registered formats that keep data out of clipboard managers, Win+V history and cloud sync.
const EXCLUDE: [&str; 3] = [
    "ExcludeClipboardContentFromMonitorProcessing",
    "CanIncludeInClipboardHistory",
    "CanUploadToCloudClipboard",
];

/// Formats the system synthesises from one another. Owner formats are listed before the
/// synthesised ones, so only the first of a group is copied; restore makes the rest again.
/// `CF_BITMAP` is a GDI handle and never copied, so its group is the two DIB formats.
const SYNTH_GROUPS: [&[u16]; 2] = [&[CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT], &[CF_DIB, CF_DIBV5]];

/// The Windows clipboard, opened with no owner window for each call.
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
            let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
            *id = unsafe { RegisterClipboardFormatW(wide.as_ptr()) };
            if *id == 0 {
                return Err(os_error("RegisterClipboardFormatW"));
            }
        }
        Ok(Self { exclude })
    }

    /// Empties the clipboard and writes `formats`, then the exclusion formats (last, so a
    /// saved `CanIncludeInClipboardHistory` = 1 cannot override ours).
    fn replace<'a>(
        &self,
        formats: impl IntoIterator<Item = (u32, &'a [u8])>,
    ) -> Result<(), ClipError> {
        let _open = Open::new()?;
        // SAFETY: the clipboard is open on this thread (`_open`).
        if unsafe { EmptyClipboard() } == 0 {
            return Err(os_error("EmptyClipboard"));
        }
        for (format, bytes) in formats {
            put(format, bytes)?;
        }
        for id in self.exclude {
            put(id, &0u32.to_ne_bytes())?;
        }
        Ok(())
    }
}

impl Clipboard for WinClipboard {
    type Snapshot = Saved;

    fn snapshot(&mut self) -> Snap<Saved> {
        let Ok(_open) = Open::new() else {
            return Snap::Locked;
        };
        let mut saved: Vec<(u32, Vec<u8>)> = Vec::new();
        let mut room = SNAPSHOT_CAP;
        let mut format = 0;
        loop {
            // SAFETY: the clipboard is open on this thread (`_open`).
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                // Zero is the end with ERROR_SUCCESS, or a failure with its code.
                // SAFETY: reads this thread's last-error value, no arguments.
                return if unsafe { GetLastError() } == 0 {
                    Snap::Complete(Saved(saved))
                } else {
                    Snap::Incomplete
                };
            }
            let kept: Vec<u32> = saved.iter().map(|(f, _)| *f).collect();
            if !copyable(format, &kept) {
                continue;
            }
            let start = Instant::now();
            // SAFETY: the clipboard is open on this thread; a delayed render runs in the owner.
            let handle = unsafe { GetClipboardData(format) };
            // shortcut: a slow delayed render is detected after it returns, not cut off; the
            // wait is bounded by the system's own render timeout. Upgrade if users report hangs.
            if handle.is_null() || start.elapsed() > duration(DELAYED_RENDER_TIMEOUT) {
                return Snap::Incomplete;
            }
            let Some(bytes) = read_global(handle, room) else {
                return Snap::Incomplete;
            };
            room -= bytes.len();
            saved.push((format, bytes));
        }
    }

    fn set_text(&mut self, text: &str) -> Result<u32, ClipError> {
        let bytes: Vec<u8> = text
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_ne_bytes)
            .collect();
        self.replace([(u32::from(CF_UNICODETEXT), bytes.as_slice())])?;
        Ok(self.sequence())
    }

    fn sequence(&self) -> u32 {
        // SAFETY: no arguments, no preconditions.
        unsafe { GetClipboardSequenceNumber() }
    }

    fn restore(&mut self, snapshot: Saved) -> Result<(), ClipError> {
        self.replace(snapshot.0.iter().map(|(f, b)| (*f, b.as_slice())))
    }
}

/// The clipboard, open on this thread until dropped.
struct Open;

impl Open {
    fn new() -> Result<Self, ClipError> {
        for attempt in 0..OPEN_RETRIES {
            if attempt > 0 {
                sleep(duration(OPEN_RETRY_GAP));
            }
            // SAFETY: a null owner is allowed; `Drop` closes what this opens.
            if unsafe { OpenClipboard(null_mut()) } != 0 {
                return Ok(Self);
            }
        }
        Err(ClipError::Locked)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this thread opened the clipboard in `Open::new`.
        unsafe { CloseClipboard() };
    }
}

/// Whether `format` is an HGLOBAL the owner placed itself (not a GDI handle, not owner-drawn,
/// not synthesised from a format already in `kept`).
fn copyable(format: u32, kept: &[u32]) -> bool {
    let Ok(id) = u16::try_from(format) else {
        return false;
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
    let synthesised = SYNTH_GROUPS
        .iter()
        .any(|g| g.contains(&id) && g.iter().any(|f| kept.contains(&u32::from(*f))));
    !handle && !synthesised
}

/// Copies an HGLOBAL's bytes, or `None` when it is bigger than `room` or cannot be locked.
fn read_global(handle: HANDLE, room: usize) -> Option<Vec<u8>> {
    // SAFETY: `handle` came from `GetClipboardData` for an HGLOBAL format, clipboard still open.
    let size = unsafe { GlobalSize(handle) };
    if size > room {
        return None;
    }
    if size == 0 {
        return Some(Vec::new());
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
    // A zero-byte movable block is allocated discarded and cannot be locked: use one byte.
    // SAFETY: plain allocation, freed below on every failure path.
    let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) };
    if mem.is_null() {
        return Err(os_error("GlobalAlloc"));
    }
    // SAFETY: `mem` is a live movable block from `GlobalAlloc`.
    let ptr = unsafe { GlobalLock(mem) };
    if ptr.is_null() {
        let err = os_error("GlobalLock");
        // SAFETY: not handed to the clipboard, so it is still ours to free.
        unsafe { GlobalFree(mem) };
        return Err(err);
    }
    // SAFETY: the block holds at least `bytes.len()` bytes and does not overlap `bytes`;
    // the unlock balances the lock above.
    unsafe {
        copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
        GlobalUnlock(mem);
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

fn duration(ms: oxi_core::types::Millis) -> Duration {
    Duration::from_millis(ms.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_owner_hglobal_formats_only() {
        let text = u32::from(CF_UNICODETEXT);
        let dib = u32::from(CF_DIB);
        let custom = 0xC123;
        let cases: [(u32, &[u32], bool); 10] = [
            (text, &[], true),
            (u32::from(CF_TEXT), &[text], false),
            (u32::from(CF_TEXT), &[], true),
            (text, &[u32::from(CF_TEXT)], false),
            (u32::from(CF_DIBV5), &[dib], false),
            (dib, &[text], true),
            (u32::from(CF_BITMAP), &[], false),
            (u32::from(CF_ENHMETAFILE), &[], false),
            (0x0300, &[], false),
            (custom, &[text, dib], true),
        ];
        for (format, kept, want) in cases {
            assert_eq!(
                copyable(format, kept),
                want,
                "format {format:#x} kept {kept:?}"
            );
        }
    }
}
