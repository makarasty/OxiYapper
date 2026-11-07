//! Audio page: microphone, live meter, DSP switches, tail, recording cap and language.
//!
//! The meter opens its own capture while the page is shown (decision D3) and polls it at
//! [`METER_HZ`] through the core [`Meter`]; leaving the page or closing the window closes it.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use oxi_core::audio::db_to_linear;
use oxi_core::config::{Audio, Config};
use oxi_core::overlay::{METER_HZ, Meter};
use oxi_core::strings::{self, NO_MIC};
use oxi_core::types::{Language, Millis};
use oxi_platform::capture::{
    Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter, list_input_devices,
};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::generated::{AudioKey, AudioState};
use crate::{AppWindow, Ctx, Page, report};

/// Bars across the meter.
const BARS: usize = 40;
/// Picker order of the language buttons.
const LANGUAGES: [Language; 3] = [Language::Ru, Language::En, Language::Auto];
// shortcut: CpalCapture keeps the processed audio until it stops, so the meter reopens it this
// often to bound memory (~4 MB); upgrade when capture gets a meter-only mode.
const REOPEN: Millis = Millis(60_000);
/// A device change reopens the stream once it has held this long: each reopen blocks the UI
/// thread on the audio driver.
const SETTLE: Millis = Millis(300);
/// How often the device list is read again while the page is shown (a listing takes ~2 ms).
const RELIST: Millis = Millis(2_000);

/// Opens a capture feeding the meter; the flag is set when the stream stops on its own.
type Opener<C> = fn(&Audio, LevelMeter, Arc<AtomicBool>) -> Result<Opened<C>, CaptureError>;

/// What the page calls outside itself; tests swap in fakes, so no test needs a microphone.
struct Io<C> {
    open: Opener<C>,
    list: fn() -> Result<Vec<String>, CaptureError>,
    now: fn() -> Millis,
}

/// An open capture, and whether its configured device was missing so the default opened.
struct Opened<C> {
    capture: C,
    fell_back: bool,
}

fn open_cpal(
    opts: &Audio,
    meter: LevelMeter,
    dead: Arc<AtomicBool>,
) -> Result<Opened<CpalCapture>, CaptureError> {
    let capture = CpalCapture::open(opts, meter, move |_: CaptureEvent| {
        dead.store(true, Ordering::Relaxed);
    })?;
    let fell_back = capture.fell_back_to_default();
    Ok(Opened { capture, fell_back })
}

fn since_start() -> Millis {
    static START: OnceLock<Instant> = OnceLock::new();
    let elapsed = START.get_or_init(Instant::now).elapsed().as_millis();
    Millis(u64::try_from(elapsed).unwrap_or(u64::MAX))
}

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let io = Io {
        open: open_cpal,
        list: list_input_devices,
        now: since_start,
    };
    bind_with(window, ctx, &io);
}

fn bind_with<C: 'static>(window: &AppWindow, ctx: &Ctx, io: &Io<C>) -> Rc<PageState<C>> {
    let state = window.global::<AudioState>();
    push_config(&state, &ctx.config.borrow().config, &Devices::default());

    // Shared by the callbacks; replacing them on a rebind drops the old meter with its capture.
    let page = Rc::new(PageState {
        window: window.as_weak(),
        ctx: ctx.clone(),
        devices: RefCell::new(Devices::default()),
        listed_at: Cell::new(Millis(0)),
        live: RefCell::new(Live::new(io.open, 0.0)),
        list: io.list,
        now: io.now,
        timer: Timer::default(),
    });

    let p = Rc::clone(&page);
    state.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    state.on_edited(move |key, value| p.edited(key, value));
    // Reset to defaults rebinds while the page may be open: its meter must keep running.
    if window.get_page() == Page::Audio {
        page.shown();
    }
    page
}

struct PageState<C> {
    window: slint::Weak<AppWindow>,
    ctx: Ctx,
    devices: RefCell<Devices>,
    listed_at: Cell<Millis>,
    live: RefCell<Live<C>>,
    list: fn() -> Result<Vec<String>, CaptureError>,
    now: fn() -> Millis,
    timer: Timer,
}

