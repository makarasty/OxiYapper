//! Config schema v1 (ARCHITECTURE section 6). Every field has a default, so a partial
//! or empty file loads. Unknown keys are not rejected here; `ConfigFile` keeps them.

use super::extra::{
    Accent, Advanced, AudioChannel, FileMode, LanguageKey, Output, Text, TypeSpeed, VadSensitivity,
    clamp_int,
};
use crate::device::DeviceChoice;
use crate::machine::{Mode, Sound};
use crate::sounds::{Builtin, Choice};
use crate::types::Language;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ops::RangeInclusive;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub hotkey: Hotkey,
    pub audio: Audio,
    pub model: Model,
    pub paste: Paste,
    pub overlay: Overlay,
    pub sounds: Sounds,
    pub history: History,
    pub system: System,
    pub debug: Debug,
    pub ui: Ui,
    pub text: Text,
    pub output: Output,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkey {
    /// Key names; "Win" is Cmd on macOS.
    pub chord: Vec<String>,
    pub mode: Mode,
    /// Esc cancels a recording. Off by default (R2.1).
    pub esc_cancels: bool,
    /// What a key that is not part of the chord does while recording.
    pub other_key: OtherKey,
    /// A press this soon after a hold ended continues it. 0 means off.
    pub hold_gap_ms: u32,
}

impl Hotkey {
    pub const HOLD_GAP_MS: RangeInclusive<u32> = 100..=1500;

    /// Puts a non-zero `hold_gap_ms` back in range and says so.
    pub(crate) fn clamp_hold_gap(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        if self.hold_gap_ms != 0 {
            clamp_int(
                "hotkey.hold_gap_ms",
                &mut self.hold_gap_ms,
                Self::HOLD_GAP_MS,
                &mut said,
            );
        }
        said
    }
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            chord: vec!["Ctrl".into(), "Win".into()],
            mode: Mode::Push,
            esc_cancels: false,
            other_key: OtherKey::Auto,
            hold_gap_ms: 400,
        }
    }
}

/// `hotkey.other_key`: a key outside the chord always reaches the system; this says whether it
/// also cancels the recording (R2.1).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OtherKey {
    /// No speech yet (VAD): the key is a shortcut and the recording is cancelled quietly.
    /// Speech already heard: the recording goes on.
    #[default]
    Auto,
    /// Never cancels.
    Never,
    /// Always cancels.
    Always,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools, reason = "one switch per config key")]
pub struct Audio {
    /// Empty means the system default device.
    pub device: String,
    pub gain_db: f64,
    pub normalize: bool,
    pub denoise: bool,
    pub trim: bool,
    pub tail_ms: u32,
    pub max_seconds: u32,
    /// Scales only the level meter shown (overlay, settings), never the audio whisper gets.
    pub meter_sensitivity_db: f32,
    pub channel: AudioChannel,
    pub vad_sensitivity: VadSensitivity,
}

impl Audio {
    pub const METER_SENSITIVITY_DB: RangeInclusive<f32> = -20.0..=20.0;

    /// Puts `meter_sensitivity_db` back in range (NaN becomes the default) and says so.
    pub(crate) fn clamp_meter_sensitivity(&mut self) -> Option<String> {
        let v = self.meter_sensitivity_db;
        let range = Self::METER_SENSITIVITY_DB;
        if range.contains(&v) {
            return None;
        }
        self.meter_sensitivity_db = if v.is_nan() {
            0.0
        } else {
            v.clamp(*range.start(), *range.end())
        };
        Some(format!(
            "audio.meter_sensitivity_db = {v} is outside {}..{}, using {}",
            range.start(),
            range.end(),
            self.meter_sensitivity_db
        ))
    }
}

