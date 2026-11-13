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
    Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter, default_input_name,
    list_input_devices, play,
};
use slint::{ComponentHandle, ModelRc, SharedString, Timer, TimerMode, VecModel};

use super::toast;
use crate::generated::{AudioKey, AudioState, GalleryTile};
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
    /// Name of the system default input now; read with the list.
    default_input: fn() -> Option<String>,
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
        default_input: default_input_name,
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
        default_input: io.default_input,
        default: RefCell::new(None),
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
    default_input: fn() -> Option<String>,
    /// The system default input at the last listing.
    default: RefCell<Option<String>>,
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

    /// Read the device list and the system default; the picker is only rebuilt when the list
    /// changed (keeps an open popup). True when either changed. Also asks for the daemon's warnings: it does not push them, and a
    /// bad sound file is found on its worker thread after the preview request has returned.
    fn relist(&self, state: &AudioState<'_>, configured: &str) -> bool {
        self.ctx.ipc.send(Request::Status);
        self.listed_at.set((self.now)());
        let listed = (self.list)();
        let error = listed.as_ref().err().map(note).unwrap_or_default();
        state.set_list_error(error.into());
        let default = (self.default_input)();
        let moved = self.default.replace(default.clone()) != default;
        let devices = Devices::new(listed.unwrap_or_default(), configured);
        if *self.devices.borrow() == devices {
            return moved;
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
                && (live.note.is_some()
                    || live.fell_back
                    || live.left_behind(&opts, self.default.borrow().as_deref()))
            {
                // A mic was plugged in: retry the failed open, or leave the default for it; or
                // the Windows default moved off the device the meter runs on (e09).
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
        title: friendly(b.id()).into(),
        // The file name stays as the id, in the tooltip.
        hint: b.id().into(),
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

/// A built-in's name on its tile, by the family of its id: `apa-hit-3` is "Hit 3", `rx-3-echo` is
/// "Click 3 echo". The id itself stays in the tile's tooltip and in the config.
fn friendly(id: &str) -> String {
    let family = |prefix: &str, name: &str| {
        id.strip_prefix(prefix)
            .map(|rest| format!("{name} {}", rest.replace('-', " ")))
    };
    match id {
        "blip-3" => "Blip".into(),
        "tap-light" => "Light tap".into(),
        "tap-wood" => "Wood tap".into(),
        "bubble-double" => "Double bubble".into(),
        "cancel-soft" => "Soft cancel".into(),
        _ => family("apa-hit-", "Hit")
            .or_else(|| family("rx-", "Click"))
            .or_else(|| family("boop-", "Boop"))
            .or_else(|| family("kill-", "Strike"))
            .unwrap_or_else(|| title(id)),
    }
}

/// A built-in's name from its id, `apa-hit-3` as "Apa hit 3": the fallback of [`friendly`]. Its id in words, `apa-hit-3` as "Apa hit 3".
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

    /// The meter runs on the system default (no device configured) but on another device than
    /// `default`, the one now: a concrete endpoint never follows a default change by itself.
    fn left_behind(&self, opts: &Audio, default: Option<&str>) -> bool {
        opts.device.is_empty()
            && self
                .capture
                .as_ref()
                .is_some_and(|r| Some(r.name.as_str()) != default)
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
mod tests;