impl<C: 'static> PageState<C> {
    /// The page was built (opened from the rail): list the devices and start the meter.
    fn shown(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let state = window.global::<AudioState>();
        let config = self.ctx.config.borrow().config.clone();
        self.relist(&state, &config.audio.device);
        push_config(&state, &config, &self.devices.borrow());

        let weak: Weak<Self> = Rc::downgrade(self);
        self.timer.start(
            TimerMode::Repeated,
            Duration::from_millis(1000 / METER_HZ),
            move || {
                if let Some(page) = weak.upgrade() {
                    page.tick();
                }
            },
        );
    }

    /// Read the device list; the picker is only rebuilt when it changed (keeps an open popup).
    /// True when it changed.
    fn relist(&self, state: &AudioState<'_>, configured: &str) -> bool {
        self.listed_at.set((self.now)());
        let listed = (self.list)();
        let error = listed.as_ref().err().map(note).unwrap_or_default();
        state.set_list_error(error.into());
        let devices = Devices::new(listed.unwrap_or_default(), configured);
        if *self.devices.borrow() == devices {
            return false;
        }
        state.set_devices(ModelRc::from(Rc::new(VecModel::from(devices.rows()))));
        state.set_device(devices.row_of(configured));
        *self.devices.borrow_mut() = devices;
        true
    }

    /// One meter frame, or the close of the capture once the page is gone.
    fn tick(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let state = window.global::<AudioState>();
        let mut live = self.live.borrow_mut();
        if window.get_page() == Page::Audio && window.window().is_visible() {
            let opts = self.ctx.config.borrow().config.audio.clone();
            let now = (self.now)();
            if now >= self.listed_at.get() + RELIST
                && self.relist(&state, &opts.device)
                && (live.note.is_some() || live.fell_back)
            {
                // A mic was plugged in: retry the failed open, or leave the default for it.
                live.tried = None;
            }
            live.tick(&opts, now);
            state.set_clipping(live.meter.clipping(now));
            state.set_note(live.note.clone().unwrap_or_default().into());
            let fallback = live.fell_back.then(|| {
                format!(
                    "{} is not connected, showing the system default",
                    opts.device
                )
            });
            state.set_fallback(fallback.unwrap_or_default().into());
        } else {
            live.stop();
            self.timer.stop();
            state.set_clipping(false);
            state.set_fallback(SharedString::new());
        }
        state.set_live(live.is_open());
        let bars: Vec<f32> = live.meter.bars().collect();
        state.set_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
    }

    fn edited(&self, key: AudioKey, value: f32) {
        let mut file = self.ctx.config.borrow_mut();
        let config = &mut file.config;
        let audio = &mut config.audio;
        match key {
            AudioKey::Device => audio.device = self.devices.borrow().name(whole(value)),
            AudioKey::Gain => audio.gain_db = f64::from(value.round()),
            AudioKey::Sensitivity => {
                let range = Audio::METER_SENSITIVITY_DB;
                audio.meter_sensitivity_db = value.round().clamp(*range.start(), *range.end());
            }
            AudioKey::Normalize => audio.normalize = value != 0.0,
            AudioKey::Trim => audio.trim = value != 0.0,
            AudioKey::Denoise => audio.denoise = value != 0.0,
            AudioKey::Tail => audio.tail_ms = whole(value),
            AudioKey::MaxMinutes => audio.max_seconds = whole(value).max(1) * 60,
            AudioKey::Language => {
                if let Some(&lang) = LANGUAGES.get(whole(value) as usize) {
                    config.model.language = lang;
                    if let Some(window) = self.window.upgrade() {
                        window
                            .global::<AudioState>()
                            .set_language(language_row(lang));
                    }
                }
            }
        }
        drop(file);
        report(&self.window, self.ctx.save());
    }
}

/// The picker rows after "System default": the listed devices, then the configured one when it
/// is not plugged in, so it stays picked and an edit elsewhere does not reset it.
#[derive(Debug, Default, PartialEq)]
struct Devices {
    listed: Vec<String>,
    missing: Option<String>,
}

