//! Sound worker: plays the start, stop, done, cancel and error sounds (`oxi_core::sounds`) on the
//! default output. The clips are loaded when the config arrives, so a custom `.wav` or `.ogg` is
//! read once, and a file that cannot be used falls back to the default sound and comes back as a
//! warning: it never stops a dictation. A sound that cannot be played (no output device, a
//! stream error, an output slow to open) comes back as a warning too, not only in the log.

use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::config::Sounds;
use oxi_core::constants::PLAY_TAIL;
use oxi_core::ipc::{SoundWarning, SoundWarningKind as Kind};
use oxi_core::machine::Sound;
use oxi_core::sounds::{Builtin, Choice, Clip, decode_custom, is_plain_local_path};
use oxi_platform::capture::{CaptureError, Playback, play};

use super::cmd::{Inbox, SoundCmd};

/// A custom file bigger than this is not read (5 s of 96 kHz stereo 24-bit is under 3 MB).
const MAX_FILE: u64 = 8 * 1024 * 1024;

/// An output that takes longer than this to open is reported: the sound starts late, and on
/// a busy machine a start sound can be over before the recording is.
const SLOW_OPEN: Duration = Duration::from_secs(1);

/// A clip that plays until it is dropped.
trait Held {
    /// The stream error that cut the sound short.
    fn failure(&self) -> Option<String>;

    /// How long after the clip is written it is heard (seconds over Bluetooth).
    fn latency(&self) -> Duration {
        Duration::ZERO
    }
}

impl Held for Playback {
    fn failure(&self) -> Option<String> {
        Playback::failure(self)
    }

    fn latency(&self) -> Duration {
        Playback::latency(self)
    }
}

/// A reported latency past this is taken as a driver error, so a stream is never held for ever.
const MAX_LATENCY: Duration = Duration::from_secs(5);

/// When a stream whose clip was written by `clip_end` may be dropped: after the tail, or after
/// the device latency when that is longer, so the end of the clip is still heard.
fn due(clip_end: Instant, stream: &dyn Held) -> Instant {
    let tail = Duration::from_millis(PLAY_TAIL.0);
    clip_end + stream.latency().min(MAX_LATENCY).max(tail)
}

/// Opens the output and starts the clip.
type Open = fn(&[f32], u32) -> Result<Box<dyn Held>, CaptureError>;

fn open_default(clip: &[f32], rate: u32) -> Result<Box<dyn Held>, CaptureError> {
    play(clip, rate).map(|p| Box::new(p) as Box<dyn Held>)
}

