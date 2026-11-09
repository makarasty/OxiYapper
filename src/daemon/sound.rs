//! Sound worker: plays the start and stop sounds (`oxi_core::sounds`) on the default output.
//! The clips are loaded when the config arrives, so a custom `.wav` is read once, and a file that
//! cannot be used falls back to the default sound and comes back as a warning: it never stops
//! a dictation.

use std::io::{self, Read};
use std::path::Path;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use oxi_core::config::Sounds;
use oxi_core::constants::PLAY_TAIL;
use oxi_core::machine::Sound;
use oxi_core::sounds::{Builtin, Choice, Clip, decode_wav, is_plain_local_path};
use oxi_platform::capture::{Playback, play};

use super::cmd::{Inbox, SoundCmd};

/// A custom file bigger than this is not read (5 s of 96 kHz stereo 24-bit is under 3 MB).
const MAX_FILE: u64 = 8 * 1024 * 1024;

pub fn spawn(rx: Receiver<SoundCmd>, inbox: Sender<Inbox>) -> io::Result<JoinHandle<()>> {
    thread::Builder::new().name("sound".into()).spawn(move || {
        let mut player = Player::new();
        loop {
            // Wake when the earliest stream is over, so its device is released at once.
            let cmd = match player.next_end() {
                Some(end) => match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout) => {
                        player.reap();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                },
                None => match rx.recv() {
                    Ok(cmd) => cmd,
                    Err(_) => return,
                },
            };
            let warnings = match cmd {
                SoundCmd::Config(sounds) => Some(player.configure(&sounds)),
                SoundCmd::Play(sound) => {
                    player.play_sound(sound);
                    None
                }
                SoundCmd::Preview {
                    choice,
                    volume_percent,
                } => Some(player.preview(&choice, f64::from(volume_percent.min(100)) / 100.0)),
            };
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
    cancel: Clip,
    error: Clip,
    /// Lines for the start and stop sounds of the last config.
    config_warnings: Vec<String>,
    /// Why the last preview played nothing; cleared by the next good preview or config.
    preview_warning: Option<String>,
    playing: Vec<(Instant, Playback)>,
}

impl Player {
    fn new() -> Self {
        // Disabled until the first config arrives.
        Self {
            enabled: false,
            volume: 0.0,
            start: Clip::builtin(Builtin::Rise),
            stop: Clip::builtin(Builtin::Fall),
            cancel: Clip::builtin(Builtin::Tick),
            error: Clip::builtin(Builtin::Fault),
            config_warnings: Vec::new(),
            preview_warning: None,
            playing: Vec::new(),
        }
    }

    /// Every line the coordinator shows: the config's, then the preview's.
    fn warnings(&self) -> Vec<String> {
        let preview = self.preview_warning.iter().cloned();
        self.config_warnings
            .iter()
            .cloned()
            .chain(preview)
            .collect()
    }

    /// Loads the clips of `sounds`; returns the warning lines, one for each clip that fell back
    /// to its default.
    fn configure(&mut self, sounds: &Sounds) -> Vec<String> {
        let (start, start_warning) = load("start", &sounds.start, Builtin::Rise);
        let (stop, stop_warning) = load("stop", &sounds.stop, Builtin::Fall);
        self.enabled = sounds.enabled;
        self.volume = sounds.volume;
        self.start = start;
        self.stop = stop;
        self.config_warnings = start_warning.into_iter().chain(stop_warning).collect();
        self.preview_warning = None;
        self.warnings()
    }

    /// Plays `text` once. A sound that cannot be used is not replaced by another: the reason
    /// goes back in the warning lines.
    fn preview(&mut self, text: &str, volume: f64) -> Vec<String> {
        self.preview_warning = match try_load(text) {
            Ok(clip) => {
                self.play_samples(&clip.scaled(volume), clip.rate);
                None
            }
            Err(why) => Some(format!("sounds.preview: {why}")),
        };
        self.warnings()
    }

    fn play_sound(&mut self, sound: Sound) {
        if !self.enabled {
            return;
        }
        let clip = match sound {
            Sound::Start => &self.start,
            Sound::Stop => &self.stop,
            Sound::Cancel => &self.cancel,
            Sound::Error => &self.error,
        };
        let samples = clip.scaled(self.volume);
        self.play_samples(&samples, clip.rate);
    }

    /// When the earliest stream still open is over.
    fn next_end(&self) -> Option<Instant> {
        self.playing.iter().map(|(end, _)| *end).min()
    }

    /// Drops the streams whose clip and tail are over, which releases the device.
    fn reap(&mut self) {
        let now = Instant::now();
        self.playing.retain(|(end, _)| *end > now);
    }

    fn play_samples(&mut self, samples: &[f32], rate: u32) {
        self.reap();
        let now = Instant::now();
        // shortcut: the output opens for each sound (tens of ms); keep one stream and mix if that
        // delay is ever heard on the start sound.
        match play(samples, rate) {
            Ok(stream) => {
                let len = Duration::from_secs_f64(samples.len() as f64 / f64::from(rate));
                self.playing
                    .push((now + len + Duration::from_millis(PLAY_TAIL.0), stream));
            }
            Err(e) => tracing::warn!(error = %e, "sound not played"),
        }
    }
}

/// The clip for the setting `text` of `key`. A custom file that cannot be used, or an id that
/// is not known, gives the `default` built-in and a warning line.
fn load(key: &str, text: &str, default: Builtin) -> (Clip, Option<String>) {
    match try_load(text) {
        Ok(clip) => (clip, None),
        Err(why) => {
            let line = format!("sounds.{key}: {why}, using {}", default.id());
            (Clip::builtin(default), Some(line))
        }
    }
}

/// The clip for the setting `text`, or why it cannot be used.
fn try_load(text: &str) -> Result<Clip, String> {
    match Choice::parse(text) {
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
    decode_wav(&bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-sound-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
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
        let (clip, warning) = load("start", "pop", Builtin::Rise);
        assert_eq!(clip, Clip::builtin(Builtin::Pop));
        assert_eq!(warning, None);
    }

    #[test]
    fn a_good_custom_file_is_used() {
        let path = temp("good.wav");
        std::fs::write(&path, tiny_wav()).unwrap();
        let (clip, warning) = load("stop", path.to_str().unwrap(), Builtin::Fall);
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
            let (clip, warning) = load("start", setting, Builtin::Rise);
            assert_eq!(clip, Clip::builtin(Builtin::Rise), "{setting}");
            let warning = warning.unwrap_or_else(|| panic!("no warning for {setting}"));
            assert!(warning.starts_with("sounds.start: "), "{warning}");
            assert!(warning.ends_with(", using rise"), "{warning}");
        }
        let _ = std::fs::remove_file(junk);
    }

    #[test]
    fn a_pipe_a_network_path_or_a_directory_is_refused_without_opening() {
        let dir = temp("dir.wav");
        std::fs::create_dir_all(&dir).unwrap();
        for setting in [r"\\.\pipe\x.wav", r"\\nas\a.wav", dir.to_str().unwrap()] {
            assert!(try_load(setting).is_err(), "{setting}");
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
        assert!(warnings[0].starts_with("sounds.preview: "), "{warnings:?}");
        assert!(player.playing.is_empty());
        assert_eq!(player.configure(&Sounds::default()), [] as [String; 0]);
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
        assert_eq!(player.stop, Clip::builtin(Builtin::Tick));
        assert_eq!(player.start, Clip::builtin(Builtin::Rise));
        assert_eq!(player.configure(&Sounds::default()), [] as [String; 0]);
    }
}