impl Devices {
    fn new(listed: Vec<String>, configured: &str) -> Self {
        let missing = (!configured.is_empty() && !listed.iter().any(|n| n == configured))
            .then(|| configured.to_owned());
        Self { listed, missing }
    }

    fn names(&self) -> impl Iterator<Item = &String> {
        self.listed.iter().chain(&self.missing)
    }

    fn rows(&self) -> Vec<SharedString> {
        let mut rows = vec![SharedString::from(strings::SYSTEM_DEFAULT_MIC)];
        rows.extend(self.listed.iter().map(SharedString::from));
        rows.extend(
            self.missing
                .iter()
                .map(|n| SharedString::from(strings::not_connected(n))),
        );
        rows
    }

    /// The config value of picker row `row`; row 0 is the system default (empty).
    fn name(&self, row: u32) -> String {
        (row as usize)
            .checked_sub(1)
            .and_then(|i| self.names().nth(i))
            .cloned()
            .unwrap_or_default()
    }

    fn row_of(&self, device: &str) -> i32 {
        let row = self.names().position(|n| n == device).map_or(0, |i| i + 1);
        i32::try_from(row).unwrap_or(0)
    }
}

/// Push every config value the page shows.
fn push_config(state: &AudioState<'_>, config: &Config, devices: &Devices) {
    let audio = &config.audio;
    state.set_devices(ModelRc::from(Rc::new(VecModel::from(devices.rows()))));
    state.set_device(devices.row_of(&audio.device));
    #[expect(clippy::cast_possible_truncation, reason = "gain is a few dB")]
    state.set_gain_db(audio.gain_db as f32);
    state.set_sensitivity_db(audio.meter_sensitivity_db);
    state.set_normalize(audio.normalize);
    state.set_trim(audio.trim);
    state.set_denoise(audio.denoise);
    #[expect(
        clippy::cast_precision_loss,
        reason = "ms and s values are far below 2^24"
    )]
    {
        state.set_tail_ms(audio.tail_ms as f32);
        state.set_max_minutes(audio.max_seconds as f32 / 60.0);
    }
    state.set_language(language_row(config.model.language));
}

fn language_row(lang: Language) -> i32 {
    LANGUAGES
        .iter()
        .position(|l| *l == lang)
        .and_then(|i| i32::try_from(i).ok())
        .unwrap_or(0)
}

/// A slider or picker value as a whole non-negative number.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "clamped to 0 first; UI values are small"
)]
fn whole(value: f32) -> u32 {
    value.round().max(0.0) as u32
}

fn note(e: &CaptureError) -> String {
    match e {
        CaptureError::NoDevice => NO_MIC.to_owned(),
        e => e.to_string(),
    }
}

/// The page's own capture and the meter it feeds.
struct Live<C> {
    open: Opener<C>,
    level: LevelMeter,
    meter: Meter,
    sensitivity: f32,
    capture: Option<Running<C>>,
    /// Device of the last open attempt; a failed one is not retried until it changes or the
    /// page is shown again.
    tried: Option<String>,
    /// A device that differs from `tried`, and since when.
    changed: Option<(String, Millis)>,
    /// Why the meter is off, after a failed open.
    note: Option<String>,
    /// The open capture runs on the system default: the configured device is missing.
    fell_back: bool,
}

struct Running<C> {
    _capture: C,
    since: Millis,
    dead: Arc<AtomicBool>,
}

impl<C> Live<C> {
    fn new(open: Opener<C>, sensitivity: f32) -> Self {
        Self {
            open,
            level: LevelMeter::default(),
            meter: Meter::new(BARS, sensitivity),
            sensitivity,
            capture: None,
            tried: None,
            changed: None,
            note: None,
            fell_back: false,
        }
    }

    fn is_open(&self) -> bool {
        self.capture.is_some()
    }

