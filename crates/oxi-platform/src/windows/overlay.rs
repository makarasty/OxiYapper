//! Layered overlay window (ARCHITECTURE section 2, main thread; section 11 items 44-45).
//!
//! A no-activate, click-through, topmost tool window: it never takes focus and is in neither
//! the taskbar nor Alt+Tab. Every frame goes to `UpdateLayeredWindow` from one reused DIB, so
//! there is no `WM_PAINT` and no flicker.

use std::io;
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ab_glyph::FontRef;
use oxi_core::config::Config;
use oxi_core::machine::OverlayState;
use oxi_core::overlay::{
    FINISH_MS, HOLD_MS, METER_HZ, Meter, PILL_W_DIP, ProgressState, Quns, Rect, Smooth,
    pill_accent, place, seed, visible,
};
use oxi_core::types::Millis;
use thiserror::Error;
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION,
    CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetMonitorInfoW,
    HBITMAP, HDC, MONITOR_DEFAULTTOPRIMARY, MONITORINFO, MonitorFromWindow, SelectObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows_sys::Win32::UI::Shell::{
    QUNS_APP, QUNS_BUSY, QUNS_NOT_PRESENT, QUNS_PRESENTATION_MODE, QUNS_QUIET_TIME,
    QUNS_RUNNING_D3D_FULL_SCREEN, SHQueryUserNotificationState,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA, GetForegroundWindow,
    GetWindowLongPtrW, KillTimer, RegisterClassExW, SW_HIDE, SW_SHOWNOACTIVATE, SetTimer,
    SetWindowLongPtrW, ShowWindow, ULW_ALPHA, UpdateLayeredWindow, WM_TIMER, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};
use windows_sys::core::PCWSTR;
use windows_sys::w;

use crate::capture::LevelMeter;
use crate::draw::{PillCanvas, bgra_into};

const CLASS: PCWSTR = w!("OxiYapperOverlay");
const TIMER: usize = 1;
/// Per-pixel alpha from the premultiplied DIB.
#[allow(clippy::cast_possible_truncation)] // the flags are 0 and 1
const BLEND: BLENDFUNCTION = BLENDFUNCTION {
    BlendOp: AC_SRC_OVER as u8,
    BlendFlags: 0,
    SourceConstantAlpha: 255,
    AlphaFormat: AC_SRC_ALPHA as u8,
};
/// More bars than ever fit in the pill; the canvas shows the newest that do.
const BARS: usize = PILL_W_DIP.unsigned_abs() as usize;

#[derive(Debug, Error)]
pub enum OverlayError {
    #[error("{0} failed")]
    Win32(&'static str, #[source] io::Error),
}

/// The progress of the jobs the pill shows (DESIGN-SYSTEM section 26), set from any thread (the
/// coordinator); the pill's timer reads it each frame, as it reads the [`LevelMeter`]. A bar
/// belongs to one pill state: the transcription's to `Transcribing`, the typing's to `Pasting`,
/// so a held Transcribing pill runs its own bar to the end while typing starts the next one
/// (DS-26.5), and a clipboard paste, which has no bar, shows none. No [`Smooth`] for the state
/// shown: the indeterminate sweep. It keeps its own clock, so the coordinator and the pill agree
/// on the time without sharing one.
#[derive(Debug, Clone)]
pub struct Progress(Arc<Mutex<Cell>>);

#[derive(Debug)]
struct Cell {
    epoch: Instant,
    /// `Transcribing`'s bar, then `Pasting`'s.
    bars: [Bar; 2],
}

#[derive(Debug, Default, Clone, Copy)]
struct Bar {
    smooth: Option<Smooth>,
    /// Cleared while it finished: it runs out for the pill held to show it, then goes.
    ended: bool,
}

impl Bar {
    /// What to draw at `now`; a cleared bar that has run out goes.
    fn shown(&mut self, now: Millis) -> Option<u8> {
        let s = self.smooth.as_mut()?;
        if self.ended && s.state(now) == ProgressState::Finished {
            *self = Self::default();
            return None;
        }
        s.shown(now);
        Some(s.percent())
    }

