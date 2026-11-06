//! Audio page: microphone, live meter, DSP switches, tail, recording cap and language.
//!
//! The meter opens its own capture while the page is shown (decision D3) and polls it at
//! [`METER_HZ`] through the core [`Meter`]; leaving the page or closing the window closes it.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use oxi_core::config::Audio;
use oxi_core::overlay::{METER_HZ, Meter};
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
const NO_MIC: &str = "No microphone";
// shortcut: CpalCapture keeps the processed audio until it stops, so the meter reopens it this
// often to bound memory (~4 MB); upgrade when capture gets a meter-only mode.
const REOPEN: Millis = Millis(60_000);

pub fn bind(window: &AppWindow, ctx: &Ctx) {
    let state = window.global::<AudioState>();
    push_config(&state, &ctx.config.borrow().config, &[]);

    // Shared by the callbacks; replacing them on a rebind drops the old meter with its capture.
    let page = Rc::new(PageState {
        window: window.as_weak(),
        ctx: ctx.clone(),
        devices: RefCell::new(Vec::new()),
        live: RefCell::new(Live::new(open_cpal, 0.0)),
        timer: Timer::default(),
        start: Instant::now(),
    });

    let p = Rc::clone(&page);
    state.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    state.on_edited(move |key, value| p.edited(key, value));
    // Reset to defaults rebinds while the page may be open: its meter must keep running.
    if window.get_page() == Page::Audio {
        page.shown();
    }
}

struct PageState {
    window: slint::Weak<AppWindow>,
    ctx: Ctx,
    /// Device names behind picker rows 1.. (row 0 is the system default).
    devices: RefCell<Vec<String>>,
    live: RefCell<Live<CpalCapture>>,
    timer: Timer,
    start: Instant,
}