impl Default for Audio {
    fn default() -> Self {
        Self {
            device: String::new(),
            gain_db: 0.0,
            normalize: true,
            denoise: false,
            trim: true,
            tail_ms: 100,
            max_seconds: 3600,
            meter_sensitivity_db: 0.0,
            channel: AudioChannel::Mix,
            vad_sensitivity: VadSensitivity::Normal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Model {
    /// Registry id, or "custom".
    pub id: String,
    pub custom_path: String,
    pub gpu: DeviceChoice,
    pub language: Language,
    /// The two languages used last, most recent first, for the picker's short list.
    pub recent_languages: [Language; 2],
    /// 0 means never unload (older files keep that meaning).
    pub idle_unload_minutes: u32,
    /// Unload as soon as a dictation ends; wins over `idle_unload_minutes`.
    pub unload_immediately: bool,
    /// Load the model when the daemon starts instead of at the first dictation.
    pub preload_at_start: bool,
    /// Model per dictation language: a registry id or "custom". Empty means one model for all.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub by_language: BTreeMap<LanguageKey, String>,
    pub advanced: Advanced,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: "turbo".into(),
            custom_path: String::new(),
            gpu: DeviceChoice::Auto,
            language: Language::RU,
            recent_languages: [Language::RU, Language::EN],
            idle_unload_minutes: 5,
            unload_immediately: false,
            preload_at_start: false,
            by_language: BTreeMap::new(),
            advanced: Advanced::default(),
        }
    }
}

impl Model {
    /// Makes `recent_languages` two different real languages (never auto): keeps the good
    /// entries in order and fills the rest from the defaults, and says so.
    pub(crate) fn normalise_recent_languages(&mut self) -> Option<String> {
        let got = self.recent_languages;
        if got[0] != got[1] && !got.contains(&Language::AUTO) {
            return None;
        }
        let mut fixed = Vec::with_capacity(2);
        for lang in got.into_iter().chain(Self::default().recent_languages) {
            if lang != Language::AUTO && !fixed.contains(&lang) {
                fixed.push(lang);
            }
        }
        self.recent_languages = [fixed[0], fixed[1]];
        Some(format!(
            "model.recent_languages must be two different languages and not auto, using {}, {}",
            fixed[0].as_str(),
            fixed[1].as_str()
        ))
    }

