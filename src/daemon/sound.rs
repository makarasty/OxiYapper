//! Sound worker: plays the start, stop, done, cancel and error sounds (`oxi_core::sounds`) on the
//! default output. The clips are loaded when the config arrives, so a custom `.wav` or `.ogg` is
//! read once, and a file that cannot be used falls back to the default sound and comes back as a
//! warning: it never stops a dictation. A sound that cannot be played (no output device, a
//! stream error, an output slow to open) comes back as a warning too, not only in the log.
//!
//! One output stream serves every sound: it opens at the first sound, mixes the sounds that
//! overlap, and closes [`KEEP`] after the last one ends, so an idle daemon holds no device (a
//! Bluetooth headset can sleep) and a dictation pays the open once, not for each of its sounds.

use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::audio::resample;
use oxi_core::config::Sounds;
use oxi_core::constants::PLAY_TAIL;
use oxi_core::ipc::{SoundWarning, SoundWarningKind as Kind};
use oxi_core::machine::Sound;
use oxi_core::sounds::{Builtin, Choice, Clip, load_custom};
use oxi_platform::capture::{CaptureError, Output};

use super::cmd::{Inbox, SoundCmd};

/// An output that takes longer than this to open is reported: the sound starts late, and on
/// a busy machine a start sound can be over before the recording is.
const SLOW_OPEN: Duration = Duration::from_secs(1);

/// How long the output stays open after the last sound is over.
const KEEP: Duration = Duration::from_secs(30);

/// The open output.
trait Sink {
    /// The rate the clips must have.
    fn rate(&self) -> u32;

    /// Starts `clip` (mono, at [`Self::rate`]) now, over whatever still plays.
    fn play(&self, clip: Arc<[f32]>);

    /// The stream error that stopped the output.
    fn failure(&self) -> Option<String>;

    /// How long after a clip is written it is heard (seconds over Bluetooth).
    fn latency(&self) -> Duration {
        Duration::ZERO
    }

    /// Is this still the system default output? When it is not, the next sound opens the new one.
    fn is_default(&self) -> bool {
        true
    }
}

impl Sink for Output {
    fn rate(&self) -> u32 {
        Output::rate(self)
    }

    fn play(&self, clip: Arc<[f32]>) {
        Output::play(self, clip);
    }

    fn failure(&self) -> Option<String> {
        Output::failure(self)
    }

    fn latency(&self) -> Duration {
        Output::latency(self)
    }

    fn is_default(&self) -> bool {
        Output::is_default(self)
    }
}

/// A reported latency past this is taken as a driver error, so a stream is never held for ever.
const MAX_LATENCY: Duration = Duration::from_secs(5);

/// The open output and when the last clip written to it is over.
struct Live {
    sink: Box<dyn Sink>,
    end: Instant,
    /// A clip was written and `heard_at` has not been reached yet: a stream error now cuts a
    /// sound. One that comes later, or before the first clip, loses nothing.
    playing: bool,
}

impl Live {
    /// When the last clip is heard to its end: the end of the clip plus the tail, or the device
    /// latency when that is longer (so the end of the clip is still heard).
    fn heard_at(&self) -> Instant {
        let tail = Duration::from_millis(PLAY_TAIL.0);
        self.end + self.sink.latency().min(MAX_LATENCY).max(tail)
    }

    /// When the output may close: [`Self::heard_at`] plus [`KEEP`].
    fn close_at(&self) -> Instant {
        self.heard_at() + KEEP
    }
}

/// Opens the output.
type Open = fn() -> Result<Box<dyn Sink>, CaptureError>;

fn open_default() -> Result<Box<dyn Sink>, CaptureError> {
    Output::open().map(|o| Box::new(o) as Box<dyn Sink>)
}

