//! Audio page: microphone, live meter, DSP switches, tail, recording cap and the sounds of the
//! five events (start, stop, text ready, cancel, error).
//!
//! The meter opens its own capture while the page is shown (decision D3) and polls it at
//! [`METER_HZ`] through the core [`Meter`]; leaving the page or closing the window closes it.
//! The Test button records [`TEST_LENGTH`] through the dictation pipeline and plays it back.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::audio::{AudioError, Level, SAMPLE_RATE, db_to_linear};
use oxi_core::config::{Audio, AudioChannel, Config, Sounds, VadSensitivity};
use oxi_core::constants::PLAY_TAIL;
use oxi_core::ipc::{Push, Request};
use oxi_core::machine::Sound;
use oxi_core::overlay::{METER_HZ, Meter};
use oxi_core::sounds::{Builtin, Choice, FolderSound, OFF, is_plain_local_path, scan_folder};
use oxi_core::strings::{self, NO_MIC};
use oxi_core::types::Millis;
use oxi_platform::capture::{
    Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter, list_input_devices, play,
};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use crate::generated::{AudioKey, AudioState, GalleryTile, Toast};
use crate::ipc::{Event, Link, Subscription};
use crate::{AppWindow, Ctx, Page, report};

/// Bars kept for the meter: enough to fill the widest control column (`Theme.control-column-max`,
/// 360 px, at the 5 px pitch of the `Meter`); a narrower track shows the newest ones that fit.
const BARS: usize = 72;
// shortcut: CpalCapture keeps the processed audio until it stops, so the meter reopens it this
// often to bound memory (~4 MB); upgrade when capture gets a meter-only mode.
const REOPEN: Millis = Millis(60_000);
/// A device change reopens the stream once it has held this long: each reopen blocks the UI
/// thread on the audio driver.
const SETTLE: Millis = Millis(300);
/// How often the device list is read again while the page is shown (a listing takes ~2 ms).
const RELIST: Millis = Millis(2_000);
/// An open mic that gave only exact zeros this long gets the no-signal note (a muted headset, or
/// one behind a noise gate; a quiet room is never exactly zero).
const NO_SIGNAL: Millis = Millis(3_000);
/// Length of the Test button's recording.
const TEST_LENGTH: Millis = Millis(3_000);
/// Tile of Off, the first of every sound (the Preview button in `audio.slint` knows it is 0).
const OFF_TILE: usize = 0;
/// Tile of a built-in: Off is before it.
fn builtin_tile(i: usize) -> usize {
    i + 1
}
/// Tile of the own file: after the built-in sounds.
const OWN_FILE_TILE: usize = Builtin::ALL.len() + 1;
/// Tile of the first file of the sound folder: after the own file.
const FOLDER_TILE: usize = OWN_FILE_TILE + 1;
/// The sounds of the Sounds tab, in the order of its picker: `AudioState.view` 2 to 6.
const ROLES: [Sound; 5] = [
    Sound::Start,
    Sound::Stop,
    Sound::Done,
    Sound::Cancel,
    Sound::Error,
];
/// Tooltip of Preview while the daemon is not there to play it.
const PREVIEW_NEEDS_DAEMON: &str = "Start OxiYapper to hear a preview.";
/// Toast when the picked sound file is a network, device or stream path.
const SOUND_PATH_REFUSED: &str = "Pick a WAV or OGG file on a local drive";
/// Toast when the picked sound folder has no usable `.wav` or `.ogg` file.
const NO_SOUNDS_IN_FOLDER: &str = "No usable WAV or OGG files in that folder";

/// Opens a capture feeding the meter; the flag is set when the stream stops on its own.
type Opener<C> = fn(&Audio, LevelMeter, Arc<AtomicBool>) -> Result<Opened<C>, CaptureError>;

/// What the page calls outside itself; tests swap in fakes, so no test needs a microphone.
struct Io<C> {
    open: Opener<C>,
    list: fn() -> Result<Vec<String>, CaptureError>,
    now: fn() -> Millis,
    play: Player,
    /// The file dialog for an own sound; `None` when cancelled.
    pick_file: fn(&Path) -> Option<PathBuf>,
    /// The folder dialog for the sound folder; `None` when cancelled.
    pick_folder: fn(&Path) -> Option<PathBuf>,
    /// Lists the usable files of a sound folder; it runs on a thread of its own.
    scan: fn(&Path) -> Vec<FolderSound>,
}

/// Plays a mono clip at the given rate; the handle keeps it playing.
type Player = fn(&[f32], u32) -> Result<Box<dyn Any>, CaptureError>;

/// An open capture, whether its configured device was missing so the default opened, and the
/// name of the device it runs on.
struct Opened<C> {
    capture: C,
    fell_back: bool,
    name: String,
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
    let name = capture.device_name().to_owned();
    Ok(Opened {
        capture,
        fell_back,
        name,
    })
}

fn play_cpal(clip: &[f32], rate: u32) -> Result<Box<dyn Any>, CaptureError> {
    Ok(Box::new(play(clip, rate)?))
}

#[cfg(windows)]
fn pick_file(start: &Path) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_directory(start)
        .add_filter("Sound", &["wav", "ogg"])
        .pick_file()
}

/// No native dialog on this OS: `can-browse` turns the button and the tile off.
#[cfg(not(windows))]
fn pick_file(_: &Path) -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn pick_folder(start: &Path) -> Option<PathBuf> {
    rfd::FileDialog::new().set_directory(start).pick_folder()
}

/// No native dialog on this OS: `can-browse` turns the buttons off.
#[cfg(not(windows))]
fn pick_folder(_: &Path) -> Option<PathBuf> {
    None
}

/// Where the sound dialog opens: the folder of the sound now set when it is an own file in an
/// existing folder, else the settings folder.
fn pick_start(current: &str, fallback: &Path) -> PathBuf {
    Path::new(current)
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_dir())
        .unwrap_or(fallback)
        .to_path_buf()
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
        play: play_cpal,
        pick_file,
        pick_folder,
        scan: scan_folder,
    };
    bind_with(window, ctx, &io);
}

fn bind_with<C: Capture + 'static>(window: &AppWindow, ctx: &Ctx, io: &Io<C>) -> Rc<PageState<C>> {
    let state = window.global::<AudioState>();
    // The sound folder is read when the page is shown, not here.
    push_config(
        &state,
        &ctx.config.borrow().config,
        &Devices::default(),
        &[],
    );

    // Shared by the callbacks; replacing them on a rebind drops the old meter with its capture
    // and this subscription.
    let page = Rc::new_cyclic(|me: &Weak<PageState<C>>| PageState {
        _events: ctx.ipc.subscribe({
            let me = me.clone();
            move |event| {
                if let Some(page) = me.upgrade() {
                    page.event(event);
                }
            }
        }),
        window: window.as_weak(),
        ctx: ctx.clone(),
        devices: RefCell::new(Devices::default()),
        listed_at: Cell::new(Millis(0)),
        live: RefCell::new(Live::new(io.open, 0.0)),
        list: io.list,
        now: io.now,
        timer: Timer::default(),
        open: io.open,
        play: io.play,
        pick_file: io.pick_file,
        pick_folder: io.pick_folder,
        scan: io.scan,
        scanning: RefCell::new(None),
        folder: RefCell::new(Vec::new()),
        test: RefCell::new(None),
    });
    state.set_can_browse(cfg!(windows));
    state.set_preview_off(ctx.ipc.link().off_reason(PREVIEW_NEEDS_DAEMON).into());

    let p = Rc::clone(&page);
    state.on_shown(move || p.shown());
    let p = Rc::clone(&page);
    state.on_edited(move |key, value| p.edited(key, value));
    let p = Rc::clone(&page);
    state.on_test(move || p.start_test());
    let p = Rc::clone(&page);
    state.on_sound_picked(move |tile| p.sound_picked(tile));
    let p = Rc::clone(&page);
    state.on_choose_file(move || p.choose_file());
    let p = Rc::clone(&page);
    state.on_choose_folder(move || p.choose_folder());
    let p = Rc::clone(&page);
    state.on_remove_folder(move || p.remove_folder());
    let p = Rc::clone(&page);
    state.on_preview(move || p.preview());
    // Reset to defaults rebinds while the page may be open: its meter must keep running.
    if window.get_page() == Page::Audio {
        page.shown();
    }
    page
}

struct PageState<C> {
    _events: Subscription,
    window: slint::Weak<AppWindow>,
    ctx: Ctx,
    devices: RefCell<Devices>,
    listed_at: Cell<Millis>,
    live: RefCell<Live<C>>,
    list: fn() -> Result<Vec<String>, CaptureError>,
    now: fn() -> Millis,
    timer: Timer,
    open: Opener<C>,
    play: Player,
    pick_file: fn(&Path) -> Option<PathBuf>,
    pick_folder: fn(&Path) -> Option<PathBuf>,
    scan: fn(&Path) -> Vec<FolderSound>,
    /// The folder scan running on its thread: the page is shown at once and the tiles follow
    /// (decoding up to 200 files is no work for the UI thread). Replacing it drops the older
    /// scan's answer.
    scanning: RefCell<Option<Scan>>,
    /// The usable files of `sounds.folder`, read when the page is shown or the folder is picked.
    folder: RefCell<Vec<FolderSound>>,
    test: RefCell<Option<MicTest<C>>>,
}