    /// Picks `lang` and moves it to the front of `recent_languages`; auto is not a language
    /// to pick again, so it leaves the list alone.
    pub fn use_language(&mut self, lang: Language) {
        self.language = lang;
        if lang != Language::AUTO && self.recent_languages[0] != lang {
            self.recent_languages = [lang, self.recent_languages[0]];
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PasteMode {
    Clipboard,
    Type,
}

/// Per-app override. App identity: exe name on Windows, bundle id on macOS, `WM_CLASS` on X11.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasteRule {
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PasteMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chord: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_delay_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub type_speed: Option<TypeSpeed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<Language>,
    /// A registry id or "custom".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_file: Option<FileMode>,
    /// `false` means no dictionary prompt in this app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dictionary: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacements: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voice_commands: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filler_filter: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause_punctuation: Option<bool>,
}

impl PasteRule {
    /// Every key a rule can have; a save drops the optional ones a rule no longer sets.
    pub(crate) const KEYS: [&str; 13] = [
        "app",
        "mode",
        "chord",
        "restore_delay_ms",
        "type_speed",
        "language",
        "model",
        "to_file",
        "dictionary",
        "replacements",
        "voice_commands",
        "filler_filter",
        "pause_punctuation",
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paste {
    pub mode: PasteMode,
    pub restore_delay_ms: u32,
    pub trailing_space: bool,
    /// Dictated text stays in the OS clipboard history (Win+V). Cloud upload is never allowed.
    pub keep_in_clipboard_history: bool,
    /// Pace of typed output; a rule can set its own.
    pub type_speed: TypeSpeed,
    pub rules: Vec<PasteRule>,
}

impl Default for Paste {
    fn default() -> Self {
        Self {
            mode: PasteMode::Clipboard,
            restore_delay_ms: 300,
            trailing_space: true,
            keep_in_clipboard_history: true,
            type_speed: TypeSpeed::Fast,
            // Remote desktop drops a synthetic Ctrl+V, so type there.
            rules: vec![PasteRule {
                app: "mstsc.exe".into(),
                mode: Some(PasteMode::Type),
                ..PasteRule::default()
            }],
        }
    }
}

/// Screen corner or edge centre the overlay sits at.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlayPosition {
    #[default]
    BottomRight,
    BottomCentre,
    BottomLeft,
    TopRight,
    TopCentre,
    TopLeft,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overlay {
    pub enabled: bool,
    pub position: OverlayPosition,
    pub hide_over_fullscreen: bool,
}

impl Default for Overlay {
    fn default() -> Self {
        Self {
            enabled: true,
            position: OverlayPosition::BottomRight,
            hide_over_fullscreen: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sounds {
    pub enabled: bool,
    /// 0 to 1.
    pub volume: f64,
    /// A built-in id (`Builtin::id`), the path of a `.wav` or `.ogg` file, or `off`. Same for the
    /// other sound keys below.
    pub start: String,
    pub stop: String,
    /// Played when the text is ready and pasted; on by default.
    pub done_enabled: bool,
    pub done: String,
    /// Played when a recording is cancelled.
    pub cancel: String,
    /// Played on an error; `boop-3` is the built-in error sound (`fault` in older files).
    pub error: String,
    /// Absolute path of a folder of `.wav` or `.ogg` files the sound pickers list.
    pub folder: String,
}

impl Sounds {
    pub const VOLUME: RangeInclusive<f64> = 0.0..=1.0;

    /// Puts `volume` back in range (NaN becomes the default) and a sound that is neither a
    /// built-in id nor a `.wav` or `.ogg` path back to its default, and says so.
    pub(crate) fn normalise(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        let default = Self::default();
        let v = self.volume;
        if !Self::VOLUME.contains(&v) {
            self.volume = if v.is_nan() {
                default.volume
            } else {
                v.clamp(*Self::VOLUME.start(), *Self::VOLUME.end())
            };
            said.push(format!(
                "sounds.volume = {v} is outside 0..1, using {}",
                self.volume
            ));
        }
        for (key, role, value, fallback) in [
            ("start", Sound::Start, &mut self.start, default.start),
            ("stop", Sound::Stop, &mut self.stop, default.stop),
            ("done", Sound::Done, &mut self.done, default.done),
            ("cancel", Sound::Cancel, &mut self.cancel, default.cancel),
            ("error", Sound::Error, &mut self.error, default.error),
        ] {
            if Choice::parse_for(role, value).is_err() {
                said.push(format!(
                    "sounds.{key} = \"{value}\" is not a built-in sound or a .wav or .ogg path, using {fallback}"
                ));
                *value = fallback;
            }
        }
        said
    }
}

impl Default for Sounds {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.5,
            start: Builtin::ApaHit3.id().into(),
            stop: Builtin::ApaHit2.id().into(),
            done_enabled: true,
            done: Builtin::Rx25.id().into(),
            cancel: Builtin::CancelSoft.id().into(),
            error: Builtin::Boop3.id().into(),
            folder: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    /// On by default (R1.3); a file with an explicit value keeps it.
    pub enabled: bool,
    /// Size cap of the folder; the oldest entries go first. 0 means no cap.
    pub max_bytes: u64,
    /// Keep the last this many days (today included). 0 means no age limit.
    pub max_days: u32,
    /// Absolute path of a folder (may be another drive) to keep the app's `OxiYapper history`
    /// subfolder in; empty or relative means `history/` in the data directory.
    pub folder: String,
    /// The History player jumps over pauses.
    pub skip_pauses: bool,
}

impl Default for History {
    fn default() -> Self {
        Self {
            enabled: true,
            max_bytes: 2_147_483_648,
            max_days: 30,
            folder: String::new(),
            skip_pauses: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct System {
    pub autostart: bool,
    pub update_check: bool,
}

impl Default for System {
    fn default() -> Self {
        Self {
            autostart: true,
            update_check: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Debug {
    /// Dictated text goes into logs only when true.
    pub log_text: bool,
}

/// Colour scheme of the settings window and the overlay.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Follow the OS light/dark setting.
    #[default]
    System,
    Dark,
    Light,
}

/// Language of the settings window and tray text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AppLanguage {
    #[default]
    En,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub theme: Theme,
    pub language: AppLanguage,
    pub accent: Accent,
    /// Human names of apps with a per-app rule, by lowercase exe: what the settings window shows
    /// for a rule picked from a scan, so the name outlives the window (UI rule 30). Display only.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub app_names: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::{Config, FileMode, Hotkey, Language, OtherKey, PasteMode, PasteRule, TypeSpeed};

    #[test]
    fn an_old_file_gets_the_new_defaults() {
        let c: Config = toml::from_str(
            "[hotkey]\nchord = [\"Ctrl\"]\n[sounds]\nstart = \"pop\"\n[history]\nmax_days = 7\n",
        )
        .unwrap();
        assert!(!c.hotkey.esc_cancels);
        assert_eq!(c.hotkey.other_key, OtherKey::Auto);
        assert!(c.history.enabled);
        assert_eq!(c.history.max_days, 7);
        assert!(c.sounds.done_enabled);
        assert_eq!(c.sounds.done, "rx-25");
        assert_eq!(c.sounds.cancel, "cancel-soft");
        assert_eq!(c.sounds.error, "boop-3");
        let off: Config = toml::from_str("[history]\nenabled = false\n").unwrap();
        assert!(!off.history.enabled);
    }

    #[test]
    fn explicit_values_survive_a_round_trip() {
        let mut c = Config::default();
        c.hotkey.esc_cancels = true;
        c.history.enabled = false;
        c.sounds.done_enabled = false;
        c.sounds.done = "tick".into();
        c.sounds.cancel = r"C:\Sounds\no.ogg".into();
        c.sounds.error = "beep.OGG".into();
        for other in [OtherKey::Auto, OtherKey::Never, OtherKey::Always] {
            c.hotkey.other_key = other;
            let text = toml::to_string(&c).unwrap();
            let back: Config = toml::from_str(&text).unwrap();
            assert_eq!(back, c);
        }
        assert_eq!(c.sounds.normalise(), Vec::<String>::new());
        let hotkey = Hotkey::default();
        assert!(!hotkey.esc_cancels && hotkey.other_key == OtherKey::Auto);
    }

    #[test]
    fn bad_sound_values_fall_back() {
        let mut c = Config::default();
        c.sounds.done = "nope".into();
        c.sounds.cancel = "fault".into();
        assert_eq!(c.sounds.normalise().len(), 2);
        assert_eq!(
            (c.sounds.done.as_str(), c.sounds.cancel.as_str()),
            ("rx-25", "cancel-soft")
        );
    }

    #[test]
    fn rule_keys_name_every_field() {
        // A new field fails to compile here until it is set, then fails here until it is in KEYS.
        let full = PasteRule {
            app: String::new(),
            mode: Some(PasteMode::Type),
            chord: Some(Vec::new()),
            restore_delay_ms: Some(0),
            type_speed: Some(TypeSpeed::Safe),
            language: Some(Language::RU),
            model: Some(String::new()),
            to_file: Some(FileMode::Also),
            dictionary: Some(false),
            replacements: Some(false),
            voice_commands: Some(false),
            filler_filter: Some(false),
            pause_punctuation: Some(false),
        };
        let table = toml::Table::try_from(&full).unwrap();
        let keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut want = PasteRule::KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(keys, want);
    }
}