pub fn spawn(rx: Receiver<SoundCmd>, inbox: Sender<Inbox>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("sound".into()).spawn(move || {
        let mut player = Player::new();
        loop {
            // Wake when the output is due to close, so its device is released at once.
            let cmd = match player.next_end() {
                Some(end) => match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                    Ok(cmd) => Some(cmd),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(cmd) => Some(cmd),
                    Err(_) => return,
                },
            };
            let before = player.play_warning.clone();
            let mut warnings = match cmd {
                None => {
                    player.reap(Instant::now());
                    None
                }
                Some(SoundCmd::Config(sounds)) => Some(player.configure(&sounds)),
                Some(SoundCmd::Play(sound)) => {
                    player.play_sound(sound, Instant::now());
                    None
                }
                Some(SoundCmd::Preview {
                    choice,
                    volume_percent,
                }) => Some(player.preview(
                    &choice,
                    f64::from(volume_percent.min(100)) / 100.0,
                    Instant::now(),
                )),
            };
            // A play failure shows at once; it is sent again only when it changes.
            if warnings.is_none() && player.play_warning != before {
                warnings = Some(player.warnings());
            }
            if let Some(warnings) = warnings
                && inbox.send(Inbox::SoundWarnings(warnings)).is_err()
            {
                return;
            }
        }
    })
}

/// A role's clip as the output wants it: at its rate, scaled by the volume.
type Cached = Option<(u32, Arc<[f32]>)>;

/// The loaded clips and the output they play on.
struct Player {
    enabled: bool,
    volume: f64,
    /// `None`: the sound is Off and plays nothing.
    start: Option<Clip>,
    stop: Option<Clip>,
    done: Option<Clip>,
    /// `sounds.done_enabled`: the done sound has its own switch.
    done_enabled: bool,
    cancel: Option<Clip>,
    error: Option<Clip>,
    /// The roles' clips ready for the output (start, stop, done, cancel, error): resampled and
    /// scaled once, until the next config or a different output rate.
    ready: [Cached; 5],
    /// Warnings for the sounds of the last config.
    config_warnings: Vec<SoundWarning>,
    /// Why the last preview played nothing; cleared by the next good preview or config.
    preview_warning: Option<SoundWarning>,
    /// Why the last sound was not played, was cut short or opened slowly; cleared by the next
    /// one that plays without trouble.
    play_warning: Option<SoundWarning>,
    /// The output, while it is open.
    live: Option<Live>,
    open: Open,
}

impl Player {
    fn new() -> Self {
        Self::with_open(open_default)
    }

    fn with_open(open: Open) -> Self {
        // Disabled until the first config arrives.
        Self {
            enabled: false,
            volume: 0.0,
            start: Some(Clip::builtin(Builtin::default_for(Sound::Start))),
            stop: Some(Clip::builtin(Builtin::default_for(Sound::Stop))),
            done: Some(Clip::builtin(Builtin::default_for(Sound::Done))),
            done_enabled: true,
            cancel: Some(Clip::builtin(Builtin::default_for(Sound::Cancel))),
            error: Some(Clip::builtin(Builtin::default_for(Sound::Error))),
            ready: Default::default(),
            config_warnings: Vec::new(),
            preview_warning: None,
            play_warning: None,
            live: None,
            open,
        }
    }

    /// Every warning the coordinator shows: the config's, the preview's, then the playback's.
    fn warnings(&self) -> Vec<SoundWarning> {
        self.config_warnings
            .iter()
            .chain(&self.preview_warning)
            .chain(&self.play_warning)
            .cloned()
            .collect()
    }

    /// Loads the clips of `sounds`; returns the warnings, one for each clip that fell back to
    /// its default.
    fn configure(&mut self, sounds: &Sounds) -> Vec<SoundWarning> {
        let mut lines = Vec::new();
        let mut get = |key: &str, text: &str, role: Sound| {
            let (clip, warning) = load(key, text, role, Builtin::default_for(role));
            lines.extend(warning.map(|line| SoundWarning {
                kind: Kind::Unused(role),
                line,
            }));
            clip
        };
        self.start = get("start", &sounds.start, Sound::Start);
        self.stop = get("stop", &sounds.stop, Sound::Stop);
        self.done = get("done", &sounds.done, Sound::Done);
        self.cancel = get("cancel", &sounds.cancel, Sound::Cancel);
        self.error = get("error", &sounds.error, Sound::Error);
        self.enabled = sounds.enabled;
        self.done_enabled = sounds.done_enabled;
        self.volume = sounds.volume;
        self.ready = Default::default();
        self.config_warnings = lines;
        self.preview_warning = None;
        self.warnings()
    }