/// A folder scan under way; `picked`: the folder was just picked, so an empty answer is toasted.
struct Scan {
    answer: Receiver<Vec<FolderSound>>,
    picked: bool,
}

/// The Test button's run: a recording, then its playback; each step ends at `until`.
enum MicTest<C> {
    Recording {
        capture: C,
        name: String,
        until: Millis,
    },
    Playing {
        _sound: Box<dyn Any>,
        until: Millis,
    },
}

impl<C: Capture + 'static> PageState<C> {
    /// The page was built (opened from the rail): list the devices and start the meter.
    fn shown(self: &Rc<Self>) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let state = window.global::<AudioState>();
        let config = self.ctx.config.borrow().config.clone();
        self.relist(&state, &config.audio.device);
        self.rescan(PathBuf::from(&config.sounds.folder), false);
        push_config(
            &state,
            &config,
            &self.devices.borrow(),
            &self.folder.borrow(),
        );

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
    /// True when it changed. Also asks for the daemon's warnings: it does not push them, and a
    /// bad sound file is found on its worker thread after the preview request has returned.
    fn relist(&self, state: &AudioState<'_>, configured: &str) -> bool {
        self.ctx.ipc.send(Request::Status);
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
        self.take_scan(&window);
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
            // The mic is open for the meter on the Recording tab only.
            if state.get_view() == 0 {
                live.tick(&opts, now);
            } else if live.is_open() || live.tried.is_some() {
                live.stop();
            }
            state.set_clipping(live.meter.clipping(now));
            state.set_note(live.note.clone().unwrap_or_default().into());
            state.set_silence(live.no_signal(now).unwrap_or_default().into());
            self.advance_test(&window, now);
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
            state.set_silence(SharedString::new());
            *self.test.borrow_mut() = None;
            state.set_testing(false);
        }
        state.set_live(live.is_open());
        // Leading silence is drawn as nothing, not a row of 2 px dots (audit M9); the Meter keeps
        // the newest bar at the right edge.
        let quiet = live.meter.bars().take_while(|&b| b <= 0.0).count();
        let bars: Vec<f32> = live.meter.bars().skip(quiet).collect();
        let clipped: Vec<bool> = live.meter.clipped().skip(quiet).collect();
        state.set_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
        state.set_clipped(ModelRc::from(Rc::new(VecModel::from(clipped))));
    }

    /// The Test button: record from the configured mic through the dictation pipeline (trim and
    /// normalise off: it plays what the mic really gives), then play it back in `tick`.
    fn start_test(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let state = window.global::<AudioState>();
        if self.test.borrow().is_some() {
            return;
        }
        let opts = Audio {
            trim: false,
            normalize: false,
            ..self.ctx.config.borrow().config.audio.clone()
        };
        let dead = Arc::new(AtomicBool::new(false));
        match (self.open)(&opts, LevelMeter::default(), dead) {
            Ok(opened) => {
                *self.test.borrow_mut() = Some(MicTest::Recording {
                    capture: opened.capture,
                    name: opened.name,
                    until: (self.now)() + TEST_LENGTH,
                });
                toast(&window, strings::MIC_TEST_RECORDING);
            }
            Err(e) => toast(&window, &strings::mic_test_failed(&note(&e))),
        }
        state.set_testing(self.test.borrow().is_some());
    }

    /// Move the Test button's run on once its step is over.
    fn advance_test(&self, window: &AppWindow, now: Millis) {
        let state = window.global::<AudioState>();
        let mut test = self.test.borrow_mut();
        match test.take() {
            Some(MicTest::Recording {
                capture,
                name,
                until,
            }) if now >= until => {
                let played = capture
                    .finish()
                    .and_then(|clip| Ok(((self.play)(&clip, SAMPLE_RATE)?, clip.len())));
                match played {
                    Ok((sound, len)) => {
                        let ms = len as u64 * 1000 / u64::from(SAMPLE_RATE);
                        *test = Some(MicTest::Playing {
                            _sound: sound,
                            until: now + Millis(ms) + PLAY_TAIL,
                        });
                        toast(window, strings::MIC_TEST_PLAYING);
                    }
                    Err(CaptureError::Audio(AudioError::MicSilence)) => {
                        toast(window, &strings::no_signal(&name));
                    }
                    Err(e) => toast(window, &strings::mic_test_failed(&note(&e))),
                }
            }
            Some(MicTest::Playing { until, .. }) if now >= until => {}
            other => *test = other,
        }
        state.set_testing(test.is_some());
    }

    fn event(&self, event: &Event) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let state = window.global::<AudioState>();
        match event {
            Event::Link(link) => {
                state.set_preview_off(link.off_reason(PREVIEW_NEEDS_DAEMON).into());
                // The daemon's warnings are its own: with it gone they are stale.
                if *link != Link::Online {
                    state.set_sound_warning(SharedString::new());
                }
            }
            Event::Push(Push::Status { warnings, .. }) => {
                state.set_sound_warning(sound_warning(warnings).into());
            }
            Event::Push(_) => {}
        }
    }

    /// Run `f` on the sound setting shown on the Sounds tab; `None` on another tab.
    fn with_sound<R>(&self, f: impl FnOnce(&mut String) -> R) -> Option<R> {
        let window = self.window.upgrade()?;
        let role = window.global::<AudioState>().get_view() - 2;
        let sounds = &mut self.ctx.config.borrow_mut().config.sounds;
        let slot = match role {
            0 => &mut sounds.start,
            1 => &mut sounds.stop,
            2 => &mut sounds.done,
            3 => &mut sounds.cancel,
            4 => &mut sounds.error,
            _ => return None,
        };
        Some(f(slot))
    }

    fn sound_picked(&self, tile: i32) {
        let tile = usize::try_from(tile).ok();
        if tile == Some(OFF_TILE) {
            return self.set_sound(OFF.to_owned());
        }
        if let Some(i) = tile.and_then(|t| t.checked_sub(FOLDER_TILE)) {
            let path = self.folder.borrow().get(i).map(|f| f.path.clone());
            if let Some(path) = path {
                self.set_sound(path.to_string_lossy().into_owned());
            }
            return;
        }
        match tile
            .and_then(|i| i.checked_sub(1))
            .and_then(|i| Builtin::ALL.get(i))
        {
            Some(builtin) => self.set_sound(builtin.id().to_owned()),
            None => self.choose_file(),
        }
    }

    /// The "Add folder" button: remember the folder, list its files as tiles. Nothing is copied.
    fn choose_folder(&self) {
        let config = self.ctx.layout.config_file();
        let fallback = config.parent().unwrap_or_else(|| Path::new("."));
        let current = self.ctx.config.borrow().config.sounds.folder.clone();
        let start = Some(Path::new(&current))
            .filter(|dir| dir.is_dir())
            .unwrap_or(fallback)
            .to_path_buf();
        let Some(path) = (self.pick_folder)(&start) else {
            return;
        };
        // The daemon never opens these; a folder that could block the listing is refused too.
        if !is_plain_local_path(&path.join("x.wav")) {
            if let Some(window) = self.window.upgrade() {
                toast(&window, SOUND_PATH_REFUSED);
            }
            return;
        }
        self.ctx.config.borrow_mut().config.sounds.folder = path.to_string_lossy().into_owned();
        // The old folder's tiles must not pick its files for the new folder.
        self.folder.borrow_mut().clear();
        self.rescan(path, true);
        self.push_sounds();
        report(&self.window, self.ctx.save());
    }

    /// The remove button: forget the folder and its tiles; the files are not touched.
    fn remove_folder(&self) {
        self.ctx.config.borrow_mut().config.sounds.folder.clear();
        self.folder.borrow_mut().clear();
        // A scan under way must not bring the old tiles back.
        *self.scanning.borrow_mut() = None;
        self.push_sounds();
        report(&self.window, self.ctx.save());
    }

    /// Lists `dir` on a thread; [`Self::tick`] takes the answer.
    fn rescan(&self, dir: PathBuf, picked: bool) {
        let (tx, answer) = mpsc::channel();
        let scan = self.scan;
        thread::spawn(move || {
            // The page is gone when the answer has no taker: nothing to do.
            let _ = tx.send(scan(&dir));
        });
        *self.scanning.borrow_mut() = Some(Scan { answer, picked });
    }

    /// Shows the folder's files once the scan is done.
    fn take_scan(&self, window: &AppWindow) {
        let answer = self.scanning.borrow().as_ref().map(|s| s.answer.try_recv());
        match answer {
            Some(Ok(found)) => {
                let picked = self.scanning.take().is_some_and(|s| s.picked);
                let none = found.is_empty();
                *self.folder.borrow_mut() = found;
                self.push_sounds();
                if picked && none {
                    toast(window, NO_SOUNDS_IN_FOLDER);
                }
            }
            Some(Err(TryRecvError::Disconnected)) => *self.scanning.borrow_mut() = None,
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// Show the sound tiles again from the config and the folder list.
    fn push_sounds(&self) {
        if let Some(window) = self.window.upgrade() {
            push_sounds(
                &window.global::<AudioState>(),
                &self.ctx.config.borrow().config.sounds,
                &self.folder.borrow(),
            );
        }
    }

    fn choose_file(&self) {
        let config = self.ctx.layout.config_file();
        let fallback = config.parent().unwrap_or_else(|| Path::new("."));
        let current = self.with_sound(|sound| sound.clone()).unwrap_or_default();
        let Some(path) = (self.pick_file)(&pick_start(&current, fallback)) else {
            return;
        };
        // The daemon refuses these at every load; saving one would leave a broken setting.
        if !is_plain_local_path(&path) {
            if let Some(window) = self.window.upgrade() {
                toast(&window, SOUND_PATH_REFUSED);
            }
            return;
        }
        self.set_sound(path.to_string_lossy().into_owned());
    }

    /// Whether the sound shown on the Sounds tab is the text ready one.
    fn on_done(&self) -> bool {
        self.window
            .upgrade()
            .is_some_and(|w| w.global::<AudioState>().get_view() - 2 == 2)
    }

    /// Save a sound, show it and play it, so a bad file is found at once. Off on the text ready
    /// sound only turns `done_enabled` off, so the sound it had comes back with the next pick.
    fn set_sound(&self, choice: String) {
        let off = choice == OFF;
        if self.on_done() {
            self.ctx.config.borrow_mut().config.sounds.done_enabled = !off;
        }
        if !(off && self.on_done()) {
            self.with_sound(|sound| *sound = choice);
        }
        self.push_sounds();
        report(&self.window, self.ctx.save());
        if !off {
            self.preview();
        }
    }

    fn preview(&self) {
        let volume_percent = percent(self.ctx.config.borrow().config.sounds.volume);
        let off = self.on_done() && !self.ctx.config.borrow().config.sounds.done_enabled;
        if let Some(choice) = self.with_sound(|sound| sound.clone()) {
            let choice = if off { OFF.to_owned() } else { choice };
            self.ctx.ipc.send(Request::PreviewSound {
                choice,
                volume_percent,
            });
        }
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
            AudioKey::Volume => config.sounds.volume = f64::from(whole(value).min(100)) / 100.0,
            AudioKey::Channel => audio.channel = channel_of(whole(value)),
            AudioKey::Vad => audio.vad_sensitivity = vad_of(whole(value)),
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
fn push_config(state: &AudioState<'_>, config: &Config, devices: &Devices, folder: &[FolderSound]) {
    let audio = &config.audio;
    state.set_devices(ModelRc::from(Rc::new(VecModel::from(devices.rows()))));
    state.set_device(devices.row_of(&audio.device));
    #[expect(clippy::cast_possible_truncation, reason = "gain is a few dB")]
    state.set_gain_db(audio.gain_db as f32);
    state.set_sensitivity_db(audio.meter_sensitivity_db);
    state.set_channel(match audio.channel {
        AudioChannel::Mix => 0,
        AudioChannel::Left => 1,
        AudioChannel::Right => 2,
    });
    state.set_vad(match audio.vad_sensitivity {
        VadSensitivity::Low => 0,
        VadSensitivity::Normal => 1,
        VadSensitivity::High => 2,
    });
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
    push_sounds(state, &config.sounds, folder);
    let defaults = Config::default();
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the default gain is a few dB"
    )]
    state.set_default_gain_db(defaults.audio.gain_db as f32);
    state.set_default_sensitivity_db(defaults.audio.meter_sensitivity_db);
    #[expect(
        clippy::cast_precision_loss,
        reason = "ms and s values are far below 2^24"
    )]
    {
        state.set_default_tail_ms(defaults.audio.tail_ms as f32);
        state.set_default_max_minutes(defaults.audio.max_seconds as f32 / 60.0);
    }
    state.set_default_volume(f32::from(percent(defaults.sounds.volume)));
}