    /// Keep a capture open on `opts` and feed one level into the meter. The stream depends only
    /// on the device, whose change reopens it once settled; the gain scales the level read.
    fn tick(&mut self, opts: &Audio, now: Millis) {
        if self.sensitivity.to_bits() != opts.meter_sensitivity_db.to_bits() {
            self.sensitivity = opts.meter_sensitivity_db;
            self.meter = Meter::new(BARS, self.sensitivity);
        }
        let key = &opts.device;
        if self.tried.as_ref() == Some(key) {
            self.changed = None;
        } else if self.changed.as_ref().is_none_or(|(k, _)| k != key) {
            self.changed = Some((key.clone(), now));
        }
        let settled = self.tried.is_none()
            || self
                .changed
                .as_ref()
                .is_some_and(|(_, at)| now >= *at + SETTLE);
        let stale = self
            .capture
            .as_ref()
            .is_some_and(|r| r.dead.load(Ordering::Relaxed) || now >= r.since + REOPEN);
        if stale || (settled && self.tried.as_ref() != Some(key)) {
            self.reopen(opts, now);
        }
        #[expect(clippy::cast_possible_truncation, reason = "gain is a few dB")]
        let gain = db_to_linear(opts.gain_db as f32);
        self.meter.push(self.level.read().scaled(gain), now);
    }

    fn reopen(&mut self, opts: &Audio, now: Millis) {
        // One stream at a time: the old one closes before the new one opens.
        self.capture = None;
        self.fell_back = false;
        self.tried = Some(opts.device.clone());
        self.changed = None;
        let dead = Arc::new(AtomicBool::new(false));
        // The meter shows the level before denoise and trim: skip their work. The gain is
        // applied to the level in `tick`, so a slider drag needs no reopen.
        let meter_opts = Audio {
            gain_db: 0.0,
            denoise: false,
            trim: false,
            ..opts.clone()
        };
        match (self.open)(&meter_opts, self.level.clone(), Arc::clone(&dead)) {
            Ok(opened) => {
                self.note = None;
                self.fell_back = opened.fell_back;
                self.capture = Some(Running {
                    _capture: opened.capture,
                    since: now,
                    dead,
                });
            }
            Err(e) => self.note = Some(note(&e)),
        }
    }