    /// Plays `text` once. A sound that cannot be used is not replaced by another: the reason
    /// goes back in the warning lines.
    fn preview(&mut self, text: &str, volume: f64, now: Instant) -> Vec<SoundWarning> {
        // The preview has no role: `fault` is the error role's, and it may be previewed.
        self.preview_warning = match try_load(text, Sound::Error) {
            Ok(clip) => {
                if let Some(clip) = clip {
                    self.play_clip(&clip, volume, None, now);
                }
                None
            }
            Err(why) => Some(warning(Kind::Preview, format!("sounds.preview: {why}"))),
        };
        self.warnings()
    }

    fn play_sound(&mut self, sound: Sound, now: Instant) {
        if !self.enabled || sound == Sound::Done && !self.done_enabled {
            return;
        }
        // The clip is taken out for the call and put back: `play_clip` borrows the whole player.
        let (slot, clip) = match sound {
            Sound::Start => (0, self.start.take()),
            Sound::Stop => (1, self.stop.take()),
            Sound::Done => (2, self.done.take()),
            Sound::Cancel => (3, self.cancel.take()),
            Sound::Error => (4, self.error.take()),
        };
        // Off: nothing to play, and the output is not opened.
        if let Some(clip) = &clip {
            self.play_clip(clip, self.volume, Some(slot), now);
        }
        let back = match sound {
            Sound::Start => &mut self.start,
            Sound::Stop => &mut self.stop,
            Sound::Done => &mut self.done,
            Sound::Cancel => &mut self.cancel,
            Sound::Error => &mut self.error,
        };
        *back = clip;
    }

    /// When the worker must look at the output next, `None` while it is closed: when the sound
    /// playing is over (an error that cut it is reported then, not [`KEEP`] later), else when the
    /// output is due to close.
    fn next_end(&self) -> Option<Instant> {
        self.live.as_ref().map(|l| {
            if l.playing {
                l.heard_at()
            } else {
                l.close_at()
            }
        })
    }

    /// Closes the output when it is due or failed, which releases the device. A stream error
    /// becomes the playback warning only when it came while a sound was playing; an idle output
    /// that dies loses nothing and is replaced by the next sound.
    fn reap(&mut self, now: Instant) {
        let Some(live) = &mut self.live else {
            return;
        };
        let failure = live.sink.failure();
        let cut = failure.is_some() && live.playing;
        if failure.is_none() && live.playing && live.heard_at() <= now {
            live.playing = false;
        }
        if failure.is_some() || live.close_at() <= now {
            self.live = None;
        }
        if let Some(e) = failure {
            tracing::warn!(error = %e, cut, "sound output failed");
            if cut {
                self.play_warning = Some(warning(
                    Kind::NotPlayed,
                    format!("sounds: the output stopped ({e})"),
                ));
            }
        }
    }