/// Push the sound settings: each sound's tiles and pick, and the volume.
fn push_sounds(state: &AudioState<'_>, sounds: &Sounds, folder: &[FolderSound]) {
    let icons = (
        state.get_sound_icon(),
        state.get_file_icon(),
        state.get_off_icon(),
    );
    let defaults = Sounds::default();
    let (tiles, picked): (Vec<_>, Vec<_>) = ROLES
        .into_iter()
        .zip(by_role(sounds).into_iter().zip(by_role(&defaults)))
        .map(|(role, (text, default))| {
            // The text ready sound has its own switch; Off there is that switch.
            let off = role == Sound::Done && !sounds.done_enabled;
            let (picked, tiles) = sound_tiles(role, text, default, off, &icons, folder);
            (ModelRc::from(Rc::new(VecModel::from(tiles))), picked)
        })
        .unzip();
    state.set_role_tiles(ModelRc::from(Rc::new(VecModel::from(tiles))));
    state.set_role_tile(ModelRc::from(Rc::new(VecModel::from(picked))));
    state.set_volume(f32::from(percent(sounds.volume)));
    let name = Path::new(&sounds.folder).file_name().map_or_else(
        || sounds.folder.clone(),
        |n| n.to_string_lossy().into_owned(),
    );
    state.set_folder_name(name.into());
}

/// The row of a channel choice of the page's picker.
fn channel_of(row: u32) -> AudioChannel {
    match row {
        1 => AudioChannel::Left,
        2 => AudioChannel::Right,
        _ => AudioChannel::Mix,
    }
}

/// The row of a speech sensitivity choice of the page's picker.
fn vad_of(row: u32) -> VadSensitivity {
    match row {
        0 => VadSensitivity::Low,
        2 => VadSensitivity::High,
        _ => VadSensitivity::Normal,
    }
}

/// The settings of [`ROLES`], in its order.
fn by_role(sounds: &Sounds) -> [&str; 5] {
    [
        &sounds.start,
        &sounds.stop,
        &sounds.done,
        &sounds.cancel,
        &sounds.error,
    ]
}

/// The tiles of one sound, Off, `Builtin::ALL`, then the own file, and the one its setting `text`
/// picks (-1: none); `off` picks Off whatever the text is. The built-in that is the sound's
/// `default` says so.
fn sound_tiles(
    role: Sound,
    text: &str,
    default: &str,
    off: bool,
    (sound_icon, file_icon, off_icon): &(slint::Image, slint::Image, slint::Image),
    folder: &[FolderSound],
) -> (i32, Vec<GalleryTile>) {
    let choice = if off {
        Some(Choice::Off)
    } else {
        Choice::parse_for(role, text).ok()
    };
    let mut tiles = vec![GalleryTile {
        icon: off_icon.clone(),
        title: "Off".into(),
        ..GalleryTile::default()
    }];
    tiles.extend(Builtin::ALL.iter().map(|b| GalleryTile {
        icon: sound_icon.clone(),
        title: title(b.id()).into(),
        caption: if b.id() == default {
            "Default".into()
        } else {
            SharedString::new()
        },
        ..GalleryTile::default()
    }));
    let own = match &choice {
        Some(Choice::Custom(path)) => path.file_name().map_or_else(
            || path.to_string_lossy().into_owned(),
            |n| n.to_string_lossy().into_owned(),
        ),
        _ => String::new(),
    };
    tiles.push(GalleryTile {
        icon: file_icon.clone(),
        title: "Own file".into(),
        hint: if own.is_empty() {
            "Choose a .wav or .ogg file of up to 5 seconds.".into()
        } else {
            text.into()
        },
        caption: own.into(),
        disabled: !cfg!(windows),
        ..GalleryTile::default()
    });
    // The files of the sound folder: tiles after the own file, named by their file.
    tiles.extend(folder.iter().map(|f| GalleryTile {
        icon: sound_icon.clone(),
        title: f.name.as_str().into(),
        hint: f.path.to_string_lossy().as_ref().into(),
        ..GalleryTile::default()
    }));
    let picked = match choice {
        Some(Choice::Off) => Some(OFF_TILE),
        Some(Choice::Builtin(b)) => Builtin::ALL.iter().position(|x| *x == b).map(builtin_tile),
        // A file of the folder is its own tile; any other path is the own file.
        Some(Choice::Custom(path)) => Some(
            folder
                .iter()
                .position(|f| f.path == path)
                .map_or(OWN_FILE_TILE, |i| FOLDER_TILE + i),
        ),
        // `Sounds::normalise` already replaced anything else.
        None => None,
    };
    (
        picked.and_then(|i| i32::try_from(i).ok()).unwrap_or(-1),
        tiles,
    )
}