    /// Close the capture and empty the meter; the next `tick` opens a new one.
    fn stop(&mut self) {
        self.capture = None;
        self.tried = None;
        self.changed = None;
        self.note = None;
        self.fell_back = false;
        self.meter = Meter::new(BARS, self.sensitivity);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use i_slint_backend_testing::{ElementHandle, mock_elapsed_time};

    use super::*;

    thread_local! {
        /// Captures open now, and opened in total.
        static OPEN: Cell<u32> = const { Cell::new(0) };
        static OPENED: Cell<u32> = const { Cell::new(0) };
        static NOW: Cell<Millis> = const { Cell::new(Millis(0)) };
        static LISTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
        static LIST_FAILS: Cell<bool> = const { Cell::new(false) };
    }

    struct Fake;

    impl Drop for Fake {
        fn drop(&mut self) {
            OPEN.set(OPEN.get() - 1);
        }
    }

    fn opened(fell_back: bool) -> Opened<Fake> {
        OPEN.set(OPEN.get() + 1);
        OPENED.set(OPENED.get() + 1);
        Opened {
            capture: Fake,
            fell_back,
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "has the Opener signature")]
    fn fake(_: &Audio, _: LevelMeter, _: Arc<AtomicBool>) -> Result<Opened<Fake>, CaptureError> {
        Ok(opened(false))
    }

    /// Like cpal: a configured name that is not listed opens the default.
    #[expect(clippy::unnecessary_wraps, reason = "has the Opener signature")]
    fn fake_fallback(
        opts: &Audio,
        _: LevelMeter,
        _: Arc<AtomicBool>,
    ) -> Result<Opened<Fake>, CaptureError> {
        let missing = !opts.device.is_empty() && !LISTED.with_borrow(|l| l.contains(&opts.device));
        Ok(opened(missing))
    }

    fn no_device(
        _: &Audio,
        _: LevelMeter,
        _: Arc<AtomicBool>,
    ) -> Result<Opened<Fake>, CaptureError> {
        Err(CaptureError::NoDevice)
    }

    /// No listed device means no device at all.
    fn fake_listed_only(
        opts: &Audio,
        meter: LevelMeter,
        dead: Arc<AtomicBool>,
    ) -> Result<Opened<Fake>, CaptureError> {
        if LISTED.with_borrow(Vec::is_empty) {
            return Err(CaptureError::NoDevice);
        }
        fake_fallback(opts, meter, dead)
    }

    fn fake_list() -> Result<Vec<String>, CaptureError> {
        if LIST_FAILS.get() {
            return Err(CaptureError::PermissionDenied);
        }
        Ok(LISTED.with_borrow(Clone::clone))
    }

    fn fake_now() -> Millis {
        NOW.get()
    }

    fn reset() {
        OPEN.set(0);
        OPENED.set(0);
        NOW.set(Millis(0));
        LISTED.with_borrow_mut(Vec::clear);
        LIST_FAILS.set(false);
    }

    /// The settings window on a fresh config dir, its Audio page on fakes.
    fn window(name: &str, config: Option<&str>, open: Opener<Fake>) -> AppWindow {
        i_slint_backend_testing::init_no_event_loop();
        let (window, _) = window_on(name, config, open);
        window.show().unwrap();
        window
    }

    /// [`window`] on the platform already set, and the page state behind it.
    fn window_on(
        name: &str,
        config: Option<&str>,
        open: Opener<Fake>,
    ) -> (AppWindow, Rc<PageState<Fake>>) {
        reset();
        let layout = crate::testing::layout("audio", name);
        if let Some(text) = config {
            std::fs::write(layout.config_file(), text).unwrap();
        }
        let window = crate::open(layout.clone()).unwrap();
        let (ctx, _) = Ctx::load(layout).unwrap();
        let io = Io {
            open,
            list: fake_list,
            now: fake_now,
        };
        let page = bind_with(&window, &ctx, &io);
        (window, page)
    }

    /// Open the Audio page; building it runs its `init`, which starts the meter.
    fn show_audio(window: &AppWindow) {
        window.set_page(Page::Audio);
        assert!(ElementHandle::find_by_accessible_label(window, "Gain").count() > 0);
    }

    fn frame(ms: u64) {
        NOW.set(NOW.get() + Millis(ms));
        mock_elapsed_time(Duration::from_millis(ms));
    }

    #[test]
    fn leaving_the_page_closes_the_capture() {
        let window = window("leave", None, fake);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        assert!(state.get_live());
        assert_eq!(OPEN.get(), 1);
        window.set_page(Page::About);
        frame(40);
        assert_eq!(OPEN.get(), 0, "capture still open after the page was left");
        assert!(!state.get_live());
    }

    #[test]
    fn hiding_the_window_closes_the_capture() {
        let window = window("hide", None, fake);
        show_audio(&window);
        frame(40);
        assert_eq!(OPEN.get(), 1);
        window.hide().unwrap();
        frame(40);
        assert_eq!(OPEN.get(), 0);
    }

    #[test]
    fn missing_device_note_and_picker_row() {
        let config = "version = 1\n[audio]\ndevice = \"USB Mic\"\n";
        let window = window("fallback", Some(config), fake_fallback);
        LISTED.set(vec!["Built-in".into()]);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        assert_eq!(
            state.get_fallback(),
            "USB Mic is not connected, showing the system default"
        );
        let rows: Vec<SharedString> = slint::Model::iter(&state.get_devices()).collect();
        assert_eq!(
            rows,
            ["System default", "Built-in", "USB Mic (not connected)"]
        );
        assert_eq!(state.get_device(), 2);
    }

    #[test]
    fn plugged_device_shows_up_and_clears_the_fallback() {
        let config = "version = 1\n[audio]\ndevice = \"USB Mic\"\n";
        let window = window("plug", Some(config), fake_fallback);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        assert_ne!(state.get_fallback(), "");
        LISTED.set(vec!["USB Mic".into()]);
        frame(RELIST.0);
        let rows: Vec<SharedString> = slint::Model::iter(&state.get_devices()).collect();
        assert_eq!(rows, ["System default", "USB Mic"]);
        assert_eq!(state.get_device(), 1);
        frame(40);
        assert_eq!(
            state.get_fallback(),
            "",
            "still on the default after the mic came back"
        );
    }

    #[test]
    fn lost_device_reopens_on_the_default_with_the_note() {
        reset();
        LISTED.set(vec!["USB Mic".into()]);
        let mut live = Live::new(fake_fallback, 0.0);
        let opts = Audio {
            device: "USB Mic".into(),
            ..Audio::default()
        };
        live.tick(&opts, Millis(0));
        assert!(!live.fell_back);
        LISTED.set(Vec::new());
        live.capture
            .as_ref()
            .unwrap()
            .dead
            .store(true, Ordering::Relaxed);
        live.tick(&opts, Millis(33));
        assert!(live.is_open());
        assert!(live.fell_back);
    }

    #[test]
    fn plugging_the_first_mic_retries_the_open() {
        let window = window("first-mic", None, fake_listed_only);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        assert_eq!(state.get_note(), NO_MIC);
        assert!(!state.get_live());
        LISTED.set(vec!["USB Mic".into()]);
        frame(RELIST.0);
        assert!(state.get_live(), "no retry after the mic was plugged in");
        assert_eq!(OPEN.get(), 1);
        assert_eq!(state.get_note(), "");
    }

    #[test]
    fn list_error_shows_its_kind() {
        let window = window("list-error", None, fake);
        LIST_FAILS.set(true);
        show_audio(&window);
        let state = window.global::<AudioState>();
        assert_eq!(state.get_list_error(), "microphone permission denied");
        LIST_FAILS.set(false);
        frame(RELIST.0);
        assert_eq!(state.get_list_error(), "");
    }

    /// With `OXI_SHOTS_DIR` set, draws the page with its configured mic not connected to
    /// `settings-audio-not-connected-<theme>.png`. Run it alone (`--exact`): it installs its own
    /// software-renderer platform, which the testing-backend tests in this binary cannot share.
    #[test]
    fn shot_not_connected() {
        use slint::platform::software_renderer::{
            MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType,
        };
        use slint::platform::{Platform, WindowAdapter};

        struct Shots(Rc<MinimalSoftwareWindow>);
        impl Platform for Shots {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                Ok(self.0.clone())
            }
        }

        let Some(dir) = std::env::var_os("OXI_SHOTS_DIR") else {
            return;
        };
        let (w, h) = (560_u32, 600_u32);
        let surface = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
        slint::platform::set_platform(Box::new(Shots(surface.clone()))).unwrap();
        let config = "version = 1\n[audio]\ndevice = \"USB Mic\"\n";
        let (window, page) = window_on("shot", Some(config), fake_fallback);
        LISTED.set(vec!["Built-in Microphone".into()]);
        surface.set_size(slint::PhysicalSize::new(w, h));
        window.show().unwrap();
        window.set_page(Page::Audio);
        page.shown();
        page.tick();
        for (theme, choice) in [
            ("light", crate::ThemeChoice::Light),
            ("dark", crate::ThemeChoice::Dark),
        ] {
            window.set_theme(choice);
            slint::platform::update_timers_and_animations();
            surface.request_redraw();
            let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
            assert!(surface.draw_if_needed(|r| {
                r.render(&mut buffer, w as usize);
            }));
            let bytes = buffer
                .iter()
                .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
                .collect();
            let size = tiny_skia::IntSize::from_wh(w, h).unwrap();
            let pixmap = tiny_skia::Pixmap::from_vec(bytes, size).unwrap();
            let path = std::path::Path::new(&dir);
            std::fs::create_dir_all(path).unwrap();
            pixmap
                .save_png(path.join(format!("settings-audio-not-connected-{theme}.png")))
                .unwrap();
        }
    }