    /// The open output, opened now when there is none (or the last one failed, timed out or is no
    /// longer the system default: a device change drops the stream, arch-plan 7.7). Gives the
    /// output's rate and the time a clip written now starts, after any open.
    fn output(&mut self, now: Instant) -> Option<(u32, Instant)> {
        self.reap(now);
        if self.live.as_ref().is_some_and(|l| !l.sink.is_default()) {
            tracing::info!("sound output is not the default any more, opening the new one");
            self.live = None;
        }
        let mut now = now;
        if self.live.is_none() {
            let asked = Instant::now();
            match (self.open)() {
                Ok(sink) => {
                    let slow = asked.elapsed();
                    now += slow;
                    tracing::info!(open_ms = slow.as_millis(), "sound output opened");
                    self.play_warning = (slow > SLOW_OPEN).then(|| {
                        let line =
                            format!("sounds: the output took {} ms to open", slow.as_millis());
                        warning(Kind::SlowOpen, line)
                    });
                    self.live = Some(Live {
                        sink,
                        end: now,
                        playing: false,
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sound not played");
                    self.play_warning = Some(warning(
                        Kind::NotPlayed,
                        format!("sounds: not played ({e})"),
                    ));
                }
            }
        } else if self
            .play_warning
            .as_ref()
            .is_some_and(|w| w.kind == Kind::SlowOpen)
        {
            // The slow open is over: this sound plays without trouble.
            self.play_warning = None;
        }
        self.live.as_ref().map(|live| (live.sink.rate(), now))
    }

    /// Plays `clip` at `volume`. A role's clip (`slot`) is resampled and scaled once; a preview
    /// is not kept.
    fn play_clip(&mut self, clip: &Clip, volume: f64, slot: Option<usize>, now: Instant) {
        let Some((rate, now)) = self.output(now) else {
            return;
        };
        let cached = slot
            .and_then(|i| self.ready[i].as_ref())
            .filter(|(r, _)| *r == rate)
            .map(|(_, samples)| Arc::clone(samples));
        let samples = match cached {
            Some(samples) => samples,
            None => match ready(clip, volume, rate) {
                Ok(samples) => {
                    if let Some(i) = slot {
                        self.ready[i] = Some((rate, Arc::clone(&samples)));
                    }
                    samples
                }
                Err(e) => {
                    tracing::warn!(error = %e, "sound not played");
                    self.play_warning = Some(warning(
                        Kind::NotPlayed,
                        format!("sounds: not played ({e})"),
                    ));
                    return;
                }
            },
        };
        if let Some(live) = &mut self.live {
            let len = Duration::from_secs_f64(samples.len() as f64 / f64::from(rate));
            live.end = live.end.max(now + len);
            live.playing = true;
            live.sink.play(samples);
        }
    }
}

/// `clip` scaled by `volume` and at `rate`.
fn ready(clip: &Clip, volume: f64, rate: u32) -> Result<Arc<[f32]>, String> {
    let scaled = clip.scaled(volume);
    if clip.rate == rate {
        return Ok(scaled.into());
    }
    resample(&scaled, clip.rate, rate)
        .map(Into::into)
        .map_err(|e| e.to_string())
}

fn warning(kind: Kind, line: String) -> SoundWarning {
    SoundWarning { kind, line }
}

/// The clip for the setting `text` of `key`, `None` for Off. A custom file that cannot be used,
/// or an id that is not known, gives the `default` built-in and a warning line.
fn load(key: &str, text: &str, role: Sound, default: Builtin) -> (Option<Clip>, Option<String>) {
    match try_load(text, role) {
        Ok(clip) => (clip, None),
        Err(why) => {
            let line = format!("sounds.{key}: {why}, using {}", default.id());
            (Some(Clip::builtin(default)), Some(line))
        }
    }
}

/// The clip for the setting `text` of `role` (`None`: Off), or why it cannot be used.
fn try_load(text: &str, role: Sound) -> Result<Option<Clip>, String> {
    match Choice::parse_for(role, text) {
        Ok(Choice::Builtin(b)) => Ok(Some(Clip::builtin(b))),
        Ok(Choice::Custom(path)) => load_custom(&path).map(Some),
        Ok(Choice::Off) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};

    use super::*;
    use crate::log::scratch::scratch_dir;

    /// A path inside a folder of its own that goes when the test ends.
    fn temp(name: &str) -> std::path::PathBuf {
        scratch_dir(&format!("oxi-sound-{}-{name}", std::process::id())).join(name)
    }

    /// A 10 ms mono 16-bit file.
    fn tiny_wav() -> Vec<u8> {
        let data: Vec<u8> = (0..80_i16).flat_map(|i| (i * 100).to_le_bytes()).collect();
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

    #[test]
    fn a_built_in_loads_without_a_warning() {
        let (clip, warning) = load("start", "pop", Sound::Start, Builtin::Rise);
        assert_eq!(
            clip,
            Some(Clip::builtin(Builtin::Rise)),
            "legacy pop is the start default"
        );
        assert_eq!(warning, None);
    }

    // A config the settings can write (an .ogg path, `fault` for the error) never falls back
    // for its name.
    #[test]
    fn every_role_loads_what_the_config_accepts() {
        let path = temp("fine.ogg");
        std::fs::write(&path, b"OggS not a stream").unwrap();
        let (_, warning) = load("done", path.to_str().unwrap(), Sound::Done, Builtin::Pop);
        assert!(warning.unwrap().contains("Ogg Vorbis"), "judged by content");
        let (clip, warning) = load("error", "fault", Sound::Error, Builtin::Fault);
        assert_eq!((clip, warning), (Some(Clip::builtin(Builtin::Fault)), None));
        let _ = std::fs::remove_file(path);
    }

    thread_local! {
        /// What the fake output saw on this test's thread: opens, then the clips it was given.
        static OPENS: Cell<usize> = const { Cell::new(0) };
        static PLAYED: RefCell<Vec<usize>> = const { RefCell::new(Vec::new()) };
        static BROKEN: Cell<bool> = const { Cell::new(false) };
        static LATENCY_MS: Cell<u64> = const { Cell::new(0) };
        static OPEN_DELAY_MS: Cell<u64> = const { Cell::new(0) };
        /// The system default moved to another device since the output was opened.
        static MOVED: Cell<bool> = const { Cell::new(false) };
    }

    fn reset() {
        OPENS.set(0);
        PLAYED.with_borrow_mut(Vec::clear);
        BROKEN.set(false);
        LATENCY_MS.set(0);
        OPEN_DELAY_MS.set(0);
        MOVED.set(false);
    }

    struct Fake;

    impl Sink for Fake {
        fn rate(&self) -> u32 {
            48_000
        }

        fn play(&self, clip: Arc<[f32]>) {
            PLAYED.with_borrow_mut(|p| p.push(clip.len()));
        }

        fn failure(&self) -> Option<String> {
            BROKEN.get().then(|| "device lost".to_owned())
        }

        fn latency(&self) -> Duration {
            Duration::from_millis(LATENCY_MS.get())
        }

        fn is_default(&self) -> bool {
            !MOVED.get()
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "an `Open`, whose type returns a Result"
    )]
    fn fake_open() -> Result<Box<dyn Sink>, CaptureError> {
        OPENS.set(OPENS.get() + 1);
        // A new output is opened on the new default.
        MOVED.set(false);
        thread::sleep(Duration::from_millis(OPEN_DELAY_MS.get()));
        Ok(Box::new(Fake))
    }

    fn no_device() -> Result<Box<dyn Sink>, CaptureError> {
        Err(CaptureError::NoOutput)
    }

    /// The worker's two looks at an output after a sound: when the sound is heard to its end, then
    /// when the output is due to close. Gives the second.
    fn close_due(player: &mut Player) -> Instant {
        let over = player.next_end().expect("open after a sound");
        player.reap(over);
        player.next_end().expect("still open in the keep time")
    }

    fn ready_player() -> Player {
        reset();
        let mut player = Player::with_open(fake_open);
        player.configure(&Sounds::default());
        player
    }

    // No output device: the sound is not played and the coordinator gets a line for it.
    #[test]
    fn a_missing_output_device_is_reported_not_only_logged() {
        let mut player = Player::with_open(no_device);
        player.configure(&Sounds::default());
        player.play_sound(Sound::Start, Instant::now());
        let warnings = player.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].kind, Kind::NotPlayed);
        assert!(
            warnings[0].line.starts_with("sounds: not played"),
            "{warnings:?}"
        );
        assert!(player.live.is_none());
    }

    #[test]
    fn a_disabled_sound_does_not_open_the_output() {
        let mut player = Player::with_open(no_device);
        let mut sounds = Sounds {
            done_enabled: false,
            ..Sounds::default()
        };
        player.configure(&sounds);
        player.play_sound(Sound::Done, Instant::now());
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
        sounds.enabled = false;
        player.configure(&sounds);
        player.play_sound(Sound::Start, Instant::now());
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
    }

    // Off per role: nothing is loaded, nothing is played and the output is not opened.
    #[test]
    fn an_off_sound_plays_nothing_and_never_opens_the_output() {
        let mut player = Player::with_open(no_device);
        let off = || String::from("off");
        let sounds = Sounds {
            start: off(),
            stop: off(),
            done: off(),
            cancel: off(),
            error: off(),
            ..Sounds::default()
        };
        assert_eq!(player.configure(&sounds), [] as [SoundWarning; 0]);
        for sound in [
            Sound::Start,
            Sound::Stop,
            Sound::Done,
            Sound::Cancel,
            Sound::Error,
        ] {
            player.play_sound(sound, Instant::now());
        }
        assert_eq!(
            player.warnings(),
            [] as [SoundWarning; 0],
            "no open was tried"
        );
        assert!(player.live.is_none());
        assert_eq!(
            player.preview("off", 0.5, Instant::now()),
            [] as [SoundWarning; 0]
        );
        // Only the Off role is silent: the others still reach the output.
        let sounds = Sounds {
            stop: off(),
            ..Sounds::default()
        };
        player.configure(&sounds);
        player.play_sound(Sound::Stop, Instant::now());
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
        player.play_sound(Sound::Start, Instant::now());
        assert_eq!(player.warnings()[0].kind, Kind::NotPlayed);
    }

    #[test]
    fn done_cancel_and_error_come_from_the_config() {
        let mut player = Player::new();
        let sounds = Sounds {
            done: "tick".into(),
            cancel: "pop".into(),
            error: "rise".into(),
            ..Sounds::default()
        };
        assert_eq!(player.configure(&sounds), [] as [SoundWarning; 0]);
        // The old ids are the default of the role they stand in.
        assert_eq!(player.done, Some(Clip::builtin(Builtin::Pop)));
        assert_eq!(player.cancel, Some(Clip::builtin(Builtin::Tick)));
        assert_eq!(player.error, Some(Clip::builtin(Builtin::Fault)));
    }

    #[test]
    fn a_good_custom_file_is_used() {
        let path = temp("good.wav");
        std::fs::write(&path, tiny_wav()).unwrap();
        let (clip, warning) = load("stop", path.to_str().unwrap(), Sound::Stop, Builtin::Fall);
        let clip = clip.unwrap();
        assert_eq!((clip.rate, clip.samples.len()), (8_000, 80));
        assert_eq!(warning, None);
        let _ = std::fs::remove_file(path);
    }

    // A missing, unreadable or unknown sound never reaches the recording: the default plays
    // and one warning line says why.
    #[test]
    fn a_missing_or_bad_custom_file_falls_back_to_the_default_and_warns() {
        let junk = temp("junk.wav");
        std::fs::write(&junk, b"not a wav").unwrap();
        let missing = temp("missing.wav");
        for setting in [
            junk.to_str().unwrap(),
            missing.to_str().unwrap(),
            "no-such-id",
        ] {
            let (clip, warning) = load("start", setting, Sound::Start, Builtin::Rise);
            assert_eq!(clip, Some(Clip::builtin(Builtin::Rise)), "{setting}");
            let warning = warning.unwrap_or_else(|| panic!("no warning for {setting}"));
            assert!(warning.starts_with("sounds.start: "), "{warning}");
            let using = format!(", using {}", Builtin::Rise.id());
            assert!(warning.ends_with(&using), "{warning}");
        }
        let _ = std::fs::remove_file(junk);
    }

    #[test]
    fn a_pipe_a_network_path_or_a_directory_is_refused_without_opening() {
        let dir = temp("dir.wav");
        std::fs::create_dir_all(&dir).unwrap();
        for setting in [r"\\.\pipe\x.wav", r"\\nas\a.wav", dir.to_str().unwrap()] {
            assert!(try_load(setting, Sound::Start).is_err(), "{setting}");
        }
        let _ = std::fs::remove_dir(dir);
    }

    // A preview of a sound that cannot be used plays nothing and says why.
    #[test]
    fn a_failed_preview_reports_and_the_next_config_clears_it() {
        let mut player = Player::new();
        let gone = temp("gone-preview.wav");
        let warnings = player.preview(gone.to_str().unwrap(), 0.4, Instant::now());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].kind, Kind::Preview);
        assert!(
            warnings[0].line.starts_with("sounds.preview: "),
            "{warnings:?}"
        );
        assert!(player.live.is_none());
        assert_eq!(
            player.configure(&Sounds::default()),
            [] as [SoundWarning; 0]
        );
    }

    #[test]
    fn configure_reports_each_fallback_and_keeps_the_rest() {
        let mut player = Player::new();
        let sounds = Sounds {
            start: temp("gone.wav").to_str().unwrap().into(),
            stop: "tick".into(),
            ..Sounds::default()
        };
        let warnings = player.configure(&sounds);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].kind, Kind::Unused(Sound::Start));
        assert_eq!(player.stop, Some(Clip::builtin(Builtin::Fall)));
        assert_eq!(player.start, Some(Clip::builtin(Builtin::Rise)));
        assert_eq!(
            player.configure(&Sounds::default()),
            [] as [SoundWarning; 0]
        );
    }

    // P7: a dictation plays start, stop and done; the output opens for the first and serves all.
    #[test]
    fn sounds_within_the_keep_time_open_the_output_once() {
        let mut player = ready_player();
        let t0 = Instant::now();
        for (i, sound) in [Sound::Start, Sound::Stop, Sound::Done]
            .into_iter()
            .enumerate()
        {
            player.play_sound(sound, t0 + Duration::from_secs(8 * i as u64));
        }
        assert_eq!(OPENS.get(), 1);
        assert_eq!(PLAYED.with_borrow(Vec::len), 3);
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
    }

    // The output is not kept for ever: 30 s after the last sound is over it closes, and the
    // next sound opens it again.
    #[test]
    fn a_sound_after_the_keep_time_opens_the_output_again() {
        let mut player = ready_player();
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        let due = close_due(&mut player);
        assert!(due >= t0 + KEEP, "closes at once, not after {KEEP:?}");
        // Not yet due: nothing closes.
        player.reap(due.checked_sub(Duration::from_millis(1)).unwrap());
        assert!(player.live.is_some());
        player.reap(due);
        assert!(player.live.is_none());
        assert_eq!(player.next_end(), None, "no timer while closed");
        player.play_sound(Sound::Stop, due + Duration::from_secs(1));
        assert_eq!(OPENS.get(), 2);
        // 31 s after the stop sound, through `play_sound` alone (no reap in between).
        player.play_sound(Sound::Done, due + Duration::from_secs(1 + 31));
        assert_eq!(OPENS.get(), 3);
    }

    #[test]
    fn overlapping_sounds_both_reach_the_one_output() {
        let mut player = ready_player();
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        player.play_sound(Sound::Stop, t0 + Duration::from_millis(10));
        assert_eq!(OPENS.get(), 1);
        assert_eq!(
            PLAYED.with_borrow(Vec::len),
            2,
            "the first was not cut for the second"
        );
        assert!(player.live.is_some());
    }

    // A clip is resampled and scaled once, then handed on as it is.
    #[test]
    fn a_roles_clip_is_prepared_once_until_the_config_changes() {
        let mut player = ready_player();
        player.play_sound(Sound::Start, Instant::now());
        let first = player.ready[0]
            .as_ref()
            .map(|(_, s)| Arc::clone(s))
            .unwrap();
        player.play_sound(Sound::Start, Instant::now());
        let second = player.ready[0]
            .as_ref()
            .map(|(_, s)| Arc::clone(s))
            .unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        player.configure(&Sounds::default());
        assert!(
            player.ready[0].is_none(),
            "a new config drops what was prepared"
        );
    }

    // A dead stream is dropped and the next sound opens a new one; the loss is said once.
    #[test]
    fn a_failed_output_is_dropped_and_the_next_sound_opens_a_new_one() {
        let mut player = ready_player();
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        BROKEN.set(true);
        player.reap(t0 + Duration::from_secs(1));
        assert!(player.live.is_none());
        let warnings = player.warnings();
        assert_eq!(warnings[0].kind, Kind::NotPlayed);
        assert!(warnings[0].line.contains("device lost"), "{warnings:?}");
        BROKEN.set(false);
        player.play_sound(Sound::Stop, t0 + Duration::from_secs(2));
        assert_eq!(OPENS.get(), 2);
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
    }

    // A device that fails while it is idle in the keep time is replaced at the next sound, not
    // written to.
    #[test]
    fn a_sound_on_a_failed_output_goes_to_a_new_one() {
        let mut player = ready_player();
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        BROKEN.set(true);
        player.play_sound(Sound::Stop, t0 + Duration::from_secs(1));
        assert_eq!(
            OPENS.get(),
            2,
            "opened a new output instead of writing to the dead one"
        );
    }

    // Review of t09: a Bluetooth output heard 2 s late lost the clip end to the 300 ms tail.
    #[test]
    fn a_slow_device_keeps_the_stream_for_its_latency() {
        let mut player = ready_player();
        LATENCY_MS.set(2_000);
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        let clip = Clip::builtin(Builtin::default_for(Sound::Start));
        let len = Duration::from_secs_f64(clip.samples.len() as f64 / f64::from(clip.rate));
        let due = close_due(&mut player);
        assert!(
            due >= t0 + len + Duration::from_secs(2) + KEEP,
            "closed before it is heard"
        );
    }

    // The open took time: the clip is due from the end of the open, and 80 ms is not slow.
    #[test]
    fn a_slow_open_does_not_shorten_the_clip() {
        let mut player = ready_player();
        OPEN_DELAY_MS.set(80);
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        let clip = Clip::builtin(Builtin::default_for(Sound::Start));
        let len = Duration::from_secs_f64(clip.samples.len() as f64 / f64::from(clip.rate));
        let tail = Duration::from_millis(PLAY_TAIL.0);
        let due = close_due(&mut player);
        // The sound starts when the open is over, so the 80 ms of the open count in front of it.
        assert!(
            due >= t0 + Duration::from_millis(80) + len + tail + KEEP,
            "due too early: the clip was counted from before the open"
        );
        assert_eq!(
            player.warnings(),
            [] as [SoundWarning; 0],
            "80 ms is not slow"
        );
    }
    // The system default moved to another device (a headset plugged in, the user's pick): the
    // next sound opens the new default instead of writing to the old device.
    #[test]
    fn a_sound_after_the_default_output_moved_opens_the_new_one() {
        let mut player = ready_player();
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        player.play_sound(Sound::Stop, t0 + Duration::from_secs(2));
        assert_eq!(OPENS.get(), 1, "same default: the stream is reused");
        MOVED.set(true);
        player.play_sound(Sound::Done, t0 + Duration::from_secs(4));
        assert_eq!(OPENS.get(), 2);
        assert_eq!(PLAYED.with_borrow(Vec::len), 3);
        assert_eq!(player.warnings(), [] as [SoundWarning; 0], "not an error");
    }

    // A stream error is the player's warning only when it cut a sound, and it is looked at when
    // that sound is over, not when the output closes.
    #[test]
    fn an_error_is_reported_when_it_cuts_a_sound_and_not_when_the_output_was_idle() {
        let mut player = ready_player();
        let t0 = Instant::now();
        player.play_sound(Sound::Start, t0);
        let over = player.next_end().expect("a sound is playing");
        assert!(over < t0 + KEEP, "looked at when the sound is over");
        BROKEN.set(true);
        player.reap(over);
        assert!(player.live.is_none());
        let warnings = player.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].line.contains("device lost"), "{warnings:?}");

        // The same output dying after its sound was heard loses nothing: no warning, and the
        // next sound opens a new one.
        BROKEN.set(false);
        player.play_sound(Sound::Stop, t0 + Duration::from_secs(5));
        let due = close_due(&mut player);
        BROKEN.set(true);
        player.reap(due.checked_sub(Duration::from_secs(1)).unwrap());
        assert!(player.live.is_none());
        assert_eq!(
            player.warnings(),
            [] as [SoundWarning; 0],
            "nothing was lost"
        );
        BROKEN.set(false);
        player.play_sound(Sound::Done, due);
        assert_eq!(OPENS.get(), 3);
    }
}
