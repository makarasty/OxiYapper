//! Tray icon, menu and balloons via `Shell_NotifyIconW`, and the main (UI) thread message loop
//! (ARCHITECTURE section 2; section 11 items 42, 46 and 47).
//!
//! One hidden top-level window owns the icon. It also receives `TaskbarCreated` (Explorer
//! restarted), `WM_POWERBROADCAST` and `WM_WTSSESSION_CHANGE`; a message-only window would miss
//! these broadcasts. The overlay window lives on the same thread and shares the loop.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io;
use std::ptr;
use std::sync::OnceLock;

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
    PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND, PostMessageW, PostQuitMessage, RegisterClassExW,
    RegisterWindowMessageW, SM_CXSMICON, SetForegroundWindow, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenu, TranslateMessage, WM_APP, WM_CONTEXTMENU, WM_NULL,
    WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP,
    WTS_SESSION_LOCK, WTS_SESSION_UNLOCK,
};

use crate::draw::tray_icon;

/// The [`Waker`] message.
const WM_WAKE: u32 = WM_APP;
/// The icon's callback message.
const WM_TRAY: u32 = WM_APP + 1;
const ICON_ID: u32 = 1;

/// A tray menu entry; the discriminant is its menu command id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuItem {
    Settings = 1,
    CopyLast = 2,
    Quit = 3,
}

/// Wakes [`MainLoop::run`] from any thread: `on_wake` runs on the UI thread.
#[derive(Debug, Clone)]
pub struct Waker(usize);

impl Waker {
    /// Fails once the loop's window is gone.
    pub fn wake(&self) -> io::Result<()> {
        let hwnd: HWND = ptr::with_exposed_provenance_mut(self.0);
        // SAFETY: PostMessageW takes any handle value and fails on a dead window.
        if unsafe { PostMessageW(hwnd, WM_WAKE, 0, 0) } == 0 {
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
    Session(Event),
    TaskbarCreated,
}

thread_local! {
    static INBOX: RefCell<VecDeque<Incoming>> = const { RefCell::new(VecDeque::new()) };
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
        Waker(self.hwnd.expose_provenance())
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

    /// Ends [`run`](Self::run) once the current message is handled.
    pub fn quit() {
        // SAFETY: no arguments to check; it posts WM_QUIT to the calling thread.
        unsafe { PostQuitMessage(0) }
    }

    /// Runs the thread's message loop until [`quit`](Self::quit). `on_wake` runs for each
    /// [`Waker::wake`], `on_menu` for the chosen menu entry, `on_session` for Suspend, Resume,
    /// Lock and Unlock.
    pub fn run(
        &mut self,
        mut on_wake: impl FnMut(&mut Self),
        mut on_menu: impl FnMut(&mut Self, MenuItem),
        mut on_session: impl FnMut(Event),
    ) {
        let mut msg = MSG::default();
        // SAFETY: `msg` is a valid out-pointer; a null window takes every message of the thread.
        // GetMessageW returns 0 on WM_QUIT and -1 on error; both end the loop.
        while unsafe { GetMessageW(&raw mut msg, ptr::null_mut(), 0, 0) } > 0 {
            // SAFETY: `msg` was filled by GetMessageW.
            unsafe {
                TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
            while let Some(incoming) = INBOX.with_borrow_mut(VecDeque::pop_front) {
                match incoming {
                    Incoming::Wake => on_wake(self),
                    Incoming::Session(event) => on_session(event),
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
        copy_wide(&mut data.szTip, &strings::tray_tooltip(self.phase));
        data
    }
}

impl Drop for MainLoop {
    fn drop(&mut self) {
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
        WM_TRAY => {
            let (event, _) = halves(&lparam.to_le_bytes());
            let (x, y) = halves(&wparam.to_le_bytes());
            (i32::from(event) == WM_CONTEXTMENU.cast_signed()).then_some(Incoming::Menu {
                x: x.into(),
                y: y.into(),
            })
        }
        WM_POWERBROADCAST | WM_WTSSESSION_CHANGE => {
            session_event(msg, wparam).map(Incoming::Session)
        }
        m if TASKBAR_CREATED.get() == Some(&m) => Some(Incoming::TaskbarCreated),
        _ => None,
    };
    if let Some(incoming) = incoming {
        INBOX.with_borrow_mut(|q| q.push_back(incoming));
    }
    // Default handling suits every message above (TRUE for WM_POWERBROADCAST, 0 otherwise).
    // SAFETY: the arguments Windows passed to this procedure.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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

/// Copies `s` into a fixed UTF-16 field, cut to fit and always NUL-terminated.
fn copy_wide(dst: &mut [u16], s: &str) {
    let room = dst.len().saturating_sub(1);
    let mut n = 0;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(room)) {
        *d = c;
        n += 1;
    }
    dst[n..].fill(0);
}