impl PageState {
    /// The page was built (opened from the rail): list the devices and start the meter.
    fn shown(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let listed = list_input_devices();
        let note = match &listed {
            Ok(names) if names.is_empty() => NO_MIC.to_owned(),
            Ok(_) => String::new(),
            Err(e) => note(e),
        };
        let config = self.ctx.config.borrow().config.clone();
        let names = picker_devices(listed.unwrap_or_default(), &config.audio.device);
        let state = window.global::<AudioState>();
        push_config(&state, &config, &names);
        state.set_note(note.into());
        *self.devices.borrow_mut() = names;

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

    /// One meter frame, or the close of the capture once the page is gone.
    fn tick(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let state = window.global::<AudioState>();
        let mut live = self.live.borrow_mut();
        if window.get_page() == Page::Audio && window.window().is_visible() {
            let opts = self.ctx.config.borrow().config.audio.clone();
            let now = Millis(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX));
            live.tick(&opts, now);
            state.set_clipping(live.meter.clipping(now));
            state.set_note(live.note.clone().unwrap_or_default().into());
        } else {
            live.stop();
            self.timer.stop();
            state.set_clipping(false);
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
            AudioKey::Device => {
                let row = whole(value).checked_sub(1);
                audio.device = row
                    .and_then(|i| self.devices.borrow().get(i as usize).cloned())
                    .unwrap_or_default();
            }
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

/// Push every config value the page shows; `names` are the devices after "System default".
fn push_config(state: &AudioState<'_>, config: &oxi_core::config::Config, names: &[String]) {
    let audio = &config.audio;
    let mut rows = vec![SharedString::from("System default")];
    rows.extend(names.iter().map(SharedString::from));
    state.set_devices(ModelRc::from(Rc::new(VecModel::from(rows))));
    let row = names
        .iter()
        .position(|n| *n == audio.device)
        .map_or(0, |i| i + 1);
    state.set_device(i32::try_from(row).unwrap_or(0));
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

/// The listed devices, plus the configured one when it is not plugged in, so the picker still
/// shows it and an edit elsewhere does not reset it to the default.
fn picker_devices(mut names: Vec<String>, configured: &str) -> Vec<String> {
    if !configured.is_empty() && !names.iter().any(|n| n == configured) {
        names.push(configured.to_owned());
    }
    names
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

/// Opens a capture feeding `meter`; `dead` is set when the stream stops on its own.
type Opener<C> = fn(&Audio, LevelMeter, Arc<AtomicBool>) -> Result<C, CaptureError>;

fn open_cpal(
    opts: &Audio,
    meter: LevelMeter,
    dead: Arc<AtomicBool>,
) -> Result<CpalCapture, CaptureError> {
    CpalCapture::open(opts, meter, move |_: CaptureEvent| {
        dead.store(true, Ordering::Relaxed);
    })
}

/// The page's own capture and the meter it feeds. Generic so tests run without a microphone.
struct Live<C> {
    open: Opener<C>,
    level: LevelMeter,
    meter: Meter,
    sensitivity: f32,
    capture: Option<Running<C>>,
    /// Device and gain of the last open attempt; a failed one is not retried until they change
    /// or the page is shown again.
    tried: Option<(String, f64)>,
    /// Why the meter is off, after a failed open.
    note: Option<String>,
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
            note: None,
        }
    }

    fn is_open(&self) -> bool {
        self.capture.is_some()
    }

    /// Keep a capture open on `opts` (the level only depends on device and gain) and feed one
    /// level into the meter.
    fn tick(&mut self, opts: &Audio, now: Millis) {
        if self.sensitivity.to_bits() != opts.meter_sensitivity_db.to_bits() {
            self.sensitivity = opts.meter_sensitivity_db;
            self.meter = Meter::new(BARS, self.sensitivity);
        }
        let stale = self
            .capture
            .as_ref()
            .is_some_and(|r| r.dead.load(Ordering::Relaxed) || now >= r.since + REOPEN);
        let key = (opts.device.clone(), opts.gain_db);
        if stale || self.tried.as_ref() != Some(&key) {
            // One stream at a time: the old one closes before the new one opens.
            self.capture = None;
            self.tried = Some(key);
            let dead = Arc::new(AtomicBool::new(false));
            match (self.open)(opts, self.level.clone(), Arc::clone(&dead)) {
                Ok(capture) => {
                    self.note = None;
                    self.capture = Some(Running {
                        _capture: capture,
                        since: now,
                        dead,
                    });
                }
                Err(e) => self.note = Some(note(&e)),
            }
        }
        self.meter.push(self.level.read(), now);
    }

    /// Close the capture and empty the meter; the next `tick` opens a new one.
    fn stop(&mut self) {
        self.capture = None;
        self.tried = None;
        self.note = None;
        self.meter = Meter::new(BARS, self.sensitivity);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    thread_local! {
        static OPEN: Cell<u32> = const { Cell::new(0) };
    }

    /// Counts open captures; a drop closes one.
    struct Fake;

    impl Drop for Fake {
        fn drop(&mut self) {
            OPEN.set(OPEN.get() - 1);
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "has the Opener signature")]
    fn fake(_: &Audio, _: LevelMeter, _: Arc<AtomicBool>) -> Result<Fake, CaptureError> {
        OPEN.set(OPEN.get() + 1);
        Ok(Fake)
    }

    fn no_device(_: &Audio, _: LevelMeter, _: Arc<AtomicBool>) -> Result<Fake, CaptureError> {
        Err(CaptureError::NoDevice)
    }

    #[test]
    fn stop_closes_the_capture_and_tick_reopens_it() {
        OPEN.set(0);
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
    fn device_or_gain_change_reopens_one_stream() {
        OPEN.set(0);
        let mut live = Live::new(fake, 0.0);
        let mut opts = Audio::default();
        live.tick(&opts, Millis(0));
        opts.gain_db = 6.0;
        live.tick(&opts, Millis(33));
        opts.device = "USB".into();
        live.tick(&opts, Millis(66));
        opts.tail_ms = 300;
        live.tick(&opts, Millis(99));
        assert_eq!(OPEN.get(), 1);
        assert_eq!(live.tried, Some(("USB".to_owned(), 6.0)));
    }

    #[test]
    fn dead_or_old_stream_is_replaced() {
        OPEN.set(0);
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
        OPEN.set(0);
        let mut live = Live::new(fake, 0.0);
        let mut opts = Audio::default();
        live.tick(&opts, Millis(0));
        opts.meter_sensitivity_db = 10.0;
        live.tick(&opts, Millis(33));
        assert!(live.is_open());
        assert_eq!(OPEN.get(), 1);
        assert_eq!(live.sensitivity.to_bits(), 10.0_f32.to_bits());
    }

    #[test]
    fn missing_configured_device_stays_in_the_picker() {
        let cases: [(&[&str], &str, &[&str]); 4] = [
            (&["A", "B"], "", &["A", "B"]),
            (&["A", "B"], "B", &["A", "B"]),
            (&["A"], "USB", &["A", "USB"]),
            (&[], "", &[]),
        ];
        for (listed, configured, want) in cases {
            let listed = listed.iter().map(|s| (*s).to_owned()).collect();
            assert_eq!(picker_devices(listed, configured), want, "{configured:?}");
        }
    }

    #[test]
    fn whole_rounds_and_floors_at_zero() {
        for (v, want) in [(0.0, 0), (2.6, 3), (-4.0, 0), (149.5, 150)] {
            assert_eq!(whole(v), want, "{v}");
        }
    }
}
