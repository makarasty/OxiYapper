//! Clipboard snapshot and restore (ARCHITECTURE section 10, steps 4, 5 and 7; items 37-40).

use std::marker::PhantomData;
use std::ptr::{copy_nonoverlapping, null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, sleep};
use std::time::{Duration, Instant};

use oxi_core::paste::{
    ClipError, Clipboard, DELAYED_RENDER_TIMEOUT, HISTORY_TIMEOUT, HistoryItem, IncompleteWhy,
    OPEN_RETRIES, OPEN_RETRY_GAP, SNAPSHOT_CAP, Snap,
};
use oxi_core::types::Millis;
use windows::ApplicationModel::DataTransfer::{
    Clipboard as Winrt, ClipboardHistoryItem, ClipboardHistoryItemsResultStatus,
    StandardDataFormats,
};
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows_sys::Win32::Foundation::{GetLastError, GlobalFree, HANDLE, HWND, SetLastError};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, EnumClipboardFormats, GetClipboardData,
    GetClipboardFormatNameW, GetClipboardOwner, GetClipboardSequenceNumber, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GMEM_FIXED, GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
    CF_ENHMETAFILE, CF_GDIOBJLAST, CF_HDROP, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY,
    CF_PALETTE, CF_PRIVATEFIRST, CF_TEXT, CF_UNICODETEXT,
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
    /// A history call is running (or stuck after its timeout): the next one fails at once.
    history_busy: Arc<AtomicBool>,
    /// Start of [`Clipboard::now`].
    born: Instant,
    /// Each history call's limit: [`HISTORY_TIMEOUT`], or less once the budget runs low.
    history_limit: Millis,
    /// A snapshot helper is running (or stuck on an owner after it was given up): the next
    /// snapshot answers `HelperBusy` at once instead of waiting out the open retries.
    snapshot_busy: Arc<AtomicBool>,
    /// Why the last snapshot was not whole.
    last_why: Option<IncompleteWhy>,
}