/// A built-in's name on its tile: its id in words, `apa-hit-3` as "Apa hit 3".
fn title(id: &str) -> String {
    let words = id.replace('-', " ");
    let mut chars = words.chars();
    chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The volume as the whole percent the slider and `PreviewSound` use.
fn percent(volume: f64) -> u8 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped to 0..=100 first"
    )]
    let percent = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
    percent
}

/// The daemon's lines about the sounds, in words for the "!". The preview's line stands only when
/// no other has one: it plays the same file, so the fact would be said twice.
fn sound_warning(lines: &[String]) -> String {
    let said = |prefix: &str, name: &str| {
        lines
            .iter()
            .find_map(|l| l.strip_prefix(prefix))
            .map(|why| format!("{name}: {why}"))
    };
    let settings: Vec<String> = [
        ("sounds.start: ", "Start sound"),
        ("sounds.stop: ", "Stop sound"),
        ("sounds.done: ", "Text ready sound"),
        ("sounds.cancel: ", "Cancel sound"),
        ("sounds.error: ", "Error sound"),
        // The daemon could not play a sound at all (no output device, the stream stopped).
        ("sounds: ", "Sounds"),
    ]
    .into_iter()
    .filter_map(|(prefix, name)| said(prefix, name))
    .collect();
    if settings.is_empty() {
        return said("sounds.preview: ", "Preview").unwrap_or_default();
    }
    settings.join("\n")
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

fn toast(window: &AppWindow, text: &str) {
    window.global::<Toast>().invoke_show(text.into());
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
    /// Device that gave a non-zero sample in this page session; a reopen keeps it.
    heard: Option<String>,
    /// Since when the open capture gave only zeros (the last signal, or the open of an unheard one).
    zeros_since: Millis,
}

/// What the meter's stream depends on: the device, and which channel it keeps.
fn stream_key(opts: &Audio) -> String {
    match opts.channel {
        AudioChannel::Mix => opts.device.clone(),
        side => format!("{}\u{0}{side:?}", opts.device),
    }
}

struct Running<C> {
    _capture: C,
    since: Millis,
    dead: Arc<AtomicBool>,
    name: String,
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
            heard: None,
            zeros_since: Millis(0),
        }
    }

    fn is_open(&self) -> bool {
        self.capture.is_some()
    }

    /// Keep a capture open on `opts` and feed one level into the meter. The stream depends only
    /// on the device and the channel, whose change reopens it once settled; the gain scales the level read.
    fn tick(&mut self, opts: &Audio, now: Millis) {
        if self.sensitivity.to_bits() != opts.meter_sensitivity_db.to_bits() {
            self.sensitivity = opts.meter_sensitivity_db;
            self.meter = Meter::new(BARS, self.sensitivity);
        }
        let key = &stream_key(opts);
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
        self.show(self.level.read(), opts, now);
    }

    /// Feed one raw level into the meter, scaled by the gain.
    fn show(&mut self, raw: Level, opts: &Audio, now: Millis) {
        if raw.peak > 0.0 {
            self.zeros_since = now;
            if let Some(r) = &self.capture
                && self.heard.as_deref() != Some(&r.name)
            {
                self.heard = Some(r.name.clone());
            }
        }
        #[expect(clippy::cast_possible_truncation, reason = "gain is a few dB")]
        let gain = db_to_linear(opts.gain_db as f32);
        self.meter.push(raw.scaled(gain), now);
    }

    /// The no-signal note once the open mic gave only exact zeros for [`NO_SIGNAL`]; any signal
    /// clears it, and a reopen of a device already heard does not start the wait over.
    fn no_signal(&self, now: Millis) -> Option<String> {
        self.capture
            .as_ref()
            .filter(|_| now >= self.zeros_since + NO_SIGNAL)
            .map(|r| strings::no_signal(&r.name))
    }

    fn reopen(&mut self, opts: &Audio, now: Millis) {
        // One stream at a time: the old one closes before the new one opens.
        self.capture = None;
        self.fell_back = false;
        self.tried = Some(stream_key(opts));
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
                if self.heard.as_deref() != Some(&opened.name) {
                    self.heard = None;
                    self.zeros_since = now;
                }
                self.note = None;
                self.fell_back = opened.fell_back;
                self.capture = Some(Running {
                    _capture: opened.capture,
                    since: now,
                    dead,
                    name: opened.name,
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
        self.heard = None;
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
        static SILENT_CLIP: Cell<bool> = const { Cell::new(false) };
        static PLAYED: Cell<usize> = const { Cell::new(0) };
        static PICKED: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        /// What the fake folder dialog answers.
        static PICKED_FOLDER: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        /// The folder the last file dialog was told to open on.
        static PICK_START: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
        /// `normalize` of the last open through `fake`.
        static NORMALIZE: Cell<Option<bool>> = const { Cell::new(None) };
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
            name: "Fake Mic".into(),
        }
    }

    /// The Test button's recording: `SILENT_CLIP` makes it all zeros, else one second.
    impl Capture for Fake {
        fn open(
            _: &Audio,
            _: LevelMeter,
            _: impl FnMut(CaptureEvent) + Send + 'static,
        ) -> Result<Self, CaptureError> {
            Ok(opened(false).capture)
        }
        fn input_rate(&self) -> u32 {
            SAMPLE_RATE
        }
        fn dropped_samples(&self) -> u64 {
            0
        }
        fn fell_back_to_default(&self) -> bool {
            false
        }
        fn finish(self) -> Result<Vec<f32>, CaptureError> {
            if SILENT_CLIP.get() {
                Err(AudioError::MicSilence.into())
            } else {
                Ok(vec![0.1; SAMPLE_RATE as usize])
            }
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "has the Player signature")]
    fn fake_play(clip: &[f32], _: u32) -> Result<Box<dyn Any>, CaptureError> {
        PLAYED.set(clip.len());
        Ok(Box::new(()))
    }

    #[expect(clippy::unnecessary_wraps, reason = "has the Opener signature")]
    fn fake(o: &Audio, _: LevelMeter, _: Arc<AtomicBool>) -> Result<Opened<Fake>, CaptureError> {
        NORMALIZE.set(Some(o.normalize));
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

    fn fake_pick(start: &Path) -> Option<PathBuf> {
        PICK_START.set(Some(start.to_path_buf()));
        PICKED.with_borrow(Clone::clone)
    }

    fn fake_pick_folder(_: &Path) -> Option<PathBuf> {
        PICKED_FOLDER.with_borrow(Clone::clone)
    }

    /// The folder whose scan waits until a test lets it go (a folder too slow to read).
    static GATED: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

    /// The real scan, held back while `dir` is the gated folder (at most 5 s, so a failed test
    /// leaves no thread behind).
    fn gated_scan(dir: &Path) -> Vec<FolderSound> {
        let held = Instant::now();
        while GATED.lock().unwrap().as_deref() == Some(dir)
            && held.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(1));
        }
        scan_folder(dir)
    }

    /// Ticks until the folder scan has answered.
    fn settle(page: &PageState<Fake>) {
        let waited = Instant::now();
        while page.scanning.borrow().is_some() {
            assert!(waited.elapsed() < Duration::from_secs(5), "no scan answer");
            thread::sleep(Duration::from_millis(1));
            page.tick();
        }
    }

    fn reset() {
        PICKED_FOLDER.with_borrow_mut(|p| *p = None);
        OPEN.set(0);
        OPENED.set(0);
        NOW.set(Millis(0));
        LISTED.with_borrow_mut(Vec::clear);
        LIST_FAILS.set(false);
        SILENT_CLIP.set(false);
        PLAYED.set(0);
        PICKED.set(None);
        PICK_START.set(None);
        NORMALIZE.set(None);
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
            play: fake_play,
            pick_file: fake_pick,
            pick_folder: fake_pick_folder,
            scan: gated_scan,
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

    /// The bars cover the track end to end at the smallest and the largest window: 40 of them
    /// drew 200 px, so the left part of a 240 to 360 px track stayed empty.
    #[test]
    fn meter_bars_reach_both_ends_of_the_track() {
        // The widest gap the `Meter` leaves at an end: its inset plus one bar pitch.
        const GAP: f32 = 7.0;
        let window = window("meter-ends", None, fake);
        show_audio(&window);
        let widest = window.global::<crate::Tokens>().get_control_column_max();
        let kept = f32::from(u16::try_from(BARS).unwrap());
        assert!(kept * 5.0 + GAP >= widest, "{BARS} bars, column {widest}");
        let state = window.global::<AudioState>();
        state.set_live(true);
        state.set_bars(ModelRc::from(Rc::new(VecModel::from(vec![0.5_f32; BARS]))));
        // 1000: a 252 px track, where whole pitches from the right end left 3 px at the left.
        for (width, height) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 860.0)] {
            window
                .window()
                .set_size(slint::LogicalSize::new(width, height));
            // Runs the shell's page-width handler; the 30 Hz meter timer stays asleep.
            mock_elapsed_time(Duration::from_millis(1));
            let meter = crate::testing::with_role(
                &window,
                "Level",
                i_slint_backend_testing::AccessibleRole::ProgressIndicator,
            );
            let left = meter.absolute_position().x;
            let right = left + meter.size().width;
            let bars: Vec<f32> = meter
                .query_descendants()
                .find_all()
                .iter()
                .filter(|b| (b.size().width - 3.0).abs() < 0.1)
                .map(|b| b.absolute_position().x)
                .filter(|&x| x >= left + 2.0)
                .collect();
            let first = bars.iter().copied().fold(f32::MAX, f32::min);
            let last = bars.iter().copied().fold(f32::MIN, f32::max);
            assert!(first - left < GAP, "{width}: {first} from {left}");
            assert!(right - (last + 3.0) < GAP, "{width}: {last} to {right}");
            // Centred: the newest bar keeps the inset the oldest one does.
            let (gap_left, gap_right) = (first - left, right - (last + 3.0));
            assert!(
                (gap_left - gap_right).abs() < 0.6,
                "{width}: insets {gap_left} and {gap_right}"
            );
        }
    }

    /// An open mic with only silence so far draws no bar; the track says it listens, so it does
    /// not look like a dead meter. The first bar takes the word away.
    #[test]
    fn a_quiet_open_mic_says_it_listens() {
        let window = window("meter-idle", None, fake);
        show_audio(&window);
        let state = window.global::<AudioState>();
        state.set_live(true);
        state.set_bars(ModelRc::from(Rc::new(VecModel::from(Vec::<f32>::new()))));
        assert!(crate::testing::has(&window, "Listening"));
        state.set_bars(ModelRc::from(Rc::new(VecModel::from(vec![0.4_f32]))));
        assert!(!crate::testing::has(&window, "Listening"));
    }

    #[test]
    fn mic_giving_only_zeros_gets_the_no_signal_note() {
        reset();
        let mut live = Live::new(fake, 0.0);
        let opts = Audio::default();
        let zero = Level {
            peak: 0.0,
            rms: 0.0,
        };
        live.tick(&opts, Millis(0));
        live.show(zero, &opts, Millis(500));
        assert_eq!(live.no_signal(Millis(500)), None, "too early");
        live.show(zero, &opts, NO_SIGNAL);
        let want =
            "No sound from Fake Mic yet. Speak, check that it is not muted, or reconnect it.";
        assert_eq!(live.no_signal(NO_SIGNAL).as_deref(), Some(want));
        // A quiet room is not exactly zero: one tiny sample clears it.
        let hiss = Level {
            peak: 1e-5,
            rms: 1e-6,
        };
        live.show(hiss, &opts, NO_SIGNAL + Millis(33));
        assert_eq!(live.no_signal(NO_SIGNAL + Millis(33)), None);
    }

    #[test]
    fn a_heard_mic_is_not_flagged_again_by_a_reopen_but_is_by_new_zeros() {
        reset();
        let mut live = Live::new(fake, 0.0);
        let opts = Audio::default();
        let zero = Level {
            peak: 0.0,
            rms: 0.0,
        };
        let hiss = Level {
            peak: 1e-5,
            rms: 1e-6,
        };
        live.tick(&opts, Millis(0));
        live.show(hiss, &opts, Millis(100));
        // Gated while nobody speaks: zeros for the whole REOPEN, then a reopen.
        live.tick(&opts, REOPEN);
        live.show(zero, &opts, REOPEN + Millis(33));
        assert!(
            live.no_signal(REOPEN + Millis(33)).is_some(),
            "3 s of zeros after a signal"
        );
        live.show(hiss, &opts, REOPEN + Millis(66));
        assert_eq!(live.no_signal(REOPEN + Millis(66)), None);
        // Another reopen right after a signal: the wait is not restarted by the reopen itself.
        let t = REOPEN + REOPEN;
        live.show(hiss, &opts, t - Millis(33));
        live.tick(&opts, t);
        live.show(zero, &opts, t + Millis(33));
        assert_eq!(live.no_signal(t + Millis(33)), None, "zeros for 66 ms only");
    }

    #[test]
    fn silent_mic_note_shows_on_the_page_and_clears_when_left() {
        let window = window("silent", None, fake);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        assert_eq!(state.get_silence(), "");
        frame(NO_SIGNAL.0);
        assert_eq!(
            state.get_silence(),
            "No sound from Fake Mic yet. Speak, check that it is not muted, or reconnect it."
        );
        window.set_page(Page::About);
        frame(40);
        assert_eq!(state.get_silence(), "");
    }

    #[test]
    fn test_button_records_then_plays_back_then_clears() {
        let window = window("mic-test", None, fake);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        state.invoke_test();
        let toast = || window.global::<Toast>().get_text();
        assert_eq!(toast(), strings::MIC_TEST_RECORDING);
        assert!(state.get_testing());
        frame(TEST_LENGTH.0);
        assert_eq!(toast(), strings::MIC_TEST_PLAYING);
        assert_eq!(PLAYED.get(), SAMPLE_RATE as usize);
        assert_eq!(
            NORMALIZE.get(),
            Some(false),
            "test clip must not be normalised"
        );
        frame(1_000);
        assert!(state.get_testing(), "playback stays open for a tail");
        frame(PLAY_TAIL.0);
        assert!(!state.get_testing());
        frame(3_000);
        assert_eq!(toast(), "", "a toast leaves by itself");
    }

    #[test]
    fn test_button_on_a_silent_mic_says_no_signal() {
        let window = window("mic-test-silent", None, fake);
        show_audio(&window);
        SILENT_CLIP.set(true);
        let state = window.global::<AudioState>();
        state.invoke_test();
        frame(TEST_LENGTH.0);
        assert_eq!(
            window.global::<Toast>().get_text(),
            "No sound from Fake Mic yet. Speak, check that it is not muted, or reconnect it."
        );
        assert_eq!(PLAYED.get(), 0);
        assert!(!state.get_testing());
    }

    #[test]
    fn audio_states_move_no_row() {
        let window = window("no-move", None, fake);
        show_audio(&window);
        window
            .window()
            .set_size(slint::LogicalSize::new(1280.0, 2400.0));
        frame(40);
        let before = crate::testing::spots(&window);
        let state = window.global::<AudioState>();
        // The mic test and the silent mic, as the page's own tick sets them.
        state.invoke_test();
        frame(NO_SIGNAL.0);
        let silence = state.get_silence();
        assert_ne!(silence, "");
        assert!(
            crate::testing::has(&window, &silence),
            "the \"!\" holds the warning"
        );
        crate::testing::assert_nothing_moved(&before, &crate::testing::spots(&window));
        // The saved device is gone and the list failed: the same slot of the Device row.
        state.set_fallback("Synth Mic is not connected, showing the system default".into());
        state.set_list_error("cannot list devices".into());
        assert!(
            crate::testing::has(
                &window,
                "cannot list devices\nSynth Mic is not connected, showing the system default"
            ),
            "the \"!\" says both, the worse first"
        );
        crate::testing::assert_nothing_moved(&before, &crate::testing::spots(&window));
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
        let config = "version = 2\n[audio]\ndevice = \"USB Mic\"\n";
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
            ["System default", "Built-in", "Not connected: USB Mic"]
        );
        assert_eq!(state.get_device(), 2);
    }

    #[test]
    fn plugged_device_shows_up_and_clears_the_fallback() {
        let config = "version = 2\n[audio]\ndevice = \"USB Mic\"\n";
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

    /// Draws the page: the configured mic not connected, the level meter with bars, and with
    /// clipping, each frame non-empty and not one colour; the page lists and opens devices through
    /// fakes. With `OXI_SHOTS_DIR` set the frames land in `settings-audio-<screen>-<theme>.png`.
    /// The renderer platform is per thread and the other tests here use the testing backend, so
    /// the drawing runs on a thread of its own.
    #[test]
    fn shots_audio() {
        thread::spawn(draw_audio).join().unwrap();
    }

    /// The window's next frame, `w` x `h` physical pixels, as a pixmap.
    fn frame_pixels(
        surface: &Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
        w: u32,
        h: u32,
    ) -> tiny_skia::Pixmap {
        use slint::platform::software_renderer::PremultipliedRgbaColor;

        let mut buffer = vec![PremultipliedRgbaColor::default(); (w * h) as usize];
        assert!(surface.draw_if_needed(|r| {
            r.render(&mut buffer, w as usize);
        }));
        let bytes = buffer
            .iter()
            .flat_map(|p| [p.red, p.green, p.blue, p.alpha])
            .collect();
        tiny_skia::Pixmap::from_vec(bytes, tiny_skia::IntSize::from_wh(w, h).unwrap()).unwrap()
    }

    thread_local! {
        // The shot platform's animation clock, stepped by hand.
        static SHOT_CLOCK: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    }

    /// A frame with the fades done. A fade starts when a frame reads the new colour: draw one, then
    /// step past them (one frame alone drew the Test button light on dark, as it faded in).
    fn settled_frame(
        surface: &Rc<slint::platform::software_renderer::MinimalSoftwareWindow>,
        w: u32,
        h: u32,
    ) -> tiny_skia::Pixmap {
        slint::platform::update_timers_and_animations();
        surface.request_redraw();
        frame_pixels(surface, w, h);
        SHOT_CLOCK.set(SHOT_CLOCK.get() + Duration::from_millis(300));
        slint::platform::update_timers_and_animations();
        surface.request_redraw();
        frame_pixels(surface, w, h)
    }

    fn draw_audio() {
        use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
        use slint::platform::{Platform, WindowAdapter};

        thread_local! {
            static LAST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
        }
        struct Shots;
        impl Platform for Shots {
            fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
                let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
                LAST.with_borrow_mut(|last| *last = Some(window.clone()));
                Ok(window)
            }
            fn duration_since_start(&self) -> Duration {
                SHOT_CLOCK.get()
            }
        }

        let dir = std::env::var_os("OXI_SHOTS_DIR").map(PathBuf::from);
        if let Some(dir) = &dir {
            std::fs::create_dir_all(dir).unwrap();
        }
        let (w, h) = (1280_u32, 800_u32);
        slint::platform::set_platform(Box::new(Shots)).unwrap();
        let bars = |f: fn(usize) -> f32| -> Vec<f32> { (0..BARS).map(f).collect() };
        let screens: [(&str, Option<&str>, Vec<f32>, bool); 4] = [
            (
                "not-connected",
                Some(
                    "version = 2
[audio]
device = \"USB Mic\"
",
                ),
                vec![],
                false,
            ),
            (
                "level",
                None,
                bars(|i| 0.1 + 0.8 * ((i as f32) * 0.5).sin().abs()),
                false,
            ),
            (
                "clipping",
                None,
                bars(|i| if i > BARS - 8 { 1.0 } else { 0.3 }),
                true,
            ),
            ("no-signal", None, vec![], false),
        ];
        for (screen, config, bars, clipping) in screens {
            let (window, page) = window_on("shot", config, fake_fallback);
            LISTED.set(vec!["Built-in Microphone".into()]);
            let surface = LAST.with_borrow(Clone::clone).unwrap();
            surface.set_size(slint::PhysicalSize::new(w, h));
            window.show().unwrap();
            // Builds the page, whose `init` starts the meter; so stop it only after that.
            show_audio(&window);
            page.tick();
            // Its next frame would overwrite the bars and hints set below.
            page.timer.stop();
            if !bars.is_empty() {
                let state = window.global::<AudioState>();
                // The loud bars at the right end clipped; the quiet ones stay accent.
                let marks: Vec<bool> = bars.iter().map(|&b| clipping && b >= 1.0).collect();
                state.set_bars(ModelRc::from(Rc::new(VecModel::from(bars))));
                state.set_clipped(ModelRc::from(Rc::new(VecModel::from(marks))));
                state.set_clipping(clipping);
            }
            if screen == "no-signal" {
                let state = window.global::<AudioState>();
                state.set_silence(strings::no_signal("Microphone (USB Headset)").into());
                window
                    .global::<Toast>()
                    .invoke_show(strings::MIC_TEST_RECORDING.into());
            }
            for (theme, choice) in [
                ("light", crate::ThemeChoice::Light),
                ("dark", crate::ThemeChoice::Dark),
            ] {
                window.set_theme(choice);
                let pixmap = settled_frame(&surface, w, h);
                let first = pixmap.pixels()[0];
                assert!(
                    pixmap.pixels().iter().any(|p| p.alpha() > 0),
                    "{screen} {theme} drew nothing"
                );
                assert!(
                    pixmap.pixels().iter().any(|p| *p != first),
                    "{screen} {theme} is one flat colour"
                );
                if let Some(dir) = &dir {
                    pixmap
                        .save_png(dir.join(format!("settings-audio-{screen}-{theme}.png")))
                        .unwrap();
                }
            }
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
        assert_eq!(d.rows(), ["System default", "A", "B", "Not connected: USB"]);
        for (row, want) in [(0, ""), (1, "A"), (3, "USB"), (9, "")] {
            assert_eq!(d.name(row), want, "row {row}");
        }
        assert_eq!(d.row_of("USB"), 3);
        assert_eq!(d.row_of(""), 0);
        assert_eq!(Devices::new(names(&["A"]), "A").missing, None);
        assert_eq!(Devices::new(Vec::new(), "").rows(), ["System default"]);
    }

    fn field(window: &AppWindow, label: &str) -> ElementHandle {
        crate::testing::with_role(
            window,
            label,
            i_slint_backend_testing::AccessibleRole::Spinbox,
        )
    }

    /// A typed value takes the same path as a dragged slider: clamped, snapped to the step, saved.
    #[test]
    fn typed_values_are_clamped_and_snapped_before_saving() {
        use slint::platform::{Key as K, PointerEventButton, WindowEvent};

        i_slint_backend_testing::init_no_event_loop();
        let (window, page) = window_on("number-field", None, fake);
        window.show().unwrap();
        show_audio(&window);
        let saved = || page.ctx.config.borrow().config.audio.clone();
        field(&window, "Gain").set_accessible_value("-50");
        assert!((saved().gain_db - -20.0).abs() < f64::EPSILON, "clamped");
        field(&window, "Gain").set_accessible_value("12.6");
        assert!((saved().gain_db - 13.0).abs() < f64::EPSILON, "rounded");
        window.global::<AudioState>().set_view(1);
        field(&window, "Tail after release").set_accessible_value("333");
        assert_eq!(saved().tail_ms, 350, "snapped to the 50 ms step");
        window.global::<AudioState>().set_view(0);
        // Not a number: nothing changes.
        field(&window, "Gain").set_accessible_value("loud");
        assert!((saved().gain_db - 13.0).abs() < f64::EPSILON);
        // L1: a positive gain shows its sign.
        assert_eq!(
            field(&window, "Gain").accessible_value().as_deref(),
            Some("+13")
        );
        // Real typing: click, go to the end, one more digit, Enter commits and clamps.
        window.global::<AudioState>().set_view(1);
        field(&window, "Max length").mock_single_click(PointerEventButton::Left);
        let w = window.window();
        for text in [SharedString::from(K::End), "5".into(), K::Return.into()] {
            w.dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            w.dispatch_event(WindowEvent::KeyReleased { text });
        }
        assert_eq!(saved().max_seconds, 7200, "605 min clamps to 120 min");
    }

    fn press(window: &AppWindow, key: impl Into<SharedString>) {
        let text: SharedString = key.into();
        let w = window.window();
        w.dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.clone() });
        w.dispatch_event(slint::platform::WindowEvent::KeyReleased { text });
    }

    fn select_all(window: &AppWindow) {
        use slint::platform::{Key as K, WindowEvent};
        let w = window.window();
        let ctrl: SharedString = K::Control.into();
        w.dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
        press(window, "a");
        w.dispatch_event(WindowEvent::KeyReleased { text: ctrl });
    }

    #[test]
    fn leaving_a_field_untouched_keeps_a_value_off_the_step() {
        use slint::platform::{Key as K, PointerEventButton};

        i_slint_backend_testing::init_no_event_loop();
        let config = "version = 2\n[audio]\ntail_ms = 125\n";
        let (window, page) = window_on("untouched", Some(config), fake);
        window.show().unwrap();
        show_audio(&window);
        window.global::<AudioState>().set_view(1);
        let tail = || page.ctx.config.borrow().config.audio.tail_ms;
        for leave in [K::Return, K::Tab] {
            field(&window, "Tail after release").mock_single_click(PointerEventButton::Left);
            press(&window, leave);
            assert_eq!(tail(), 125, "the 50 ms step snapped an unedited value");
        }
        assert_eq!(
            field(&window, "Tail after release")
                .accessible_value()
                .as_deref(),
            Some("125")
        );
    }

    #[test]
    fn nan_inf_and_empty_are_refused_and_the_old_value_stays() {
        use slint::platform::{Key as K, PointerEventButton};

        i_slint_backend_testing::init_no_event_loop();
        let (window, page) = window_on("refused", None, fake);
        window.show().unwrap();
        show_audio(&window);
        window.global::<AudioState>().set_view(1);
        let seconds = || page.ctx.config.borrow().config.audio.max_seconds;
        let type_over = |text: &str| {
            select_all(&window);
            press(&window, K::Delete);
            for c in text.chars() {
                press(&window, c.to_string());
            }
            press(&window, K::Return);
        };
        field(&window, "Max length").mock_single_click(PointerEventButton::Left);
        // Selecting works, or the refusals below would prove nothing.
        type_over("25");
        assert_eq!(seconds(), 1500);
        for text in ["nan", "inf", "infinity", ""] {
            type_over(text);
            assert_eq!(seconds(), 1500, "{text:?} was accepted");
        }
        assert_eq!(
            field(&window, "Max length").accessible_value().as_deref(),
            Some("25")
        );
        field(&window, "Max length").set_accessible_value("nan");
        assert_eq!(seconds(), 1500, "set-value took nan");
    }

    /// A path of the OS under test: `Path::file_name` does not split `\` on Unix.
    fn snd(name: &str) -> String {
        std::env::temp_dir()
            .join("Sounds")
            .join(name)
            .to_string_lossy()
            .into_owned()
    }

    /// The window on the Start sound sub-tab, with the fake file dialog.
    fn sounds_page(name: &str, config: Option<&str>) -> (AppWindow, Rc<PageState<Fake>>) {
        i_slint_backend_testing::init_no_event_loop();
        let (window, page) = window_on(name, config, fake);
        window.show().unwrap();
        show_audio(&window);
        window.global::<AudioState>().set_view(2);
        (window, page)
    }

    #[test]
    fn choosing_an_own_file_saves_its_path_and_cancelling_changes_nothing() {
        let (window, page) = sounds_page("own-file", None);
        let state = window.global::<AudioState>();
        let start = || page.ctx.config.borrow().config.sounds.start.clone();
        state.invoke_sound_picked(i32::try_from(OWN_FILE_TILE).unwrap());
        assert_eq!(start(), "apa-hit-3", "the dialog was cancelled");
        PICKED.set(Some(snd("my.wav").into()));
        state.invoke_sound_picked(i32::try_from(OWN_FILE_TILE).unwrap());
        assert_eq!(start(), snd("my.wav"));
        let own = own_tile(&state, 0);
        assert_eq!(picked_tile(&state, 0), OWN_FILE_TILE);
        assert_eq!(own.caption, "my.wav");
        assert_eq!(own.hint, snd("my.wav"));
        // The stop sound's own file does not touch the start sound.
        state.set_view(3);
        PICKED.set(Some(snd("end.wav").into()));
        state.invoke_choose_file();
        assert_eq!(page.ctx.config.borrow().config.sounds.stop, snd("end.wav"));
        assert_eq!(start(), snd("my.wav"));
    }

    /// The dialog opens in the folder of the own file now set when it exists, else in the settings
    /// folder.
    #[test]
    fn the_file_dialog_opens_on_the_current_sound_folder_or_the_settings_folder() {
        let (window, page) = sounds_page("pick-start", None);
        let state = window.global::<AudioState>();
        let settings = page
            .ctx
            .layout
            .config_file()
            .parent()
            .unwrap()
            .to_path_buf();
        state.invoke_choose_file();
        assert_eq!(
            PICK_START.take(),
            Some(settings.clone()),
            "a built-in sound"
        );
        // An own file whose folder exists: the dialog opens there.
        let sub = settings.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let here = sub.join("mine.wav").to_string_lossy().into_owned();
        page.with_sound(|sound| *sound = here);
        state.invoke_choose_file();
        assert_eq!(PICK_START.take(), Some(sub));
        let nested = settings.join("gone").join("a.wav");
        page.with_sound(|sound| *sound = nested.to_string_lossy().into_owned());
        state.invoke_choose_file();
        assert_eq!(PICK_START.take(), Some(settings), "its folder is gone");
        assert_eq!(pick_start("my.wav", Path::new("x")), Path::new("x"));
    }

    /// The tile picked for the sound `role` (an index of [`ROLES`]).
    fn picked_tile(state: &AudioState<'_>, role: usize) -> usize {
        let tile = slint::Model::row_data(&state.get_role_tile(), role).unwrap();
        usize::try_from(tile).unwrap()
    }

    fn own_tile(state: &AudioState<'_>, role: usize) -> GalleryTile {
        let tiles = slint::Model::row_data(&state.get_role_tiles(), role).unwrap();
        slint::Model::row_data(&tiles, OWN_FILE_TILE).unwrap()
    }

    /// Every sound of the picker writes its own key; the error default (`boop-3`, not one of the
    /// four tiles there were) is the tile picked, and an own `.ogg` is taken.
    #[test]
    fn every_sound_is_picked_on_its_own_tile_and_key() {
        let (window, page) = sounds_page("roles", None);
        let state = window.global::<AudioState>();
        let sounds = || page.ctx.config.borrow().config.sounds.clone();
        let defaults = Sounds::default();
        for (role, default) in by_role(&defaults).into_iter().enumerate() {
            let at = Builtin::ALL.iter().position(|b| b.id() == default).unwrap();
            assert_eq!(picked_tile(&state, role), builtin_tile(at), "{default}");
        }
        let kill = Builtin::ALL
            .iter()
            .position(|b| *b == Builtin::Kill1)
            .map(builtin_tile)
            .unwrap();
        for role in 0..ROLES.len() {
            state.set_view(i32::try_from(role).unwrap() + 2);
            state.invoke_sound_picked(i32::try_from(kill).unwrap());
            assert_eq!(by_role(&sounds())[role], "kill-1", "role {role}");
            assert_eq!(picked_tile(&state, role), kill);
        }
        state.set_view(5);
        PICKED.set(Some(snd("no.ogg").into()));
        state.invoke_choose_file();
        assert_eq!(sounds().cancel, snd("no.ogg"));
        assert_eq!(picked_tile(&state, 3), OWN_FILE_TILE);
        assert_eq!(own_tile(&state, 3).caption, "no.ogg");
        assert_eq!(
            sounds().error,
            "kill-1",
            "the cancel file left the error sound"
        );
    }

    // Off on every role: start, stop, cancel and error write "off"; text ready only turns its
    // switch off and keeps its sound, which the next pick on it turns back on.
    #[test]
    fn off_is_the_first_tile_of_every_sound() {
        let (window, page) = sounds_page("off-tiles", None);
        let state = window.global::<AudioState>();
        let sounds = || page.ctx.config.borrow().config.sounds.clone();
        for role in 0..ROLES.len() {
            state.set_view(i32::try_from(role).unwrap() + 2);
            let tiles = slint::Model::row_data(&state.get_role_tiles(), role).unwrap();
            assert_eq!(
                slint::Model::row_data(&tiles, OFF_TILE).unwrap().title,
                "Off"
            );
            state.invoke_sound_picked(i32::try_from(OFF_TILE).unwrap());
            assert_eq!(picked_tile(&state, role), OFF_TILE, "role {role}");
        }
        let s = sounds();
        assert_eq!(
            (
                s.start.as_str(),
                s.stop.as_str(),
                s.cancel.as_str(),
                s.error.as_str()
            ),
            ("off", "off", "off", "off")
        );
        assert!(!s.done_enabled);
        assert_eq!(s.done, "rx-25", "the sound it had is kept");
        // The next pick on text ready switches it back on.
        state.set_view(4);
        state.invoke_sound_picked(i32::try_from(builtin_tile(0)).unwrap());
        assert!(sounds().done_enabled);
        assert_eq!(picked_tile(&state, 2), builtin_tile(0));
    }

    #[test]
    fn tile_names_are_the_ids_in_words() {
        assert_eq!(title("apa-hit-3"), "Apa hit 3");
        assert_eq!(title("rx-3-echo"), "Rx 3 echo");
        assert_eq!(title("nine"), "Nine");
        assert_eq!(title(""), "");
    }

    fn refused(name: &str, bad: &str) {
        let (window, page) = sounds_page(name, None);
        PICKED.set(Some(bad.into()));
        window.global::<AudioState>().invoke_choose_file();
        let start = page.ctx.config.borrow().config.sounds.start.clone();
        assert_eq!(start, "apa-hit-3", "{bad} was saved");
        assert_eq!(window.global::<Toast>().get_text(), SOUND_PATH_REFUSED);
    }

    #[test]
    fn a_network_file_is_refused_not_saved() {
        refused("own-file-unc", r"\\nas\s\a.wav");
    }

    #[test]
    fn a_device_name_file_is_refused_not_saved() {
        refused("own-file-nul", "NUL.wav");
    }

    #[test]
    fn the_microphone_is_open_on_the_recording_tab_only() {
        let window = window("mic-tab", None, fake);
        show_audio(&window);
        frame(40);
        let state = window.global::<AudioState>();
        assert_eq!(OPEN.get(), 1);
        for view in [1, 2, 3] {
            state.set_view(view);
            frame(40);
            assert_eq!(OPEN.get(), 0, "view {view}");
            assert!(!state.get_live());
        }
        state.set_view(0);
        frame(40);
        assert_eq!(OPEN.get(), 1);
    }

    #[test]
    fn a_standing_sound_warning_goes_when_the_daemon_does() {
        let (window, page) = sounds_page("warning-link", None);
        let state = window.global::<AudioState>();
        page.ctx.ipc.deliver(&Event::Push(Push::Status {
            phase: oxi_core::machine::Phase::Idle,
            model: String::new(),
            device: oxi_core::device::DeviceChoice::default(),
            backend: String::new(),
            loaded: false,
            warnings: vec!["sounds.start: empty, using rise".into()],
        }));
        assert_ne!(state.get_sound_warning(), "");
        page.ctx.ipc.deliver(&Event::Link(Link::Offline));
        assert_eq!(state.get_sound_warning(), "");
    }

    #[test]
    fn sound_warnings_say_which_sound_and_the_preview_only_when_alone() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(sound_warning(&lines(&["model: x"])), "");
        assert_eq!(
            sound_warning(&lines(&["sounds.preview: not a WAV file"])),
            "Preview: not a WAV file"
        );
        assert_eq!(
            sound_warning(&lines(&[
                "sounds.start: empty, using rise",
                "sounds.stop: empty, using fall",
                "sounds.preview: empty",
            ])),
            "Start sound: empty, using rise\nStop sound: empty, using fall"
        );
        assert_eq!(
            sound_warning(&lines(&[
                "sounds.done: not a WAV file, using rx-25",
                "sounds: not played (no output device)",
            ])),
            "Text ready sound: not a WAV file, using rx-25\nSounds: not played (no output device)"
        );
    }

    #[test]
    fn volume_rounds_to_a_whole_percent() {
        assert_eq!(percent(0.0), 0);
        assert_eq!(percent(0.404), 40);
        assert_eq!(percent(1.0), 100);
        assert_eq!(percent(7.0), 100);
    }

    #[test]
    fn whole_rounds_and_floors_at_zero() {
        for (v, want) in [(0.0, 0), (2.6, 3), (-4.0, 0), (149.5, 150)] {
            assert_eq!(whole(v), want, "{v}");
        }
    }

    /// A folder in the temp dir that goes when the test ends, with or without a panic.
    struct SoundFolder(PathBuf);

    impl SoundFolder {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("oxi-audio-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for SoundFolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A 10 ms mono 16-bit file.
    fn tiny_wav() -> Vec<u8> {
        let data = vec![0_u8; 160];
        let mut out = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        out.extend(16_u32.to_le_bytes());
        out.extend([1, 0, 1, 0]);
        out.extend(8_000_u32.to_le_bytes());
        out.extend(16_000_u32.to_le_bytes());
        out.extend([2, 0, 16, 0]);
        out.extend(b"data");
        out.extend(u32::try_from(data.len()).unwrap().to_le_bytes());
        out.extend(data);
        out
    }

    fn tile_titles(state: &AudioState<'_>, role: usize) -> Vec<String> {
        let tiles = slint::Model::row_data(&state.get_role_tiles(), role).unwrap();
        slint::Model::iter(&tiles)
            .map(|t| t.title.to_string())
            .collect()
    }

    // "Add folder": the files of the folder become tiles after the own file, a tile picks its
    // file by path, and nothing is copied.
    #[test]
    fn adding_a_folder_lists_its_files_as_tiles_and_a_tile_picks_its_file() {
        let dir = SoundFolder::new("tiles");
        std::fs::write(dir.0.join("b.wav"), tiny_wav()).unwrap();
        std::fs::write(dir.0.join("a.wav"), tiny_wav()).unwrap();
        std::fs::write(dir.0.join("notes.txt"), b"x").unwrap();
        let (window, page) = sounds_page("folder", None);
        let state = window.global::<AudioState>();
        state.invoke_choose_folder();
        assert_eq!(
            page.ctx.config.borrow().config.sounds.folder,
            "",
            "cancelled"
        );
        assert_eq!(tile_titles(&state, 0).len(), FOLDER_TILE);
        PICKED_FOLDER.with_borrow_mut(|p| *p = Some(dir.0.clone()));
        state.invoke_choose_folder();
        settle(&page);
        let folder = dir.0.to_string_lossy().into_owned();
        assert_eq!(page.ctx.config.borrow().config.sounds.folder, folder);
        let titles = tile_titles(&state, 0);
        assert_eq!(&titles[FOLDER_TILE..], ["a", "b"], "sorted, no text file");
        assert_eq!(
            state.get_folder_name(),
            dir.0.file_name().unwrap().to_string_lossy().as_ref()
        );
        // The stop sound shown on its own tab picks the second file.
        state.set_view(3);
        state.invoke_sound_picked(i32::try_from(FOLDER_TILE + 1).unwrap());
        assert_eq!(
            page.ctx.config.borrow().config.sounds.stop,
            dir.0.join("b.wav").to_string_lossy()
        );
        assert_eq!(picked_tile(&state, 1), FOLDER_TILE + 1);
        let default = Builtin::ALL
            .iter()
            .position(|b| *b == Builtin::ApaHit3)
            .unwrap();
        assert_eq!(
            picked_tile(&state, 0),
            builtin_tile(default),
            "start is untouched"
        );
        // A file that is not in the folder stays the own file tile.
        PICKED.set(Some(snd("elsewhere.wav").into()));
        state.invoke_choose_file();
        assert_eq!(picked_tile(&state, 1), OWN_FILE_TILE);
        // Remove forgets the folder and its tiles; the files stay.
        state.invoke_remove_folder();
        assert_eq!(page.ctx.config.borrow().config.sounds.folder, "");
        assert_eq!(state.get_folder_name(), "");
        assert_eq!(tile_titles(&state, 0).len(), FOLDER_TILE);
        assert!(dir.0.join("a.wav").is_file());
    }

    // The folder is read when the page is shown: a file added since appears, and a folder that
    // is gone lists nothing.
    #[test]
    fn the_sound_folder_is_read_again_when_the_page_is_shown() {
        let dir = SoundFolder::new("shown");
        let config = format!(
            "version = 2\n[sounds]\nfolder = '{}'\n",
            dir.0.to_string_lossy()
        );
        let (window, page) = sounds_page("folder-shown", Some(&config));
        let state = window.global::<AudioState>();
        settle(&page);
        assert_eq!(tile_titles(&state, 0).len(), FOLDER_TILE, "empty folder");
        std::fs::write(dir.0.join("new.wav"), tiny_wav()).unwrap();
        page.shown();
        settle(&page);
        assert_eq!(&tile_titles(&state, 0)[FOLDER_TILE..], ["new"]);
        std::fs::remove_dir_all(&dir.0).unwrap();
        page.shown();
        settle(&page);
        assert_eq!(tile_titles(&state, 0).len(), FOLDER_TILE, "folder gone");
    }

    // A slow folder never holds the page: it is shown and ticks at once, the tiles come when the
    // scan is done, and a picked folder without a usable file says so then.
    #[test]
    fn a_slow_folder_scan_does_not_hold_the_page() {
        let dir = SoundFolder::new("slow");
        std::fs::write(dir.0.join("late.wav"), tiny_wav()).unwrap();
        let empty = SoundFolder::new("slow-empty");
        let config = format!(
            "version = 2
[sounds]
folder = '{}'
",
            dir.0.to_string_lossy()
        );
        *GATED.lock().unwrap() = Some(dir.0.clone());
        let (window, page) = sounds_page("folder-slow", Some(&config));
        let state = window.global::<AudioState>();
        page.tick();
        assert!(page.scanning.borrow().is_some(), "still reading");
        assert_eq!(tile_titles(&state, 0).len(), FOLDER_TILE, "no tile yet");
        *GATED.lock().unwrap() = None;
        settle(&page);
        assert_eq!(&tile_titles(&state, 0)[FOLDER_TILE..], ["late"]);

        PICKED_FOLDER.with_borrow_mut(|p| *p = Some(empty.0.clone()));
        state.invoke_choose_folder();
        assert_eq!(
            tile_titles(&state, 0).len(),
            FOLDER_TILE,
            "the old files are gone at once"
        );
        settle(&page);
        assert_eq!(
            window.global::<Toast>().get_text(),
            NO_SOUNDS_IN_FOLDER,
            "said when the scan is done"
        );
    }

    #[test]
    fn channel_and_speech_sensitivity_are_saved_and_shown() {
        i_slint_backend_testing::init_no_event_loop();
        let (window, page) = window_on("channel", None, fake);
        window.show().unwrap();
        show_audio(&window);
        let state = window.global::<AudioState>();
        assert_eq!((state.get_channel(), state.get_vad()), (0, 1));
        state.invoke_edited(AudioKey::Channel, 2.0);
        state.invoke_edited(AudioKey::Vad, 2.0);
        let config = page.ctx.config.borrow().config.clone();
        assert_eq!(config.audio.channel, AudioChannel::Right);
        assert_eq!(config.audio.vad_sensitivity, VadSensitivity::High);
        state.invoke_edited(AudioKey::Channel, 1.0);
        state.invoke_edited(AudioKey::Vad, 0.0);
        let config = page.ctx.config.borrow().config.clone();
        assert_eq!(config.audio.channel, AudioChannel::Left);
        assert_eq!(config.audio.vad_sensitivity, VadSensitivity::Low);
    }

    // The meter's stream keeps one channel, so a channel change must reopen it; a plain Mix
    // keeps the device as its key.
    #[test]
    fn the_meter_stream_key_follows_the_device_and_the_channel() {
        let mut opts = Audio {
            device: "USB Mic".into(),
            ..Audio::default()
        };
        assert_eq!(stream_key(&opts), "USB Mic");
        opts.channel = AudioChannel::Left;
        let left = stream_key(&opts);
        opts.channel = AudioChannel::Right;
        assert_ne!(left, stream_key(&opts));
        assert_ne!(left, "USB Mic");
    }
}
