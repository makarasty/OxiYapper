//! Tray icon, menu and balloons via `Shell_NotifyIconW`, and the main (UI) thread message loop
//! (ARCHITECTURE section 2; section 11 items 42, 46 and 47).
//!
//! One hidden top-level window owns the icon. It also receives `TaskbarCreated` (Explorer
//! restarted), `WM_POWERBROADCAST` and `WM_WTSSESSION_CHANGE`; a message-only window would miss
//! these broadcasts. The overlay window lives on the same thread and shares the loop.

use std::any::Any;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::panic;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use oxi_core::APP_NAME;
use oxi_core::machine::{Event, Notice, Phase};
use oxi_core::strings;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::RemoteDesktop::{
    NOTIFY_FOR_THIS_SESSION, WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_MESSAGE,
    NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIcon, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetMessageW, HICON, MF_GRAYED, MF_STRING, MSG,
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, RegisterClassExW, RegisterWindowMessageW,
    SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_NULL, WM_POWERBROADCAST, WM_WTSSESSION_CHANGE,
    WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP, WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use crate::draw::tray_icon;

/// The [`Waker::wake`] message.
const WM_WAKE: u32 = WM_APP;
/// The icon's callback message.
const WM_TRAY: u32 = WM_APP + 1;
/// The [`Waker::quit`] message.
const WM_EXIT: u32 = WM_APP + 2;
const ICON_ID: u32 = 1;

/// A tray menu entry; the discriminant is its menu command id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Settings = 1,
    CopyLast = 2,
    Quit = 3,
}

/// Wakes or ends [`MainLoop::run`] from any thread.
#[derive(Debug, Clone)]
pub struct Waker(Arc<Mutex<Option<usize>>>);

impl Waker {
    /// `on_wake` runs on the UI thread. Fails with `NotConnected` once the loop is dropped.
    pub fn wake(&self) -> io::Result<()> {
        self.post(WM_WAKE)
    }

    /// [`MainLoop::run`] returns once the messages before this one are handled.
    pub fn quit(&self) -> io::Result<()> {
        self.post(WM_EXIT)
    }