pub fn spawn(rx: Receiver<SoundCmd>, inbox: Sender<Inbox>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("sound".into()).spawn(move || {
        let mut player = Player::new();
        loop {
            // Wake when the earliest stream is over, so its device is released at once.
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
                    player.reap();
                    None
                }
                Some(SoundCmd::Config(sounds)) => Some(player.configure(&sounds)),
                Some(SoundCmd::Play(sound)) => {
                    player.play_sound(sound);
                    None
                }
                Some(SoundCmd::Preview {
                    choice,
                    volume_percent,
                }) => Some(player.preview(&choice, f64::from(volume_percent.min(100)) / 100.0)),
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

/// The loaded clips and the streams still playing. A stream stops when it is dropped, so each
/// is kept until its clip is over.
struct Player {
    enabled: bool,
    volume: f64,
    start: Clip,
    stop: Clip,
    done: Clip,
    /// `sounds.done_enabled`: the done sound has its own switch.
    done_enabled: bool,
    cancel: Clip,
    error: Clip,
    /// Warnings for the sounds of the last config.
    config_warnings: Vec<SoundWarning>,
    /// Why the last preview played nothing; cleared by the next good preview or config.
    preview_warning: Option<SoundWarning>,
    /// Why the last sound was not played, was cut short or opened slowly; cleared by the next
    /// one that plays without trouble.
    play_warning: Option<SoundWarning>,
    /// Each stream with the moment its clip is written; see [`due`].
    playing: Vec<(Instant, Box<dyn Held>)>,
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
            start: Clip::builtin(Builtin::Rise),
            stop: Clip::builtin(Builtin::Fall),
            done: Clip::builtin(Builtin::Pop),
            done_enabled: true,
            cancel: Clip::builtin(Builtin::Tick),
            error: Clip::builtin(Builtin::Fault),
            config_warnings: Vec::new(),
            preview_warning: None,
            play_warning: None,
            playing: Vec::new(),
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
        let mut get = |key: &str, text: &str, role: Sound, default: Builtin| {
            let (clip, warning) = load(key, text, role, default);
            lines.extend(warning.map(|line| SoundWarning {
                kind: Kind::Unused(role),
                line,
            }));
            clip
        };
        self.start = get("start", &sounds.start, Sound::Start, Builtin::Rise);
        self.stop = get("stop", &sounds.stop, Sound::Stop, Builtin::Fall);
        self.done = get("done", &sounds.done, Sound::Done, Builtin::Pop);
        self.cancel = get("cancel", &sounds.cancel, Sound::Cancel, Builtin::Tick);
        self.error = get("error", &sounds.error, Sound::Error, Builtin::Fault);
        self.enabled = sounds.enabled;
        self.done_enabled = sounds.done_enabled;
        self.volume = sounds.volume;
        self.config_warnings = lines;
        self.preview_warning = None;
        self.warnings()
    }

    /// Plays `text` once. A sound that cannot be used is not replaced by another: the reason
    /// goes back in the warning lines.
    fn preview(&mut self, text: &str, volume: f64) -> Vec<SoundWarning> {
        // The preview has no role: `fault` is the error role's, and it may be previewed.
        self.preview_warning = match try_load(text, Sound::Error) {
            Ok(clip) => {
                self.play_samples(&clip.scaled(volume), clip.rate);
                None
            }
            Err(why) => Some(warning(Kind::Preview, format!("sounds.preview: {why}"))),
        };
        self.warnings()
    }

    fn play_sound(&mut self, sound: Sound) {
        if !self.enabled || sound == Sound::Done && !self.done_enabled {
            return;
        }
        let clip = match sound {
            Sound::Start => &self.start,
            Sound::Stop => &self.stop,
            Sound::Done => &self.done,
            Sound::Cancel => &self.cancel,
            Sound::Error => &self.error,
        };
        let samples = clip.scaled(self.volume);
        self.play_samples(&samples, clip.rate);
    }

    /// When the earliest stream still open is over.
    fn next_end(&self) -> Option<Instant> {
        self.playing.iter().map(|(end, s)| due(*end, &**s)).min()
    }

    /// Drops the streams whose clip and tail are over, which releases the device. A stream
    /// that met an error on the way becomes the playback warning.
    fn reap(&mut self) {
        let now = Instant::now();
        let failure = self
            .playing
            .iter()
            .filter(|(end, s)| due(*end, &**s) <= now)
            .find_map(|(_, stream)| stream.failure());
        self.playing.retain(|(end, s)| due(*end, &**s) > now);
        if let Some(e) = failure {
            tracing::warn!(error = %e, "sound output failed");
            self.play_warning = Some(warning(
                Kind::NotPlayed,
                format!("sounds: the output stopped ({e})"),
            ));
        }
    }

    fn play_samples(&mut self, samples: &[f32], rate: u32) {
        self.reap();
        // shortcut: the output opens for each sound (tens of ms, seconds when the device is
        // busy); keep one stream and mix if that delay is ever heard on the start sound.
        let asked = Instant::now();
        match (self.open)(samples, rate) {
            Ok(stream) => {
                // The clip starts when the open ends. Counting from `asked` dropped a stream
                // that opened slowly in the middle of its clip (the start sound, items 58-60).
                let started = Instant::now();
                let len = Duration::from_secs_f64(samples.len() as f64 / f64::from(rate));
                self.playing.push((started + len, stream));
                let slow = started.saturating_duration_since(asked);
                tracing::info!(open_ms = slow.as_millis(), "sound played");
                self.play_warning = (slow > SLOW_OPEN).then(|| {
                    let line = format!("sounds: the output took {} ms to open", slow.as_millis());
                    warning(Kind::SlowOpen, line)
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
    }
}

fn warning(kind: Kind, line: String) -> SoundWarning {
    SoundWarning { kind, line }
}

/// The clip for the setting `text` of `key`. A custom file that cannot be used, or an id that
/// is not known, gives the `default` built-in and a warning line.
fn load(key: &str, text: &str, role: Sound, default: Builtin) -> (Clip, Option<String>) {
    match try_load(text, role) {
        Ok(clip) => (clip, None),
        Err(why) => {
            let line = format!("sounds.{key}: {why}, using {}", default.id());
            (Clip::builtin(default), Some(line))
        }
    }
}

/// The clip for the setting `text` of `role`, or why it cannot be used.
fn try_load(text: &str, role: Sound) -> Result<Clip, String> {
    match Choice::parse_for(role, text) {
        Ok(Choice::Builtin(b)) => Ok(Clip::builtin(b)),
        Ok(Choice::Custom(path)) => read_custom(&path),
        Err(e) => Err(e.to_string()),
    }
}

fn read_custom(path: &Path) -> Result<Clip, String> {
    // Before any open: a pipe or a network path can block the read for ever.
    if !is_plain_local_path(path) {
        return Err("not a plain local file path".into());
    }
    let cannot = |e: io::Error| format!("cannot read the file ({})", e.kind());
    if !std::fs::metadata(path).map_err(cannot)?.is_file() {
        return Err("not a regular file".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE + 1).read_to_end(&mut bytes))
        .map_err(cannot)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err("the file is bigger than 8 MB".into());
    }
    decode_custom(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
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
        assert_eq!(clip, Clip::builtin(Builtin::Pop));
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
        assert_eq!((clip, warning), (Clip::builtin(Builtin::Fault), None));
        let _ = std::fs::remove_file(path);
    }

    struct Fake(Option<String>);

    impl Held for Fake {
        fn failure(&self) -> Option<String> {
            self.0.clone()
        }
    }

    const LATENCY: Duration = Duration::from_millis(80);

    fn no_device(_: &[f32], _: u32) -> Result<Box<dyn Held>, CaptureError> {
        Err(CaptureError::NoOutput)
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "an `Open`, whose type returns a Result"
    )]
    fn slow_open(_: &[f32], _: u32) -> Result<Box<dyn Held>, CaptureError> {
        thread::sleep(LATENCY);
        Ok(Box::new(Fake(None)))
    }

    struct Bluetooth;

    impl Held for Bluetooth {
        fn failure(&self) -> Option<String> {
            None
        }

        fn latency(&self) -> Duration {
            Duration::from_secs(2)
        }
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "an `Open`, whose type returns a Result"
    )]
    fn bluetooth(_: &[f32], _: u32) -> Result<Box<dyn Held>, CaptureError> {
        Ok(Box::new(Bluetooth))
    }

    // Review of t09: a Bluetooth output heard 2 s late lost the clip end to the 300 ms tail.
    #[test]
    fn a_slow_device_keeps_the_stream_for_its_latency() {
        let mut player = Player::with_open(bluetooth);
        player.configure(&Sounds::default());
        let asked = Instant::now();
        player.play_sound(Sound::Start);
        let clip = Clip::builtin(Builtin::Rise);
        let len = Duration::from_secs_f64(clip.samples.len() as f64 / f64::from(clip.rate));
        let end = player.next_end().expect("the stream is kept");
        assert!(
            end >= asked + len + Duration::from_secs(2),
            "dropped before it is heard"
        );
        player.reap();
        assert_eq!(player.playing.len(), 1);
    }

    #[allow(
        clippy::unnecessary_wraps,
        reason = "an `Open`, whose type returns a Result"
    )]
    fn breaks_midway(_: &[f32], _: u32) -> Result<Box<dyn Held>, CaptureError> {
        Ok(Box::new(Fake(Some("device lost".into()))))
    }

    // No output device: the sound is not played and the coordinator gets a line for it.
    #[test]
    fn a_missing_output_device_is_reported_not_only_logged() {
        let mut player = Player::with_open(no_device);
        player.configure(&Sounds::default());
        player.play_sound(Sound::Start);
        let warnings = player.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].kind, Kind::NotPlayed);
        assert!(
            warnings[0].line.starts_with("sounds: not played"),
            "{warnings:?}"
        );
        assert!(player.playing.is_empty());
    }

    #[test]
    fn a_disabled_sound_does_not_open_the_output() {
        let mut player = Player::with_open(no_device);
        let mut sounds = Sounds {
            done_enabled: false,
            ..Sounds::default()
        };
        player.configure(&sounds);
        player.play_sound(Sound::Done);
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
        sounds.enabled = false;
        player.configure(&sounds);
        player.play_sound(Sound::Start);
        assert_eq!(player.warnings(), [] as [SoundWarning; 0]);
    }

    // The start sound dropped on a slow open: the stream was due to be dropped from the moment
    // the open began, so a slow open ate the clip. It is due from the moment the open ended.
    #[test]
    fn a_slow_open_does_not_shorten_the_clip() {
        let mut player = Player::with_open(slow_open);
        player.configure(&Sounds::default());
        let asked = Instant::now();
        player.play_sound(Sound::Start);
        let clip = Clip::builtin(Builtin::Rise);
        let len = Duration::from_secs_f64(clip.samples.len() as f64 / f64::from(clip.rate));
        let tail = Duration::from_millis(PLAY_TAIL.0);
        let end = player.next_end().expect("the stream is kept");
        assert!(end >= asked + LATENCY + len + tail, "due too early");
        assert_eq!(
            player.warnings(),
            [] as [SoundWarning; 0],
            "80 ms is not slow"
        );
    }

    #[test]
    fn a_stream_error_becomes_a_warning_when_the_stream_is_reaped() {
        let mut player = Player::with_open(breaks_midway);
        player.configure(&Sounds::default());
        player.play_sound(Sound::Stop);
        assert_eq!(player.warnings(), [] as [SoundWarning; 0], "not over yet");
        let tail = Duration::from_millis(PLAY_TAIL.0);
        player.playing[0].0 = Instant::now().checked_sub(tail).unwrap();
        player.reap();
        let warnings = player.warnings();
        assert_eq!(warnings[0].kind, Kind::NotPlayed);
        assert!(warnings[0].line.contains("device lost"), "{warnings:?}");
        assert!(player.playing.is_empty());
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
        assert_eq!(player.done, Clip::builtin(Builtin::Tick));
        assert_eq!(player.cancel, Clip::builtin(Builtin::Pop));
        assert_eq!(player.error, Clip::builtin(Builtin::Rise));
    }

    #[test]
    fn a_good_custom_file_is_used() {
        let path = temp("good.wav");
        std::fs::write(&path, tiny_wav()).unwrap();
        let (clip, warning) = load("stop", path.to_str().unwrap(), Sound::Stop, Builtin::Fall);
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
            assert_eq!(clip, Clip::builtin(Builtin::Rise), "{setting}");
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
        let warnings = player.preview(gone.to_str().unwrap(), 0.4);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].kind, Kind::Preview);
        assert!(
            warnings[0].line.starts_with("sounds.preview: "),
            "{warnings:?}"
        );
        assert!(player.playing.is_empty());
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
        assert_eq!(player.stop, Clip::builtin(Builtin::Tick));
        assert_eq!(player.start, Clip::builtin(Builtin::Rise));
        assert_eq!(
            player.configure(&Sounds::default()),
            [] as [SoundWarning; 0]
        );
    }
}