    /// How long its finish still runs and stays full at `now`.
    fn finishing_left(&self, now: Millis) -> Option<Duration> {
        let at = self.smooth.as_ref()?.finishing_since()?;
        Some(Duration::from_millis(
            (at.0 + FINISH_MS + HOLD_MS).saturating_sub(now.0),
        ))
        .filter(|left| !left.is_zero())
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(Cell {
            epoch: Instant::now(),
            bars: [Bar::default(); 2],
        })))
    }
}

/// The bar of pill state `state`; only `Transcribing` and `Pasting` have one.
fn slot(state: OverlayState) -> Option<usize> {
    match state {
        OverlayState::Transcribing => Some(0),
        OverlayState::Pasting => Some(1),
        _ => None,
    }
}

impl Progress {
    fn cell(&self) -> std::sync::MutexGuard<'_, Cell> {
        // A panic while the lock is held leaves a value that is still a valid `Cell`.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Runs `f` on the bar of `state`, if it has one.
    fn with(&self, state: OverlayState, f: impl FnOnce(&mut Smooth)) {
        let mut cell = self.cell();
        if let Some(i) = slot(state)
            && let Some(s) = &mut cell.bars[i].smooth
        {
            f(s);
        }
    }

    /// Milliseconds on this cell's clock.
    #[must_use]
    pub fn now(&self) -> Millis {
        Millis(u64::try_from(self.cell().epoch.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    /// A new job: a new bar from 0 for pill state `state` (DS-26.5). `est`: the expected length
    /// of the whole job. A new transcription also ends the typing bar of the job before.
    pub fn start(&self, state: OverlayState, est: Option<Millis>) {
        self.start_at(state, est, self.now());
    }

    pub fn start_at(&self, state: OverlayState, est: Option<Millis>, now: Millis) {
        let Some(i) = slot(state) else { return };
        let mut cell = self.cell();
        if i == 0 {
            cell.bars[1] = Bar::default();
        }
        cell.bars[i] = Bar {
            smooth: Some(Smooth::start(est, now)),
            ended: false,
        };
    }

    /// A real fraction, 0 to 1, for the bar of `state`; no effect without one.
    pub fn real(&self, state: OverlayState, fraction: f32) {
        self.real_at(state, fraction, self.now());
    }

    pub fn real_at(&self, state: OverlayState, fraction: f32, now: Millis) {
        self.with(state, |s| s.real(fraction, now));
    }

    /// The job of `state` is done: its bar runs to the end quickly.
    pub fn finish(&self, state: OverlayState) {
        self.finish_at(state, self.now());
    }

    pub fn finish_at(&self, state: OverlayState, now: Millis) {
        self.with(state, |s| s.finish(now));
    }

    /// No bars: indeterminate, or no job. A bar still finishing is left to run out, so the pill
    /// held for it is not cut mid-way.
    pub fn clear(&self) {
        self.clear_at(self.now());
    }

    pub fn clear_at(&self, now: Millis) {
        for bar in &mut self.cell().bars {
            if bar.finishing_left(now).is_some() {
                bar.ended = true;
            } else {
                *bar = Bar::default();
            }
        }
    }

    /// Is there a job (a bar not cleared)?
    #[must_use]
    pub fn active(&self) -> bool {
        self.cell()
            .bars
            .iter()
            .any(|b| b.smooth.is_some() && !b.ended)
    }

    /// How long a finished bar still runs to its end and stays full; `None` when none is
    /// finishing. The pill that shows it stays until then.
    #[must_use]
    pub fn finishing_left(&self) -> Option<Duration> {
        let now = self.now();
        self.cell()
            .bars
            .iter()
            .filter_map(|b| b.finishing_left(now))
            .max()
    }

    /// The percent to draw now in pill state `state`; `None` while it is indeterminate.
    #[must_use]
    pub fn get(&self, state: OverlayState) -> Option<u8> {
        self.get_at(state, self.now())
    }

    pub fn get_at(&self, state: OverlayState, now: Millis) -> Option<u8> {
        let i = slot(state)?;
        self.cell().bars[i].shown(now)
    }
}

/// The overlay pill window. Create, show and drop it on the UI thread, whose message loop
/// dispatches the 30 Hz frame timer (meter while recording, progress while busy).
pub struct Overlay<'f> {
    inner: *mut Inner<'f>,
}

struct Inner<'f> {
    hwnd: HWND,
    level: LevelMeter,
    progress: Progress,
    meter: Meter,
    canvas: PillCanvas<'f>,
    state: OverlayState,
    rect: Rect,
    start: Instant,
    dc: HDC,
    dib: Option<Dib>,
    /// Makes `render` fail, to test the error path of `show`.
    #[cfg(test)]
    fail_render: bool,
}

/// A top-down 32-bit DIB selected into `Inner::dc`.
struct Dib {
    bitmap: HBITMAP,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl<'f> Overlay<'f> {
    /// The hidden window. `level` is the capture's meter, `font` the label font (Segoe UI);
    /// without a font the pill shows its icon and meter only. Also makes the process
    /// per-monitor DPI aware (v2), which a manifest may already have done.
    pub fn new(level: LevelMeter, font: Option<FontRef<'f>>) -> Result<Self, OverlayError> {
        // SAFETY: plain Win32 calls with valid arguments; every handle made here is checked and
        // owned by the returned `Overlay`, which frees it in `Drop`.
        unsafe {
            // Fails when the manifest or an earlier call set the awareness: both are fine.
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let instance = GetModuleHandleW(null());
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>().try_into().unwrap_or(u32::MAX),
                lpfnWndProc: Some(wndproc),
                hInstance: instance,
                lpszClassName: CLASS,
                ..Default::default()
            };
            if RegisterClassExW(&raw const class) == 0
                && GetLastError() != ERROR_CLASS_ALREADY_EXISTS
            {
                return Err(last("RegisterClassExW"));
            }
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOOLWINDOW
                    | WS_EX_TOPMOST
                    | WS_EX_NOACTIVATE,
                CLASS,
                null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                null(),
            );
            if hwnd.is_null() {
                return Err(last("CreateWindowExW"));
            }
            let dc = CreateCompatibleDC(null_mut());
            if dc.is_null() {
                let err = last("CreateCompatibleDC");
                DestroyWindow(hwnd);
                return Err(err);
            }
            let inner = Box::into_raw(Box::new(Inner {
                hwnd,
                level,
                progress: Progress::default(),
                meter: Meter::new(BARS, 0.0),
                canvas: PillCanvas::new(font),
                state: OverlayState::Hidden,
                rect: Rect {
                    x: 0,
                    y: 0,
                    w: 0,
                    h: 0,
                },
                start: Instant::now(),
                dc,
                dib: None,
                #[cfg(test)]
                fail_render: false,
            }));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, inner as isize);
            Ok(Self { inner })
        }
    }

    pub fn hwnd(&self) -> HWND {
        // SAFETY: `inner` lives until `Drop`.
        unsafe { (*self.inner).hwnd }
    }

    /// Shows `state` on the monitor of the foreground window, placed and scaled for that
    /// monitor now, without activating. Hides instead for `Hidden`, a disabled overlay, or a
    /// fullscreen app when `hide_over_fullscreen` is set.
    pub fn show(&mut self, state: OverlayState, cfg: &Config) -> Result<(), OverlayError> {
        self.inner().show(state, cfg)
    }

    pub fn hide(&mut self) {
        self.inner().hide();
    }

    /// The cell the coordinator writes whisper's percent into.
    #[must_use]
    pub fn progress(&self) -> Progress {
        // SAFETY: `inner` lives until `Drop`.
        unsafe { (*self.inner).progress.clone() }
    }

    fn inner(&mut self) -> &mut Inner<'f> {
        // SAFETY: `inner` lives until `Drop`; the window procedure borrows it only while a
        // WM_TIMER is dispatched, which never happens inside `show` or `hide` (posted
        // messages are not delivered by the calls they make).
        unsafe { &mut *self.inner }
    }
}