    fn post(&self, msg: u32) -> io::Result<()> {
        // Held while posting: Drop clears the handle under the same lock before destroying the
        // window, so a recycled handle never gets our message.
        let slot = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let addr = slot.ok_or(io::ErrorKind::NotConnected)?;
        // SAFETY: the window is alive while the slot holds its handle.
        if unsafe { PostMessageW(ptr::with_exposed_provenance_mut(addr), msg, 0, 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// What the window procedure saw; [`MainLoop::run`] handles it outside the procedure, so no
/// callback runs re-entrantly inside a Win32 call.
enum Incoming {
    Wake,
    Menu { x: i32, y: i32 },
    TaskbarCreated,
    Exit,
}

type OnSession = Box<dyn FnMut(Event)>;

thread_local! {
    static INBOX: RefCell<VecDeque<Incoming>> = const { RefCell::new(VecDeque::new()) };
    /// `on_session` while [`MainLoop::run`] runs: the window procedure calls it directly.
    static SESSION: RefCell<Option<OnSession>> = const { RefCell::new(None) };
    /// Session events not yet given to `on_session`, in arrival order.
    static PENDING: RefCell<VecDeque<Event>> = const { RefCell::new(VecDeque::new()) };
    /// A panic of `on_session` caught in the window procedure; `run` rethrows it.
    static PANIC: RefCell<Option<Box<dyn Any + Send>>> = const { RefCell::new(None) };
}

/// Clears [`SESSION`] when `run` ends, also by panic.
struct SessionGuard;

impl Drop for SessionGuard {
    fn drop(&mut self) {
        SESSION.with_borrow_mut(|f| *f = None);
    }
}

/// Gives the pending session events to `on_session`, oldest first. They stay pending while it
/// is not installed (before `run`), after a panic, or while it runs: a re-entrant call returns
/// at once and the outer call delivers them next, so the order holds.
fn flush() {
    SESSION.with(|f| {
        let Ok(mut f) = f.try_borrow_mut() else {
            return;
        };
        let Some(f) = f.as_mut() else { return };
        while PANIC.with_borrow(Option::is_none)
            && let Some(event) = PENDING.with_borrow_mut(VecDeque::pop_front)
        {
            f(event);
        }
    });
}

/// Rethrows a panic caught in the window procedure, then delivers pending session events.
fn settle() {
    if let Some(payload) = PANIC.with_borrow_mut(Option::take) {
        panic::resume_unwind(payload);
    }
    flush();
}

/// Window class atom, 0 when registration failed.
static CLASS: OnceLock<u16> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

/// The hidden tray window and its icon. Create, use and drop it on the UI thread.
#[derive(Debug)]
pub struct MainLoop {
    hwnd: HWND,
    phase: Phase,
    icon: HICON,
    copy_last: bool,
    /// Status warnings, one tooltip line each under the phase.
    warnings: Vec<String>,
    /// The handle every [`Waker`] posts to; `None` once dropped.
    slot: Arc<Mutex<Option<usize>>>,
}

impl MainLoop {
    /// Creates the window, subscribes to session events and adds the Idle icon.
    pub fn new() -> io::Result<Self> {
        // SAFETY: a null name asks for the module of the running executable.
        let instance = unsafe { GetModuleHandleW(ptr::null()) };
        let atom = *CLASS.get_or_init(|| {
            let name = wide(APP_NAME);
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>().try_into().unwrap_or(u32::MAX),
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: name.as_ptr(),
                ..Default::default()
            };
            // SAFETY: `class` and the name it points to outlive the call; Windows copies both.
            unsafe { RegisterClassExW(&raw const class) }
        });
        if atom == 0 {
            return Err(io::Error::other("RegisterClassExW failed"));
        }
        TASKBAR_CREATED.get_or_init(|| {
            // SAFETY: a NUL-terminated string that outlives the call.
            unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) }
        });
        INBOX.with_borrow_mut(VecDeque::clear);
        PENDING.with_borrow_mut(VecDeque::clear);
        PANIC.with_borrow_mut(|p| *p = None);
        let title = wide(APP_NAME);
        // SAFETY: the class atom stands for the class name (MAKEINTATOM); every pointer is valid
        // for the call. Never shown: no WS_VISIBLE, and a tool window has no taskbar button.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW,
                ptr::without_provenance(usize::from(atom)),
                title.as_ptr(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                ptr::null_mut(),
                ptr::null_mut(),
                instance,
                ptr::null(),
            )
        };
        if hwnd.is_null() {
            return Err(io::Error::last_os_error());
        }
        // From here Drop removes whatever was set up.
        let mut this = Self {
            hwnd,
            phase: Phase::Idle,
            icon: ptr::null_mut(),
            copy_last: false,
            warnings: Vec::new(),
            slot: Arc::new(Mutex::new(Some(hwnd.expose_provenance()))),
        };
        // SAFETY: `hwnd` is our live window.
        if unsafe { WTSRegisterSessionNotification(hwnd, NOTIFY_FOR_THIS_SESSION) } == 0 {
            return Err(io::Error::last_os_error());
        }
        this.add()?;
        Ok(this)
    }

    #[must_use]
    pub fn waker(&self) -> Waker {
        Waker(Arc::clone(&self.slot))
    }

    /// The hidden window, for owning other windows and for posting test messages.
    #[must_use]
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Shows the icon and tooltip of `phase`.
    pub fn set_phase(&mut self, phase: Phase) -> io::Result<()> {
        let icon = self.render(phase)?;
        let old = std::mem::replace(&mut self.icon, icon);
        self.phase = phase;
        let shown = shell(NIM_MODIFY, &self.data(NIF_ICON | NIF_TIP | NIF_SHOWTIP));
        destroy_icon(old);
        shown
    }

    /// Shows `warnings` (config corrections, a GPU fallback, a parse error) in the tooltip.
    pub fn set_warnings(&mut self, warnings: &[String]) -> io::Result<()> {
        warnings.clone_into(&mut self.warnings);
        shell(NIM_MODIFY, &self.data(NIF_TIP | NIF_SHOWTIP))
    }

    /// Enables "Copy last text" in the menu (item 42: there is a last text in memory).
    pub fn set_copy_last(&mut self, enabled: bool) {
        self.copy_last = enabled;
    }

    /// A balloon (a toast on Windows 10 and later) with the full text of `notice`.
    pub fn notify(&self, notice: Notice) -> io::Result<()> {
        let mut data = self.data(NIF_INFO);
        copy_wide(&mut data.szInfoTitle, APP_NAME);
        copy_wide(&mut data.szInfo, strings::notice(notice));
        // The daemon plays its own sounds.
        data.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
        shell(NIM_MODIFY, &data)
    }