    #[test]
    fn stop_closes_the_capture_and_tick_reopens_it() {
        reset();
        let mut live = Live::new(fake, 0.0);
        let opts = Audio::default();
        live.tick(&opts, Millis(0));
        live.tick(&opts, Millis(33));
        assert!(live.is_open());
        assert_eq!(OPEN.get(), 1, "one stream, not one per frame");
        live.stop();
        assert!(!live.is_open());
        assert_eq!(OPEN.get(), 0);
        live.tick(&opts, Millis(66));
        assert_eq!(OPEN.get(), 1);
    }

    #[test]
    fn gain_drag_keeps_the_stream_and_a_device_change_reopens_once_settled() {
        reset();
        let mut live = Live::new(fake, 0.0);
        let mut opts = Audio::default();
        live.tick(&opts, Millis(0));
        for (i, t) in (33..300).step_by(33).enumerate() {
            opts.gain_db = f64::from(u32::try_from(i).unwrap());
            live.tick(&opts, Millis(t));
        }
        assert_eq!(OPENED.get(), 1, "a gain change reopened the stream");
        opts.device = "USB Mic".into();
        live.tick(&opts, Millis(330));
        assert_eq!(OPENED.get(), 1, "reopened before the device settled");
        live.tick(&opts, Millis(330) + SETTLE);
        assert_eq!(OPENED.get(), 2);
        assert_eq!(OPEN.get(), 1);
        assert_eq!(live.tried.as_deref(), Some("USB Mic"));
    }