impl Drop for Overlay<'_> {
    fn drop(&mut self) {
        // SAFETY: the window dies first, with its timer, so nothing reaches `inner` after.
        unsafe {
            DestroyWindow((*self.inner).hwnd);
            drop(Box::from_raw(self.inner));
        }
    }
}

impl Inner<'_> {
    fn show(&mut self, state: OverlayState, cfg: &Config) -> Result<(), OverlayError> {
        let shown = self.try_show(state, cfg);
        if shown.is_err() {
            // No stale pill and no frame timer after a failed show. A recording goes on, so the
            // pill's clock keeps its start for the next frame that is drawn.
            if matches!(state, OverlayState::Recording { .. }) {
                self.hide_window();
            } else {
                self.hide();
            }
        }
        shown
    }

    fn try_show(&mut self, state: OverlayState, cfg: &Config) -> Result<(), OverlayError> {
        if state == OverlayState::Hidden || !visible(&cfg.overlay, quns()) {
            self.hide();
            return Ok(());
        }
        let (work, dpi) = foreground_monitor()?;
        // The config comes with every show, so a changed accent lands on the next one (no poll).
        self.canvas.set_accent(pill_accent(seed(cfg.ui.accent)));
        let recording = matches!(state, OverlayState::Recording { .. });
        if recording && !matches!(self.state, OverlayState::Recording { .. }) {
            self.meter = Meter::new(BARS, cfg.audio.meter_sensitivity_db);
        }
        // The meter moves while recording, the progress track while loading or transcribing.
        let animated = recording
            || matches!(
                state,
                OverlayState::LoadingModel | OverlayState::Transcribing | OverlayState::Pasting
            );
        self.state = state;
        self.rect = place(cfg.overlay.position, work, dpi as f32 / 96.0);
        self.canvas.set_progress(self.progress.get(self.state));
        self.render()?;
        // SAFETY: `hwnd` is our live window.
        unsafe {
            ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            if animated {
                #[allow(clippy::cast_possible_truncation)] // 1000 / 30
                SetTimer(self.hwnd, TIMER, (1000 / METER_HZ) as u32, None);
            } else {
                KillTimer(self.hwnd, TIMER);
            }
        }
        Ok(())
    }

    /// The pill is gone because its state ended: the next recording's clock starts at zero.
    fn hide(&mut self) {
        self.hide_window();
        self.canvas.hide();
    }

    /// The window and its timer go; the canvas keeps what it knows.
    fn hide_window(&mut self) {
        self.state = OverlayState::Hidden;
        // SAFETY: `hwnd` is our live window.
        unsafe {
            KillTimer(self.hwnd, TIMER);
            ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn tick(&mut self) {
        if matches!(self.state, OverlayState::Recording { .. }) {
            self.meter.push(self.level.read(), self.now());
        }
        self.canvas.set_progress(self.progress.get(self.state));
        // A failed frame keeps the last one on screen; the next tick draws again.
        let _ = self.render();
    }

    fn now(&self) -> Millis {
        Millis(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    /// Draws the pill into the DIB and hands it to the window, which also moves it to `rect`.
    /// After the first frame of a state it allocates nothing: the canvas and the DIB are reused.
    fn render(&mut self) -> Result<(), OverlayError> {
        #[cfg(test)]
        if self.fail_render {
            return Err(OverlayError::Win32("render", io::Error::other("injected")));
        }
        let now = self.now();
        let Some(pm) = self.canvas.draw(self.state, &self.meter, now, self.rect) else {
            return Ok(());
        };
        let Rect { x, y, w, h } = self.rect;
        bgra_into(pm, dib(&mut self.dib, self.dc, w, h)?);
        // SAFETY: `dc` holds a `w` x `h` DIB; every pointer is to a live local.
        let ok = unsafe {
            UpdateLayeredWindow(
                self.hwnd,
                null_mut(),
                &POINT { x, y },
                &SIZE { cx: w, cy: h },
                self.dc,
                &POINT { x: 0, y: 0 },
                0,
                &BLEND,
                ULW_ALPHA,
            )
        };
        if ok == 0 {
            return Err(last("UpdateLayeredWindow"));
        }
        Ok(())
    }
}

/// The pixels of a `w` x `h` DIB selected into `dc`, made anew only when the size changes.
fn dib(slot: &mut Option<Dib>, dc: HDC, w: i32, h: i32) -> Result<&mut [u8], OverlayError> {
    if slot.as_ref().is_none_or(|d| (d.w, d.h) != (w, h)) {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>().try_into().unwrap_or(u32::MAX),
                biWidth: w,
                biHeight: -h, // top-down, the row order of a pixmap
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits = null_mut();
        // SAFETY: `info` and `bits` are live locals; the old bitmap is deleted only after the
        // new one replaced it in `dc`.
        unsafe {
            let bitmap = CreateDIBSection(
                dc,
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits,
                null_mut(),
                0,
            );
            if bitmap.is_null() {
                return Err(last("CreateDIBSection"));
            }
            SelectObject(dc, bitmap);
            let new = Dib {
                bitmap,
                bits: bits.cast(),
                w,
                h,
            };
            if let Some(old) = slot.replace(new) {
                DeleteObject(old.bitmap);
            }
        }
    }
    let d = slot.as_ref().ok_or_else(|| last("CreateDIBSection"))?;
    let len = usize::try_from(d.w).unwrap_or(0) * usize::try_from(d.h).unwrap_or(0) * 4;
    // SAFETY: a w x h 32-bit DIB has exactly `len` bytes at `bits`, alive while it is
    // selected; the `&mut` slot keeps every other writer out.
    Ok(unsafe { std::slice::from_raw_parts_mut(d.bits, len) })
}

impl Drop for Inner<'_> {
    fn drop(&mut self) {
        // SAFETY: our own DC and bitmap; the DC goes first so the bitmap is no longer selected.
        unsafe {
            DeleteDC(self.dc);
            if let Some(d) = &self.dib {
                DeleteObject(d.bitmap);
            }
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_TIMER && wp == TIMER {
        // SAFETY: GWLP_USERDATA holds this window's `Inner` from `Overlay::new` until the window
        // is destroyed in `Overlay::drop`; see `Overlay::inner` for why no other borrow is live.
        let inner = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Inner<'_>;
        // SAFETY: as above; null only before `new` stored it.
        if let Some(inner) = unsafe { inner.as_mut() } {
            inner.tick();
            return 0;
        }
    }
    // SAFETY: the default handling for our own window.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// The work area (taskbar excluded) and effective DPI of the foreground window's monitor; the
/// primary monitor when nothing is in the foreground.
fn foreground_monitor() -> Result<(Rect, u32), OverlayError> {
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>().try_into().unwrap_or(u32::MAX),
        ..Default::default()
    };
    let (mut dpi, mut dpi_y) = (96, 96);
    // SAFETY: out pointers are live locals; MonitorFromWindow accepts a null window.
    unsafe {
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTOPRIMARY);
        if GetMonitorInfoW(monitor, &raw mut info) == 0 {
            return Err(last("GetMonitorInfoW"));
        }
        // A failure leaves 96, the unscaled size.
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &raw mut dpi, &raw mut dpi_y);
    }
    let RECT {
        left,
        top,
        right,
        bottom,
    } = info.rcWork;
    let work = Rect {
        x: left,
        y: top,
        w: right - left,
        h: bottom - top,
    };
    Ok((work, dpi))
}

/// `SHQueryUserNotificationState`; a failed query counts as a normal desktop.
fn quns() -> Quns {
    let mut state = 0;
    // SAFETY: `state` is a live out pointer.
    if unsafe { SHQueryUserNotificationState(&raw mut state) } < 0 {
        return Quns::AcceptsNotifications;
    }
    match state {
        QUNS_NOT_PRESENT => Quns::NotPresent,
        QUNS_BUSY => Quns::Busy,
        QUNS_RUNNING_D3D_FULL_SCREEN => Quns::D3dFullScreen,
        QUNS_PRESENTATION_MODE => Quns::PresentationMode,
        QUNS_QUIET_TIME => Quns::QuietTime,
        QUNS_APP => Quns::App,
        _ => Quns::AcceptsNotifications,
    }
}

fn last(call: &'static str) -> OverlayError {
    OverlayError::Win32(call, io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    use super::*;

    #[test]
    fn progress_is_unknown_until_started_and_shared_by_its_clones() {
        let t = OverlayState::Transcribing;
        let cell = Progress::default();
        let reader = cell.clone();
        assert_eq!(reader.get_at(t, Millis(0)), None, "no job is indeterminate");
        cell.start_at(t, Some(Millis(4000)), Millis(0));
        assert!(reader.active());
        let mut last = 0;
        for at in (100..=3000).step_by(100) {
            let p = reader.get_at(t, Millis(at)).unwrap();
            assert!(p >= last && p < 100, "{at}: {p}");
            last = p;
        }
        assert!(
            last > 30,
            "the estimate moves the bar with no report: {last}"
        );
        cell.real_at(t, 1.0, Millis(3000));
        cell.finish_at(t, Millis(3000));
        assert_eq!(reader.get_at(t, Millis(3000 + FINISH_MS)), Some(100));
        cell.clear_at(Millis(4000));
        assert_eq!(reader.get_at(t, Millis(4000)), None);
    }

    /// Review c04 #3, #4, #5: each pill state reads its own bar, and a clear leaves a finishing
    /// bar to run out.
    #[test]
    fn transcription_and_typing_bars_never_show_in_each_other_pill() {
        let (t, p) = (OverlayState::Transcribing, OverlayState::Pasting);
        let cell = Progress::default();
        cell.start_at(t, Some(Millis(1000)), Millis(0));
        cell.finish_at(t, Millis(1000));
        // #4: a clipboard paste has no bar: the Pasting pill shows no percent, not 100.
        assert_eq!(cell.get_at(p, Millis(1000 + FINISH_MS + HOLD_MS)), None);
        // #3: typing starts its bar while the Transcribing pill is held: that pill still runs
        // to 100, the typing bar starts from 0.
        cell.start_at(p, Some(Millis(2000)), Millis(1050));
        assert_eq!(cell.get_at(t, Millis(1000 + FINISH_MS)), Some(100));
        assert!(cell.get_at(p, Millis(1060)).unwrap() < 10);
        // #5: cleared mid-finish, the transcription bar runs out; a running bar goes at once.
        let cell = Progress::default();
        cell.start_at(t, Some(Millis(1000)), Millis(0));
        cell.finish_at(t, Millis(500));
        cell.clear_at(Millis(520));
        assert!(!cell.active());
        assert!(cell.get_at(t, Millis(520 + FINISH_MS)).is_some());
        assert_eq!(cell.get_at(t, Millis(500 + FINISH_MS + HOLD_MS)), None);
        cell.start_at(p, None, Millis(0));
        cell.clear_at(Millis(10));
        assert_eq!(cell.get_at(p, Millis(10)), None);
    }

    /// Live: shows a window. Runs only with `OXI_OVERLAY_TEST=1`.
    #[test]
    fn a_failed_show_hides_and_stops_the_meter_timer() {
        if std::env::var_os("OXI_OVERLAY_TEST").is_none_or(|v| v != "1") {
            eprintln!("skipped: set OXI_OVERLAY_TEST=1");
            return;
        }
        let cfg = Config::default();
        let mut overlay = Overlay::new(LevelMeter::default(), None).unwrap();
        let recording = OverlayState::Recording {
            locked: false,
            ending_soon: false,
        };
        overlay.show(recording, &cfg).unwrap();
        overlay.inner().fail_render = true;
        assert!(overlay.show(OverlayState::Transcribing, &cfg).is_err());
        let inner = overlay.inner();
        assert_eq!(inner.state, OverlayState::Hidden);
        // SAFETY: our live window; KillTimer returns 0 when no such timer runs.
        unsafe {
            assert_eq!(IsWindowVisible(inner.hwnd), 0, "a stale pill stays up");
            assert_eq!(
                KillTimer(inner.hwnd, TIMER),
                0,
                "the meter timer still runs"
            );
        }
    }
}