    /// Runs the thread's message loop until [`Waker::quit`]. `on_wake` runs for each
    /// [`Waker::wake`] and `on_menu` for the chosen menu entry, both from the loop.
    ///
    /// `on_session` gets Suspend, Resume, Lock and Unlock in arrival order. When it is not
    /// already running, it runs inside the window procedure before the reply: Windows sends
    /// `PBT_APMSUSPEND` and sleeps about 2 s later whether or not the reply came, so a queued
    /// Suspend could arrive only after resume (item 46). Rules for `on_session`:
    /// - Keep it bounded: forward the event.
    /// - Never pump messages in it (message box, cross-thread `SendMessage`): a Suspend that
    ///   arrives meanwhile waits until it returns, possibly until after resume.
    /// - It can run re-entrantly inside a modal loop that `on_wake` or `on_menu` starts (menu,
    ///   message box), so never take a lock they hold.
    /// - Its panic leaves `run` as a panic once the window procedure returned. In release
    ///   builds (`panic = "abort"`) the panic aborts the process and Drop does not run.
    ///
    /// On [`Waker::quit`], pending session events are delivered and the wakes, menu requests and
    /// `TaskbarCreated` queued behind it are dropped: they belong to the ended run, and a later
    /// `run` would replay them out of date. Each `run` checks that the icon is shown first.
    pub fn run(
        &mut self,
        mut on_wake: impl FnMut(&mut Self),
        mut on_menu: impl FnMut(&mut Self, MenuItem),
        on_session: impl FnMut(Event) + 'static,
    ) {
        SESSION.with_borrow_mut(|f| *f = Some(Box::new(on_session)));
        let _clear = SessionGuard;
        // A TaskbarCreated dropped by an earlier quit may have left no icon.
        let shown = shell(NIM_MODIFY, &self.data(NIF_ICON | NIF_TIP | NIF_SHOWTIP));
        if let Err(e) = shown.or_else(|_| self.add()) {
            tracing::warn!("tray icon add failed: {e}");
        }
        // Events seen before `run` come before any sent while it pumps.
        flush();
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid out-pointer; a null window takes every message of the thread.
        // GetMessageW returns 0 on WM_QUIT and -1 on error; both end the loop.
        while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {
            // SAFETY: `msg` was filled by GetMessageW.
            unsafe {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
            settle();
            while let Some(incoming) = INBOX.with_borrow_mut(VecDeque::pop_front) {
                match incoming {
                    Incoming::Wake => on_wake(self),
                    Incoming::Exit => {
                        INBOX.with_borrow_mut(VecDeque::clear);
                        settle();
                        return;
                    }
                    Incoming::Menu { x, y } => match self.menu(x, y) {
                        Ok(Some(item)) => on_menu(self, item),
                        Ok(None) => {}
                        Err(e) => tracing::warn!("tray menu failed: {e}"),
                    },
                    Incoming::TaskbarCreated => {
                        if let Err(e) = self.add() {
                            tracing::warn!("tray icon re-add failed: {e}");
                        }
                    }
                }
                // A callback's modal loop may have caught a panic or queued session events.
                settle();
            }
        }
    }

    /// Adds the icon of the current phase; Explorer forgets it when it restarts.
    fn add(&mut self) -> io::Result<()> {
        let icon = self.render(self.phase)?;
        destroy_icon(std::mem::replace(&mut self.icon, icon));
        let mut data = self.data(NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP);
        data.uCallbackMessage = WM_TRAY;
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // A TaskbarCreated that did not lose the icon (DPI change) would make NIM_ADD fail.
        let _ = shell(NIM_DELETE, &data);
        shell(NIM_ADD, &data)?;
        // Version 4: the callback carries the event in LOWORD(lParam), the anchor in wParam.
        shell(NIM_SETVERSION, &data)
    }

    /// The icon of `phase` at the small-icon size of the window's DPI.
    // shortcut: the size is read when the icon is drawn, so a DPI change shows at the next phase.
    fn render(&self, phase: Phase) -> io::Result<HICON> {
        // SAFETY: `hwnd` is our live window; both calls only read.
        let side = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, GetDpiForWindow(self.hwnd)) };
        let pixmap = u32::try_from(side)
            .ok()
            .and_then(|s| tray_icon(phase, s))
            .ok_or_else(|| io::Error::other("tray icon size"))?;
        // Icons take straight (not premultiplied) alpha, BGRA, top row first.
        let colour: Vec<u8> = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.blue(), c.green(), c.red(), c.alpha()]
            })
            .collect();
        // The AND mask is unused with 32-bit alpha but required: rows padded to 16 bits.
        let rows = usize::try_from(side).unwrap_or(0);
        let mask = vec![0u8; rows.div_ceil(16) * 2 * rows];
        // SAFETY: both buffers hold `side` rows of the sizes CreateIcon reads for 1 plane,
        // 32 bits per pixel, and outlive the call; Windows copies them.
        let icon = unsafe {
            CreateIcon(
                ptr::null_mut(),
                side,
                side,
                1,
                32,
                mask.as_ptr(),
                colour.as_ptr(),
            )
        };
        if icon.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(icon)
    }

    /// Shows the menu at the anchor and waits for a choice; `None` when dismissed.
    fn menu(&self, x: i32, y: i32) -> io::Result<Option<MenuItem>> {
        // SAFETY: no arguments; the menu is destroyed below.
        let menu = unsafe { CreatePopupMenu() };
        if menu.is_null() {
            return Err(io::Error::last_os_error());
        }
        let entries = [
            (MenuItem::Settings, strings::MENU_SETTINGS, true),
            (MenuItem::CopyLast, strings::MENU_COPY_LAST, self.copy_last),
            (MenuItem::Quit, strings::MENU_QUIT, true),
        ];
        for (item, text, enabled) in entries {
            let flags = if enabled {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            // SAFETY: `menu` is live and the text outlives the call (Windows copies it).
            unsafe { AppendMenuW(menu, flags, item as usize, wide(text).as_ptr()) };
        }
        // SAFETY: `hwnd` and `menu` are live. The window must be foreground or the menu does
        // not close on a click outside it; WM_NULL afterwards is the documented companion.
        let chosen = unsafe {
            SetForegroundWindow(self.hwnd);
            let id = TrackPopupMenu(
                menu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY,
                x,
                y,
                0,
                self.hwnd,
                ptr::null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(menu);
            id
        };
        Ok([MenuItem::Settings, MenuItem::CopyLast, MenuItem::Quit]
            .into_iter()
            .find(|&item| item as i32 == chosen))
    }

    /// The icon's identity plus `flags`, with the current icon and tooltip filled in.
    fn data(&self, flags: NOTIFY_ICON_DATA_FLAGS) -> NOTIFYICONDATAW {
        let mut data = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>().try_into().unwrap_or(u32::MAX),
            hWnd: self.hwnd,
            uID: ICON_ID,
            uFlags: flags,
            hIcon: self.icon,
            ..Default::default()
        };
        copy_wide(&mut data.szTip, &tooltip(self.phase, &self.warnings));
        data
    }
}