/// Why a read gave no whole snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Miss {
    Incomplete(IncompleteWhy),
    Locked,
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
        Ok(Self {
            exclude,
            history_busy: Arc::default(),
            born: Instant::now(),
            history_limit: HISTORY_TIMEOUT,
            snapshot_busy: Arc::default(),
            last_why: None,
        })
    }

    /// Reads on a helper thread: `GetClipboardData` on a delayed-render format waits for its
    /// owner without a time limit, so the wait here gives up after `DELAYED_RENDER_TIMEOUT`.
    /// A helper that gave up keeps the clipboard open until the owner answers or exits (the
    /// thread cannot be cut short); an owner Windows already marks hung is never asked.
    fn read_whole(&self) -> Result<Saved, Miss> {
        // SAFETY: no arguments; a null owner is passed on as null.
        if unsafe { IsHungAppWindow(GetClipboardOwner()) } != 0 {
            return Err(Miss::Incomplete(IncompleteWhy::HungOwner));
        }
        if self.snapshot_busy.swap(true, Ordering::Acquire) {
            return Err(Miss::Incomplete(IncompleteWhy::HelperBusy));
        }
        let free = Free(Arc::clone(&self.snapshot_busy));
        let (tx, rx) = mpsc::channel();
        let exclude = self.exclude;
        // A failed spawn drops the closure, and `free` with it.
        let spawned = thread::Builder::new()
            .name("clipboard-snapshot".into())
            .spawn(move || {
                let free = free;
                let read = read_all(&tx, exclude);
                // Freed before the answer (the clipboard is closed by now), so the caller's next
                // snapshot never finds it still set.
                drop(free);
                let _ = tx.send(Step::Done(read));
            });
        if spawned.is_err() {
            return Err(Miss::Incomplete(IncompleteWhy::NoHelper));
        }
        // The open retries are bounded by OPEN_RETRIES; only the reads after it are timed.
        let mut reading = None;
        loop {
            let step = match reading {
                Some(format) => match rx.recv_timeout(duration(DELAYED_RENDER_TIMEOUT)) {
                    Ok(step) => step,
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(Miss::Incomplete(IncompleteWhy::RenderTimeout { format }));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(Miss::Incomplete(IncompleteWhy::NoHelper));
                    }
                },
                None => rx
                    .recv()
                    .map_err(|_| Miss::Incomplete(IncompleteWhy::NoHelper))?,
            };
            match step {
                Step::Reading(format) => reading = Some(format),
                Step::Done(read) => return read,
            }
        }
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

    /// [`WinClipboard::read_whole`]; a miss is logged with its reason (names and sizes only).
    fn snapshot(&mut self) -> Snap<Saved> {
        let started = Instant::now();
        let read = self.read_whole();
        self.last_why = None;
        match read {
            Ok(saved) => Snap::Complete(saved),
            Err(miss) => {
                log_miss(miss, started.elapsed());
                match miss {
                    Miss::Locked => Snap::Locked,
                    Miss::Incomplete(why) => {
                        self.last_why = Some(why);
                        Snap::Incomplete
                    }
                }
            }
        }
    }

    fn incomplete_why(&self) -> Option<IncompleteWhy> {
        self.last_why
    }

    fn is_excluded(&self, snapshot: &Saved) -> bool {
        carries_exclusion(&snapshot.0, self.exclude) || history_skips(&snapshot.0, self.exclude)
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

    fn restore(&mut self, snapshot: &Saved, in_history: bool) -> Result<u32, ClipError> {
        self.replace(
            in_history,
            snapshot.0.iter().map(|(f, b)| (*f, b.as_slice())),
        )
    }

    /// Win+V, newest first, through the `WinRT` history API. `None` with history switched off
    /// in Windows settings, a status other than success, a failed call or a timeout.
    fn history(&mut self) -> Option<Vec<HistoryItem>> {
        let items = self.history_call(|| {
            with_items(|items| {
                items
                    .iter()
                    .map(|item| {
                        Ok(HistoryItem {
                            id: item.Id().map_err(winrt("Id"))?.to_string(),
                            text: item_text(item),
                        })
                    })
                    .collect()
            })
        });
        items.ok().flatten()
    }

    /// Ids and timestamps only: no entry's text is read, so a look costs one list call.
    fn history_ids(&mut self) -> Result<Option<Vec<String>>, ClipError> {
        self.history_call(|| {
            with_items(|items| {
                items
                    .iter()
                    .map(|item| item.Id().map(|id| id.to_string()))
                    .collect::<Result<_, _>>()
                    .map_err(winrt("Id"))
            })
        })
    }

    /// Reads that one entry's text only.
    fn history_text(&mut self, id: &str) -> Option<String> {
        let id = id.to_owned();
        let text = self.history_call(move || {
            with_items(|items| {
                let item = items
                    .iter()
                    .find(|i| i.Id().is_ok_and(|x| x == id.as_str()));
                Ok(item.and_then(item_text))
            })
        });
        text.ok().flatten().flatten()
    }

    fn delete_from_history(&mut self, id: &str) -> Result<(), ClipError> {
        const FAILED: ClipError = ClipError::Os {
            call: "DeleteItemFromHistory",
            code: 0,
        };
        let id = id.to_owned();
        let deleted = self.history_call(move || {
            with_items(|items| {
                let item = items
                    .iter()
                    .find(|i| i.Id().is_ok_and(|x| x == id.as_str()));
                match item.map(Winrt::DeleteItemFromHistory) {
                    Some(Ok(true)) => Ok(()),
                    Some(Err(e)) => Err(winrt("DeleteItemFromHistory")(e)),
                    _ => Err(FAILED),
                }
            })
        })?;
        deleted.ok_or(FAILED)
    }

    fn now(&self) -> Millis {
        Millis(u64::try_from(self.born.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    fn limit_history_calls(&mut self, limit: Millis) {
        self.history_limit = limit.min(HISTORY_TIMEOUT);
    }
}

impl WinClipboard {
    /// Runs one history call through [`on_sta`], given up at the time left of the paste's
    /// budget ([`Clipboard::limit_history_calls`]).
    fn history_call<R: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<R, ClipError> + Send + 'static,
    ) -> Result<R, ClipError> {
        on_sta(&self.history_busy, duration(self.history_limit), f)
    }
}

/// A history call that did not answer within its limit (`HRESULT_FROM_WIN32(ERROR_TIMEOUT)`).
const HISTORY_TIMED_OUT: ClipError = ClipError::Os {
    call: "WinRT clipboard history",
    code: 0x8007_05B4,
};

/// Clears the busy flag when the helper thread ends, also by a panic.
struct Free(Arc<AtomicBool>);

impl Drop for Free {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Runs `f` on a fresh thread in a single-threaded COM apartment (the `WinRT` clipboard calls
/// answer `0x8000001D` on a thread without one) and gives up after `limit`: a stalled history
/// service must not hang the paste worker. A thread that timed out keeps `busy` set until it
/// returns, and every call meanwhile fails at once instead of stacking another stuck thread.
/// Every `WinRT` object must be dropped inside `f`: one released after the apartment is gone
/// crashes.
fn on_sta<R: Send + 'static>(
    busy: &Arc<AtomicBool>,
    limit: Duration,
    f: impl FnOnce() -> Result<R, ClipError> + Send + 'static,
) -> Result<R, ClipError> {
    if busy.swap(true, Ordering::Acquire) {
        return Err(HISTORY_TIMED_OUT);
    }
    let free = Free(Arc::clone(busy));
    let (tx, rx) = mpsc::channel();
    // A failed spawn drops the closure, and `free` with it.
    thread::Builder::new()
        .name("clipboard-history".into())
        .spawn(move || {
            let free = free;
            // SAFETY: balanced by the `CoUninitialize` below, on this same thread.
            let init = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            let out = f();
            if init.is_ok() {
                // SAFETY: the `CoInitializeEx` above succeeded on this thread.
                unsafe { CoUninitialize() };
            }
            // Freed before the answer, so the caller's next call never finds it still set.
            drop(free);
            let _ = tx.send(out);
        })
        .map_err(|e| ClipError::Os {
            call: "spawn clipboard-history thread",
            code: e.raw_os_error().map_or(0, i32::cast_unsigned),
        })?;
    // A panic in `f` disconnects the channel: reported like a timeout.
    rx.recv_timeout(limit).unwrap_or(Err(HISTORY_TIMED_OUT))
}

/// Hands `f` the Win+V entries, newest first; `None` when history is off or the call did not
/// succeed. Must run inside [`on_sta`].
fn with_items<R>(
    f: impl FnOnce(&[ClipboardHistoryItem]) -> Result<R, ClipError>,
) -> Result<Option<R>, ClipError> {
    if !Winrt::IsHistoryEnabled().map_err(winrt("IsHistoryEnabled"))? {
        return Ok(None);
    }
    let call = winrt("GetHistoryItemsAsync");
    let result = Winrt::GetHistoryItemsAsync()
        .and_then(|op| op.join())
        .map_err(&call)?;
    if result.Status().map_err(&call)? != ClipboardHistoryItemsResultStatus::Success {
        return Ok(None);
    }
    let mut items: Vec<ClipboardHistoryItem> = result.Items().map_err(&call)?.into_iter().collect();
    items.sort_by_key(|i| std::cmp::Reverse(i.Timestamp().map_or(0, |t| t.UniversalTime)));
    f(&items).map(Some)
}

/// An entry's text, `None` for a non-text entry or one the history cannot render.
fn item_text(item: &ClipboardHistoryItem) -> Option<String> {
    let content = item.Content().ok()?;
    let text = StandardDataFormats::Text().ok()?;
    if !content.Contains(&text).ok()? {
        return None;
    }
    content
        .GetTextAsync()
        .ok()?
        .join()
        .ok()
        .map(|h| h.to_string())
}

/// A `WinRT` error as the failed call and its HRESULT.
fn winrt(call: &'static str) -> impl Fn(windows::core::Error) -> ClipError {
    move |e| ClipError::Os {
        call,
        code: e.code().0.cast_unsigned(),
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

/// Saved data Win+V never shows: files (Explorer puts `CF_HDROP` and shell formats, no text or
/// bitmap) or nothing but markers. Anything else may register, however big (Win+V's size limit
/// is not documented), so a doubt means waiting for the entry, never deleting under it.
fn history_skips(saved: &[(u32, Vec<u8>)], markers: [u32; 3]) -> bool {
    let shown = |f: u32| {
        [CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT, CF_DIB, CF_DIBV5]
            .iter()
            .any(|x| u32::from(*x) == f)
    };
    let files_only = saved.iter().any(|(f, _)| *f == u32::from(CF_HDROP))
        && !saved.iter().any(|(f, _)| shown(*f));
    files_only || saved.iter().all(|(f, _)| markers.contains(f))
}

/// What the snapshot thread reports: about to read this format (a call that can block on its
/// owner), or finished (closed).
enum Step {
    Reading(u32),
    Done(Result<Saved, Miss>),
}

/// Copies every copyable format, sending `Reading` before each call that can block on an owner.
/// The clipboard is closed when this returns.
fn read_all(tx: &Sender<Step>, exclude: [u32; 3]) -> Result<Saved, Miss> {
    let incomplete = |why| Err(Miss::Incomplete(why));
    let Ok(window) = Window::new() else {
        return incomplete(IncompleteWhy::NoHelper);
    };
    let Ok(_open) = Open::new(&window) else {
        return Err(Miss::Locked);
    };
    let mut saved: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut skipped_handle = false;
    let mut room = SNAPSHOT_CAP;
    let mut format = 0;
    loop {
        // Cleared first: the end of the list is told from a failure by the last error, and an
        // earlier call (a retried open, say) may have left one behind.
        // SAFETY: the clipboard is open on this thread (`_open`).
        format = unsafe {
            SetLastError(0);
            EnumClipboardFormats(format)
        };
        if format == 0 {
            // SAFETY: reads this thread's last-error value, no arguments.
            let code = unsafe { GetLastError() };
            if code != 0 {
                return incomplete(IncompleteWhy::EnumFailed { code });
            }
            // A clipboard of handle formats only (a lone metafile, markers aside) would
            // restore as nothing.
            if skipped_handle && saved.iter().all(|(f, _)| exclude.contains(f)) {
                return incomplete(IncompleteWhy::OnlyHandles);
            }
            return Ok(Saved(saved));
        }
        if tx.send(Step::Reading(format)).is_err() {
            return incomplete(IncompleteWhy::NoHelper); // the caller gave up already
        }
        let kept: Vec<u32> = saved.iter().map(|(f, _)| *f).collect();
        match kind(format, &kept) {
            Kind::Handle => skipped_handle = true,
            Kind::Synthesised => {}
            Kind::Copy => {
                // SAFETY: the clipboard is open on this thread; a delayed render runs in the owner.
                let handle = unsafe {
                    SetLastError(0);
                    GetClipboardData(format)
                };
                if handle.is_null() {
                    // SAFETY: reads this thread's last-error value, no arguments.
                    let code = unsafe { GetLastError() };
                    return incomplete(IncompleteWhy::NoData { format, code });
                }
                let bytes = read_global(handle, format, room).map_err(Miss::Incomplete)?;
                room -= bytes.len();
                saved.push((format, bytes));
            }
        }
    }
}

/// The registered name of `format` (`HTML Format`, say); empty for a standard format.
fn format_name(format: u32) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: `buf` holds 128 units; the call writes at most that many, NUL included.
    let len = unsafe { GetClipboardFormatNameW(format, buf.as_mut_ptr(), 128) };
    String::from_utf16_lossy(&buf[..usize::try_from(len).unwrap_or(0).min(buf.len())])
}

/// The exe file name of the clipboard owner's process; empty with no owner.
fn owner_exe() -> String {
    // SAFETY: no arguments; a null owner gives pid 0 and an empty name.
    let owner = unsafe { GetClipboardOwner() };
    super::input::process_facts(super::input::window_pid(owner)).0
}

/// One line per snapshot that is not whole: why, and which program and format, never content.
fn log_miss(miss: Miss, took: Duration) {
    let format = match miss {
        Miss::Incomplete(
            IncompleteWhy::NoData { format, .. }
            | IncompleteWhy::NoLock { format, .. }
            | IncompleteWhy::TooBig { format, .. }
            | IncompleteWhy::RenderTimeout { format },
        ) => Some(format),
        _ => None,
    };
    // SAFETY: no arguments.
    let clip_formats = unsafe { CountClipboardFormats() };
    tracing::info!(
        ?miss,
        format_name = format.map(format_name).unwrap_or_default(),
        clip_owner_exe = owner_exe(),
        clip_formats,
        snap_ms = took.as_millis(),
        "clipboard not saved whole"
    );
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

/// Copies an HGLOBAL's bytes of `format`, or why not: bigger than `room`, or unreadable.
fn read_global(handle: HANDLE, format: u32, room: usize) -> Result<Vec<u8>, IncompleteWhy> {
    let as_u64 = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    // GlobalSize gives 0 both for an empty block and on failure; the last error tells them apart.
    // SAFETY: `handle` came from `GetClipboardData` for an HGLOBAL format, clipboard still open.
    let size = unsafe {
        SetLastError(0);
        GlobalSize(handle)
    };
    if size == 0 {
        // SAFETY: reads this thread's last-error value, no arguments.
        let code = unsafe { GetLastError() };
        return if code == 0 {
            Ok(Vec::new())
        } else {
            Err(IncompleteWhy::NoLock { format, code })
        };
    }
    if size > room {
        return Err(IncompleteWhy::TooBig {
            format,
            bytes: as_u64(size),
            room: as_u64(room),
        });
    }
    // SAFETY: as above; unlocked below.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        // SAFETY: reads this thread's last-error value, no arguments.
        let code = unsafe { GetLastError() };
        return Err(IncompleteWhy::NoLock { format, code });
    }
    // SAFETY: a locked block of `size` bytes stays valid until `GlobalUnlock`.
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) }.to_vec();
    // SAFETY: balances the `GlobalLock` above.
    unsafe { GlobalUnlock(handle) };
    Ok(bytes)
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

    #[test]
    fn a_stalled_history_call_times_out_and_stacks_no_second_thread() {
        let busy: Arc<AtomicBool> = Arc::default();
        let (release, stall) = mpsc::channel::<()>();
        let started = Instant::now();
        let stuck = on_sta(&busy, Duration::from_millis(50), move || {
            let _ = stall.recv();
            Ok(1)
        });
        assert_eq!(stuck, Err(HISTORY_TIMED_OUT));
        assert!(started.elapsed() < Duration::from_secs(2));
        // Still stuck: the next call fails at once instead of starting another thread.
        let next = on_sta(&busy, Duration::from_secs(5), || Ok(2));
        assert_eq!(next, Err(HISTORY_TIMED_OUT));
        drop(release);
        let deadline = Instant::now() + Duration::from_secs(5);
        while busy.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::yield_now();
        }
        assert_eq!(on_sta(&busy, Duration::from_secs(5), || Ok(3)), Ok(3));
        // A panicking call is an error, and the flag is free again after it.
        let panicked = on_sta(&busy, Duration::from_secs(5), || -> Result<u8, _> {
            panic!("helper panics")
        });
        assert_eq!(panicked, Err(HISTORY_TIMED_OUT));
        assert_eq!(on_sta(&busy, Duration::from_secs(5), || Ok(4)), Ok(4));
    }

    /// Only files are judged never to enter Win+V; a big copy may still register (its measure is
    /// not documented), so it counts as one to wait for, never as excluded.
    #[test]
    fn only_files_are_judged_never_to_enter_history() {
        let text = |n: usize| (u32::from(CF_UNICODETEXT), vec![0; n]);
        let files = (u32::from(CF_HDROP), vec![0; 40]);
        let shell = (0xC100, vec![0; 200]);
        // PrtScn at 1920x1080, 32-bit: CF_BITMAP only, read back as a synthesised DIB.
        let screenshot = (u32::from(CF_DIB), vec![0; 40 + 1920 * 1080 * 4]);
        let cases: [(&str, Vec<Format>, bool); 8] = [
            ("text", vec![text(10)], false),
            (
                "files from Explorer",
                vec![files.clone(), shell.clone()],
                true,
            ),
            ("files with text", vec![files, text(10)], false),
            ("text over 4 MB", vec![text(4 << 20)], false),
            ("big text, small html", vec![text(4 << 20), shell], false),
            (
                "big text, markers",
                vec![(0xC002, vec![1, 0, 0, 0]), text(5 << 20)],
                false,
            ),
            ("PrtScn screenshot", vec![screenshot], false),
            ("empty", Vec::new(), true),
        ];
        for (name, saved, want) in cases {
            assert_eq!(history_skips(&saved, IDS), want, "{name}");
        }
    }

    /// The paste budget is a deadline: each history call waits at most the time left, never
    /// more than [`HISTORY_TIMEOUT`]. A stalled call is given up at the time left, not at
    /// [`HISTORY_TIMEOUT`]. No clipboard or Win+V call: the stalled call is a plain closure.
    #[test]
    fn history_calls_wait_at_most_the_time_left() {
        let mut clip = WinClipboard::new().unwrap();
        assert_eq!(clip.history_limit, HISTORY_TIMEOUT);
        clip.limit_history_calls(Millis(9000));
        assert_eq!(clip.history_limit, HISTORY_TIMEOUT);
        clip.limit_history_calls(Millis(50));
        let (release, stall) = mpsc::channel::<()>();
        let started = Instant::now();
        let stuck = clip.history_call(move || {
            let _ = stall.recv();
            Ok(())
        });
        let waited = started.elapsed();
        drop(release);
        assert_eq!(stuck, Err(HISTORY_TIMED_OUT));
        assert!(
            waited < duration(HISTORY_TIMEOUT) / 2,
            "waited {waited:?}, not the 50 ms left"
        );
    }

    /// Arch-plan 3.3: while an earlier helper is still busy, a snapshot answers at once and
    /// spawns nothing. Neither opens nor reads the clipboard.
    #[test]
    fn a_busy_snapshot_helper_answers_helper_busy_at_once() {
        let clip = WinClipboard::new().unwrap();
        clip.snapshot_busy.store(true, Ordering::Release);
        let started = Instant::now();
        let read = clip.read_whole();
        assert!(started.elapsed() < Duration::from_millis(100));
        // A hung owner is answered before the flag is looked at; either way nothing was read.
        assert!(
            matches!(
                read,
                Err(Miss::Incomplete(
                    IncompleteWhy::HelperBusy | IncompleteWhy::HungOwner
                ))
            ),
            "{read:?}"
        );
        assert!(
            clip.snapshot_busy.load(Ordering::Acquire),
            "not ours to clear"
        );
    }

    /// The one read-only probe the owner allowed (arch-plan 3.3 item 5): reads the clipboard as
    /// the app does and prints only the outcome, the reason, the owner's exe and every format's
    /// id, registered name and size. No content, no write, no history call.
    /// `cargo test -p oxi-platform --lib probe_snapshot_reason -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads this machine's clipboard (read only)"]
    fn probe_snapshot_reason() {
        let mut clip = WinClipboard::new().unwrap();
        let started = Instant::now();
        let snap = clip.snapshot();
        let took = started.elapsed().as_millis();
        let (kind, saved) = match &snap {
            Snap::Complete(saved) => ("Complete", format!(" saved {saved:?}")),
            Snap::Incomplete => ("Incomplete", String::new()),
            Snap::Locked => ("Locked", String::new()),
        };
        println!(
            "snapshot {kind} in {took} ms, why {:?}{saved}",
            clip.incomplete_why()
        );
        drop(snap);
        println!("owner exe '{}'", owner_exe());
        // A format whose owner did not answer would block this listing too: names only then.
        let sizes = !matches!(
            clip.incomplete_why(),
            Some(
                IncompleteWhy::RenderTimeout { .. }
                    | IncompleteWhy::HungOwner
                    | IncompleteWhy::HelperBusy
            )
        );
        let Ok(window) = Window::new() else {
            println!("no window for the listing");
            return;
        };
        let Ok(_open) = Open::new(&window) else {
            println!("listing: clipboard locked");
            return;
        };
        let mut format = 0;
        loop {
            // SAFETY: the clipboard is open on this thread (`_open`).
            format = unsafe {
                SetLastError(0);
                EnumClipboardFormats(format)
            };
            if format == 0 {
                // SAFETY: reads this thread's last-error value, no arguments.
                println!("end of list, last error {}", unsafe { GetLastError() });
                break;
            }
            let size = if sizes && kind_of(format) != Kind::Handle {
                // SAFETY: open on this thread; the handle is only sized, never locked or read.
                unsafe {
                    SetLastError(0);
                    let handle = GetClipboardData(format);
                    if handle.is_null() {
                        format!("no data (error {})", GetLastError())
                    } else {
                        SetLastError(0);
                        format!("{} bytes (error {})", GlobalSize(handle), GetLastError())
                    }
                }
            } else {
                "handle or skipped".into()
            };
            println!("format {format:#06x} '{}' {size}", format_name(format));
        }
    }

    fn kind_of(format: u32) -> Kind {
        kind(format, &[])
    }

    /// The one read-only Win+V check the owner allowed (arch-plan 3.6): entry ids only, no text,
    /// no write. Prints the count of entries and the place of the daemon's newest entry id, read
    /// from the file named in `OXI_WINV_ENTRIES` (`clipboard-entries.txt`).
    /// `cargo test -p oxi-platform --lib probe_winv_place -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads this machine's Win+V entry ids (read only)"]
    fn probe_winv_place() {
        use oxi_core::paste::OurEntries;

        let file = std::env::var_os("OXI_WINV_ENTRIES").expect("set OXI_WINV_ENTRIES");
        let ours = OurEntries::from_text(&std::fs::read_to_string(file).unwrap_or_default());
        match WinClipboard::new().unwrap().history_ids() {
            Ok(Some(list)) => {
                let place = ours
                    .ids()
                    .last()
                    .and_then(|id| list.iter().position(|x| x == id));
                println!(
                    "entries {}, our newest id at {}",
                    list.len(),
                    place.map_or_else(|| "absent".to_owned(), |p| p.to_string())
                );
            }
            Ok(None) => println!("history off"),
            Err(e) => println!("history call failed: {e}"),
        }
    }

    /// Runs the real sequence (6 dictations over a user copy) and prints the Win+V order.
    /// `cargo test -p oxi-platform --lib live_win_v -- --ignored --nocapture`. It overwrites
    /// the clipboard for a few seconds, puts it back, and deletes the entries it made. It adds
    /// two entries (its user copy and one dictation), so it refuses a history with less room:
    /// a full Win+V would push the owner's oldest entries out for good.
    #[test]
    #[allow(clippy::too_many_lines, reason = "a linear live script")]
    #[ignore = "uses this machine's real clipboard and Win+V history"]
    fn live_win_v_order_after_dictations() {
        use oxi_core::config::Paste;
        use oxi_core::hotkey::Key;
        use oxi_core::paste::{
            Foreground, HISTORY_CAP, Input, InputBlocked, OurEntries, paste_with,
        };

        struct Keys;
        impl Input for Keys {
            fn modifiers_held(&self) -> bool {
                false
            }
            fn foreground(&self) -> Foreground {
                Foreground {
                    app: "notepad.exe".into(),
                    elevated: false,
                }
            }
            fn send_chord(&mut self, _: &[Key]) -> Result<(), InputBlocked> {
                Ok(())
            }
            fn type_units(&mut self, _: &[u16], _: Millis) -> Result<(), InputBlocked> {
                Ok(())
            }
            fn sleep(&mut self, time: Millis) {
                sleep(duration(time));
            }
        }

        const MINE: &str = "oxi-live";
        let show = |clip: &mut WinClipboard, label: &str| {
            let items = clip.history().unwrap();
            let lines: Vec<String> = items
                .iter()
                .take(6)
                .enumerate()
                .map(|(i, it)| match &it.text {
                    Some(t) if t.starts_with(MINE) => format!("{}: {t} [{}]", i + 1, &it.id[..8]),
                    Some(t) => format!("{}: <other text, {} bytes>", i + 1, t.len()),
                    None => format!("{}: <non-text>", i + 1),
                })
                .collect();
            println!(
                "{label} ({} entries):
  {}",
                items.len(),
                lines.join(
                    "
  "
                )
            );
        };
        let mut clip = WinClipboard::new().unwrap();
        // Refuses before any write when the current clipboard could not be put back whole.
        let Snap::Complete(original) = clip.snapshot() else {
            panic!("the current clipboard cannot be saved whole; copy plain text and rerun");
        };
        let (seq0, items0) = (clip.sequence(), clip.history().unwrap().len());
        println!("start: sequence {seq0}, history entries {items0}");
        assert!(
            items0 + 2 <= HISTORY_CAP,
            "Win+V has {items0} entries: clear some so 2 more fit, then rerun"
        );
        clip.set_text("oxi-live user copy", true).unwrap();
        sleep(Duration::from_millis(500));
        let (mut ours, cfg) = (OurEntries::default(), Paste::default());
        for n in 1..=6 {
            let started = Instant::now();
            let out = paste_with(
                &format!("oxi-live dictation {n}"),
                &cfg,
                &mut clip,
                &mut Keys,
                &mut ours,
                &mut |_| {},
            );
            println!(
                "dictation {n}: {out:?} in {} ms",
                started.elapsed().as_millis()
            );
            sleep(Duration::from_millis(1000));
        }
        show(&mut clip, "Win+V after 6 dictations");
        let texts: Vec<_> = clip
            .history()
            .unwrap()
            .into_iter()
            .map(|i| i.text)
            .collect();
        let ours = |t: &Option<String>| {
            t.as_deref()
                .is_some_and(|t| t.starts_with("oxi-live dictation"))
        };
        // The current clipboard, read back through the snapshot path.
        if let Snap::Complete(Saved(formats)) = clip.snapshot() {
            let text = formats
                .iter()
                .find(|(f, _)| *f == u32::from(CF_UNICODETEXT));
            let utf16: Vec<u16> = text
                .map(|(_, b)| {
                    b.as_chunks::<2>()
                        .0
                        .iter()
                        .map(|c| u16::from_ne_bytes(*c))
                        .take_while(|u| *u != 0)
                        .collect()
                })
                .unwrap_or_default();
            println!("Ctrl+V gives: {}", String::from_utf16_lossy(&utf16));
        }
        // Clean up: our entries out of history, the original clipboard back.
        for it in clip.history().unwrap() {
            if it.text.as_deref().is_some_and(|t| t.starts_with(MINE)) {
                clip.delete_from_history(&it.id).unwrap();
            }
        }
        // Outside the history: its own entry is still there.
        clip.restore(&original, false).unwrap();
        sleep(Duration::from_millis(500));
        show(&mut clip, "after clean-up");
        // After the clean-up, so a failure here leaves nothing behind on this machine.
        assert_eq!(texts[0].as_deref(), Some("oxi-live dictation 6"));
        assert_eq!(texts[1].as_deref(), Some("oxi-live user copy"));
        assert_eq!(
            texts.len(),
            items0 + 2,
            "an entry was pushed out or left behind"
        );
        assert_eq!(texts.iter().filter(|t| ours(t)).count(), 1);
    }
}