    #[test]
    fn dead_or_old_stream_is_replaced() {
        reset();
        let mut live = Live::new(fake, 0.0);
        let opts = Audio::default();
        live.tick(&opts, Millis(0));
        let first = Arc::clone(&live.capture.as_ref().unwrap().dead);
        first.store(true, Ordering::Relaxed);
        live.tick(&opts, Millis(33));
        assert!(!live.capture.as_ref().unwrap().dead.load(Ordering::Relaxed));
        live.tick(&opts, Millis(33) + REOPEN);
        assert_eq!(live.capture.as_ref().unwrap().since, Millis(33) + REOPEN);
        assert_eq!(OPEN.get(), 1);
        assert_eq!(OPENED.get(), 3);
    }

    #[test]
    fn no_device_shows_no_microphone_and_is_not_retried_each_frame() {
        let mut live = Live::new(no_device, 0.0);
        let opts = Audio::default();
        live.tick(&opts, Millis(0));
        assert!(!live.is_open());
        assert_eq!(live.note.as_deref(), Some(NO_MIC));
        live.note = None;
        live.tick(&opts, Millis(33));
        assert_eq!(live.note, None, "retried on the next frame");
        live.stop();
        live.tick(&opts, Millis(66));
        assert_eq!(
            live.note.as_deref(),
            Some(NO_MIC),
            "showing the page again retries"
        );
    }

    #[test]
    fn sensitivity_change_keeps_the_stream() {
        reset();
        let mut live = Live::new(fake, 0.0);
        let mut opts = Audio::default();
        live.tick(&opts, Millis(0));
        opts.meter_sensitivity_db = 10.0;
        live.tick(&opts, Millis(33) + SETTLE);
        assert!(live.is_open());
        assert_eq!(OPENED.get(), 1);
        assert_eq!(live.sensitivity.to_bits(), 10.0_f32.to_bits());
    }

    #[test]
    fn device_rows_map_to_config_names() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let d = Devices::new(names(&["A", "B"]), "USB");
        assert_eq!(
            d.rows(),
            ["System default", "A", "B", "USB (not connected)"]
        );
        for (row, want) in [(0, ""), (1, "A"), (3, "USB"), (9, "")] {
            assert_eq!(d.name(row), want, "row {row}");
        }
        assert_eq!(d.row_of("USB"), 3);
        assert_eq!(d.row_of(""), 0);
        assert_eq!(Devices::new(names(&["A"]), "A").missing, None);
        assert_eq!(Devices::new(Vec::new(), "").rows(), ["System default"]);
    }

    #[test]
    fn whole_rounds_and_floors_at_zero() {
        for (v, want) in [(0.0, 0), (2.6, 3), (-4.0, 0), (149.5, 150)] {
            assert_eq!(whole(v), want, "{v}");
        }
    }
}