impl Drop for MainLoop {
    fn drop(&mut self) {
        // First, under the lock: no Waker posts to this handle once the window is gone.
        *self.slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
        let _ = shell(NIM_DELETE, &self.data(0));
        // SAFETY: our window, on its own thread (MainLoop is not Send); a failed unregister
        // (never registered) is harmless.
        unsafe {
            WTSUnRegisterSessionNotification(self.hwnd);
            DestroyWindow(self.hwnd);
        }
        destroy_icon(self.icon);
    }
}

/// Maps a power or session message to the machine event (items 46 and 47).
fn session_event(msg: u32, wparam: WPARAM) -> Option<Event> {
    match (msg, u32::try_from(wparam).ok()?) {
        (WM_POWERBROADCAST, PBT_APMSUSPEND) => Some(Event::Suspend),
        // Sent on every resume, also when no user is present (PBT_APMRESUMESUSPEND is not).
        (WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC) => Some(Event::Resume),
        (WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK) => Some(Event::Lock),
        (WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK) => Some(Event::Unlock),
        _ => None,
    }
}

/// Low and high 16-bit halves of a message parameter, sign-extended (`GET_X_LPARAM`).
fn halves(bytes: &[u8]) -> (i16, i16) {
    (
        i16::from_le_bytes([bytes[0], bytes[1]]),
        i16::from_le_bytes([bytes[2], bytes[3]]),
    )
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let incoming = match msg {
        WM_WAKE => Some(Incoming::Wake),
        WM_EXIT => Some(Incoming::Exit),
        WM_TRAY => {
            let (event, _) = halves(&lparam.to_le_bytes());
            let (x, y) = halves(&wparam.to_le_bytes());
            (i32::from(event) == WM_CONTEXTMENU.cast_signed()).then_some(Incoming::Menu {
                x: x.into(),
                y: y.into(),
            })
        }
        WM_POWERBROADCAST | WM_WTSSESSION_CHANGE => {
            if let Some(event) = session_event(msg, wparam) {
                PENDING.with_borrow_mut(|q| q.push_back(event));
                // Unwinding cannot cross this `extern "system"` frame: keep the panic for `run`.
                if let Err(payload) = panic::catch_unwind(flush) {
                    PANIC.with_borrow_mut(|p| *p = Some(payload));
                }
                nudge(hwnd);
            }
            None
        }
        m if TASKBAR_CREATED.get() == Some(&m) => Some(Incoming::TaskbarCreated),
        _ => None,
    };
    if let Some(incoming) = incoming {
        INBOX.with_borrow_mut(|q| q.push_back(incoming));
        nudge(hwnd);
    }
    // Default handling suits every message above (TRUE for WM_POWERBROADCAST, 0 otherwise).
    // SAFETY: the arguments Windows passed to this procedure.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A sent message (`PBT_APMSUSPEND`, `TaskbarCreated`, the shell's callback) is handled inside
/// `GetMessageW`, which then keeps waiting: a posted `WM_NULL` makes it return, so `run` drains
/// the inbox and rethrows a caught panic.
fn nudge(hwnd: HWND) {
    // SAFETY: `hwnd` is the window the procedure runs for.
    unsafe { PostMessageW(hwnd, WM_NULL, 0, 0) };
}

fn shell(message: NOTIFY_ICON_MESSAGE, data: &NOTIFYICONDATAW) -> io::Result<()> {
    // SAFETY: `data` is a fully initialised NOTIFYICONDATAW with its own cbSize.
    if unsafe { Shell_NotifyIconW(message, data) } == 0 {
        // Shell_NotifyIconW sets no last error.
        return Err(io::Error::other(format!(
            "Shell_NotifyIconW({message}) failed"
        )));
    }
    Ok(())
}

fn destroy_icon(icon: HICON) {
    if !icon.is_null() {
        // SAFETY: an icon we created and no longer hand to the shell.
        unsafe { DestroyIcon(icon) };
    }
}

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// The phase line, then one line per warning; Windows cuts the tip at 127 characters.
fn tooltip(phase: Phase, warnings: &[String]) -> String {
    let mut tip = strings::tray_tooltip(phase);
    for w in warnings {
        tip.push('\n');
        tip.push_str(w);
    }
    tip
}

/// Copies `s` into a fixed UTF-16 field, cut to fit and always NUL-terminated.
fn copy_wide(dst: &mut [u16], s: &str) {
    let room = dst.len().saturating_sub(1);
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(room)) {
        *d = c;
        n += 1;
    }
    // A cut between the halves of a surrogate pair leaves a lone high surrogate: drop it.
    if n > 0 && n == room && (0xD800..0xDC00).contains(&dst[n - 1]) {
        n -= 1;
    }
    dst[n..].fill(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_wide_never_splits_a_surrogate_pair() {
        let mut dst = [0xFFFFu16; 8];
        // 6 units, then an emoji (2 units) that would straddle the 7-unit room.
        copy_wide(&mut dst, "abcdef\u{1F600}");
        assert_eq!(String::from_utf16(&dst[..6]).unwrap(), "abcdef");
        assert_eq!(dst[6..], [0, 0]);
        // An emoji that fits whole stays.
        copy_wide(&mut dst, "abcde\u{1F600}");
        assert_eq!(String::from_utf16_lossy(&dst[..7]), "abcde\u{1F600}");
        assert_eq!(dst[7], 0);
    }

    #[test]
    fn tooltip_lists_each_warning_under_the_phase() {
        assert_eq!(
            tooltip(Phase::Idle, &[]),
            strings::tray_tooltip(Phase::Idle)
        );
        let warnings = [
            "running on CPU: out of memory".to_owned(),
            "bad line".to_owned(),
        ];
        assert_eq!(
            tooltip(Phase::Recording, &warnings),
            format!(
                "{}\nrunning on CPU: out of memory\nbad line",
                strings::tray_tooltip(Phase::Recording)
            )
        );
    }

    #[test]
    fn a_long_tooltip_is_cut_and_terminated() {
        let mut tip = [1u16; 8];
        copy_wide(&mut tip, &tooltip(Phase::Idle, &["x".repeat(50)]));
        assert_eq!(tip[7], 0);
        assert_eq!(String::from_utf16_lossy(&tip[..7]), "OxiYapp");
    }
}
