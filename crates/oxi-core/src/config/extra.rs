//! Settings added by the UI overhaul (arch-plan section 1). Nothing reads them yet; every key
//! is optional, so an old file loads with the defaults here.

use crate::types::Language;
use serde::de::{self, IntoDeserializer, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt::{self, Display};
use std::marker::PhantomData;
use std::ops::RangeInclusive;
use std::str::FromStr;

/// `"auto"` or a value. Serialised as the string `auto` or the bare value
/// (`threads = "auto"`, `threads = 6`). No enum used as `T` may have a variant named `auto`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub enum Auto<T> {
    #[default]
    Auto,
    Set(T),
}

impl<T: Serialize> Serialize for Auto<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Auto => s.serialize_str("auto"),
            Self::Set(v) => v.serialize(s),
        }
    }
}

struct AutoVisitor<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> AutoVisitor<T> {
    fn set<E: de::Error, V: IntoDeserializer<'de, E>>(v: V) -> Result<Auto<T>, E> {
        T::deserialize(v.into_deserializer())
            .map(Auto::Set)
            .map_err(|e| E::custom(format_args!("{e} (or \"auto\")")))
    }
}

impl<'de, T: Deserialize<'de>> Visitor<'de> for AutoVisitor<T> {
    type Value = Auto<T>;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("\"auto\" or a value")
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Auto<T>, E> {
        if v == "auto" {
            Ok(Auto::Auto)
        } else {
            Self::set(v)
        }
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Auto<T>, E> {
        Self::set(v)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Auto<T>, E> {
        Self::set(v)
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Auto<T>, E> {
        Self::set(v)
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Auto<T>, E> {
        Self::set(v)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Auto<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(AutoVisitor(PhantomData))
    }
}

/// Which side of a stereo microphone is used (`audio.channel`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioChannel {
    #[default]
    Mix,
    Left,
    Right,
}

/// How easily the trim gate hears speech (`audio.vad_sensitivity`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VadSensitivity {
    Low,
    #[default]
    Normal,
    High,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decoding {
    Greedy,
    Beam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioCtx {
    Full,
    Fit,
}

/// Pace of typed output (`paste.type_speed`, and per app on a rule).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TypeSpeed {
    #[default]
    Fast,
    Safe,
}

/// Whether a dictation is also written to a file (`output.file_mode`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileMode {
    #[default]
    Off,
    Also,
    Only,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    #[default]
    Md,
    Txt,
}

/// Key of `model.by_language`. `Language` has no `Ord`, so the map orders by its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LanguageKey(pub Language);

impl PartialOrd for LanguageKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LanguageKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_str().cmp(other.0.as_str())
    }
}

/// `model.advanced`: every `auto` keeps what the build does today (arch-plan 1.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    #[serde(deserialize_with = "auto_u8")]
    pub threads: Auto<u8>,
    pub flash_attn: Auto<bool>,
    pub decoding: Auto<Decoding>,
    /// Used with greedy decoding.
    #[serde(deserialize_with = "sat_u8")]
    pub best_of: u8,
    /// Used with beam decoding.
    #[serde(deserialize_with = "sat_u8")]
    pub beam_size: u8,
    pub temperature: Auto<f32>,
    pub temperature_fallback: Auto<bool>,
    /// Put before the dictionary words.
    pub initial_prompt: String,
    pub audio_ctx: Auto<AudioCtx>,
    pub no_speech_threshold: Auto<f32>,
    pub logprob_threshold: Auto<f32>,
    pub entropy_threshold: Auto<f32>,
    pub suppress_non_speech: Auto<bool>,
}

impl Default for Advanced {
    fn default() -> Self {
        Self {
            threads: Auto::Auto,
            flash_attn: Auto::Auto,
            decoding: Auto::Auto,
            best_of: 1,
            beam_size: 5,
            temperature: Auto::Auto,
            temperature_fallback: Auto::Auto,
            initial_prompt: String::new(),
            audio_ctx: Auto::Auto,
            no_speech_threshold: Auto::Auto,
            logprob_threshold: Auto::Auto,
            entropy_threshold: Auto::Auto,
            suppress_non_speech: Auto::Auto,
        }
    }
}

impl Advanced {
    pub const THREADS: RangeInclusive<u8> = 1..=32;
    pub const BEST_OF: RangeInclusive<u8> = 1..=5;
    pub const BEAM_SIZE: RangeInclusive<u8> = 2..=8;
    pub const TEMPERATURE: RangeInclusive<f32> = 0.0..=1.0;
    pub const NO_SPEECH: RangeInclusive<f32> = 0.0..=1.0;
    pub const LOGPROB: RangeInclusive<f32> = -5.0..=0.0;
    pub const ENTROPY: RangeInclusive<f32> = 0.0..=10.0;

    /// Puts every number back in range (NaN becomes `auto`) and says so.
    pub(crate) fn normalise(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        if let Auto::Set(v) = &mut self.threads {
            clamp_int("model.advanced.threads", v, Self::THREADS, &mut said);
        }
        clamp_int(
            "model.advanced.best_of",
            &mut self.best_of,
            Self::BEST_OF,
            &mut said,
        );
        clamp_int(
            "model.advanced.beam_size",
            &mut self.beam_size,
            Self::BEAM_SIZE,
            &mut said,
        );
        for (key, value, range) in [
            ("temperature", &mut self.temperature, Self::TEMPERATURE),
            (
                "no_speech_threshold",
                &mut self.no_speech_threshold,
                Self::NO_SPEECH,
            ),
            (
                "logprob_threshold",
                &mut self.logprob_threshold,
                Self::LOGPROB,
            ),
            (
                "entropy_threshold",
                &mut self.entropy_threshold,
                Self::ENTROPY,
            ),
        ] {
            if let Auto::Set(v) = value {
                let key = format!("model.advanced.{key}");
                if clamp_f32(&key, v, &range, f32::NAN, &mut said) {
                    *value = Auto::Auto;
                }
            }
        }
        said
    }
}

/// Reads any integer and saturates it into `u8`, so `normalise` can clamp and warn instead of
/// the whole file failing to load on `threads = 256` or `best_of = -1`.
fn sat_u8<'de, D: Deserializer<'de>>(d: D) -> Result<u8, D::Error> {
    Ok(saturate(i64::deserialize(d)?))
}

fn auto_u8<'de, D: Deserializer<'de>>(d: D) -> Result<Auto<u8>, D::Error> {
    Ok(match Auto::<i64>::deserialize(d)? {
        Auto::Auto => Auto::Auto,
        Auto::Set(v) => Auto::Set(saturate(v)),
    })
}

fn saturate(v: i64) -> u8 {
    u8::try_from(v.clamp(0, i64::from(u8::MAX))).unwrap_or(u8::MAX)
}

/// A trigger phrase that types a block of text (`[[text.snippets]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snippet {
    pub trigger: String,
    /// May be multi-line.
    pub text: String,
}

/// `[text]`: changes to the dictated text. Every switch is off by default (owner rule).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools, reason = "one switch per config key")]
pub struct Text {
    pub pause_punctuation: bool,
    pub pause_sentence_ms: u32,
    /// 0 means no paragraph break from pauses.
    pub pause_paragraph_ms: u32,
    pub filler_filter: bool,
    /// Lowercase whole words.
    pub fillers: Vec<String>,
    pub voice_commands: bool,
    pub snippets: Vec<Snippet>,
    pub confidence_filter: bool,
    pub confidence_min: f32,
}

impl Default for Text {
    fn default() -> Self {
        Self {
            pause_punctuation: false,
            pause_sentence_ms: 800,
            pause_paragraph_ms: 0,
            filler_filter: false,
            fillers: ["э", "ээ", "эм", "мм", "um", "uh", "erm"]
                .map(String::from)
                .to_vec(),
            voice_commands: false,
            snippets: Vec::new(),
            confidence_filter: false,
            confidence_min: 0.35,
        }
    }
}

impl Text {
    pub const PAUSE_SENTENCE_MS: RangeInclusive<u32> = 300..=3000;
    pub const PAUSE_PARAGRAPH_MS: RangeInclusive<u32> = 1000..=10_000;
    pub const CONFIDENCE_MIN: RangeInclusive<f32> = 0.05..=0.95;

    /// Puts numbers back in range, lowercases and de-duplicates the fillers, drops snippets with
    /// no trigger, and says so.
    pub(crate) fn normalise(&mut self) -> Vec<String> {
        let mut said = Vec::new();
        clamp_int(
            "text.pause_sentence_ms",
            &mut self.pause_sentence_ms,
            Self::PAUSE_SENTENCE_MS,
            &mut said,
        );
        if self.pause_paragraph_ms != 0 {
            clamp_int(
                "text.pause_paragraph_ms",
                &mut self.pause_paragraph_ms,
                Self::PAUSE_PARAGRAPH_MS,
                &mut said,
            );
        }
        clamp_f32(
            "text.confidence_min",
            &mut self.confidence_min,
            &Self::CONFIDENCE_MIN,
            Self::default().confidence_min,
            &mut said,
        );
        let mut fillers: Vec<String> = Vec::with_capacity(self.fillers.len());
        for word in &self.fillers {
            let word = word.trim().to_lowercase();
            if !word.is_empty() && !fillers.contains(&word) {
                fillers.push(word);
            }
        }
        if fillers != self.fillers {
            said.push("text.fillers must be lowercase, non-empty and unique, cleaned".into());
            self.fillers = fillers;
        }
        let before = self.snippets.len();
        self.snippets.retain(|s| !s.trigger.trim().is_empty());
        if self.snippets.len() != before {
            said.push("text.snippets: entries with an empty trigger were dropped".into());
        }
        said
    }
}

/// `[output]`: also write each dictation to a file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Output {
    pub file_mode: FileMode,
    /// Absolute path of a folder.
    pub file_folder: String,
    pub file_format: FileFormat,
}

impl Output {
    /// `file_mode`, but `Off` while there is no folder to write to.
    pub fn effective_mode(&self) -> FileMode {
        if self.file_folder.trim().is_empty() {
            FileMode::Off
        } else {
            self.file_mode
        }
    }

    pub(crate) fn warning(&self) -> Option<String> {
        (self.file_mode != FileMode::Off && self.file_folder.trim().is_empty()).then(|| {
            "output.file_mode is on but output.file_folder is empty, so nothing is written".into()
        })
    }
}

/// An sRGB colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// `ui.accent`: the theme's own accent, or one `#RRGGBB` colour. A preset is only a named hex
/// in the UI, so presets can change with no migration.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Accent {
    #[default]
    Default,
    Hex(Rgb),
}

/// The string is neither `default` nor `#RRGGBB`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is neither \"default\" nor a #RRGGBB colour")]
pub struct BadAccent(pub String);

impl FromStr for Accent {
    type Err = BadAccent;
    fn from_str(s: &str) -> Result<Self, BadAccent> {
        if s == "default" {
            return Ok(Self::Default);
        }
        let bad = || BadAccent(s.to_owned());
        let hex = s
            .strip_prefix('#')
            .filter(|h| h.len() == 6)
            .ok_or_else(bad)?;
        let byte = |i: usize| {
            hex.get(i..i + 2)
                .filter(|p| p.bytes().all(|b| b.is_ascii_hexdigit()))
                .and_then(|p| u8::from_str_radix(p, 16).ok())
                .ok_or_else(bad)
        };
        Ok(Self::Hex(Rgb(byte(0)?, byte(2)?, byte(4)?)))
    }
}

impl Display for Accent {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::Hex(Rgb(r, g, b)) => write!(f, "#{r:02X}{g:02X}{b:02X}"),
        }
    }
}

impl Serialize for Accent {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

/// Lenient: a bad string loads as `default`; `ConfigFile` reports it as a warning.
impl<'de> Deserialize<'de> for Accent {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(String::deserialize(d)?.parse().unwrap_or_default())
    }
}

/// Puts `v` in `range` and says so.
pub(crate) fn clamp_int<T: Ord + Copy + Display>(
    key: &str,
    v: &mut T,
    range: RangeInclusive<T>,
    said: &mut Vec<String>,
) {
    let old = *v;
    *v = old.clamp(*range.start(), *range.end());
    if *v != old {
        said.push(format!(
            "{key} = {old} is outside {}..{}, using {v}",
            range.start(),
            range.end()
        ));
    }
}

/// Puts `v` in `range` (NaN becomes `default`) and says so. Returns true when the result is
/// still NaN, which tells an `Auto` caller to fall back to `auto`.
pub(crate) fn clamp_f32(
    key: &str,
    v: &mut f32,
    range: &RangeInclusive<f32>,
    default: f32,
    said: &mut Vec<String>,
) -> bool {
    let old = *v;
    if range.contains(&old) {
        return false;
    }
    *v = if old.is_nan() {
        default
    } else {
        old.clamp(*range.start(), *range.end())
    };
    if v.is_nan() {
        said.push(format!("{key} = {old} is not a number, using auto"));
        return true;
    }
    said.push(format!(
        "{key} = {old} is outside {}..{}, using {v}",
        range.start(),
        range.end()
    ));
    false
}
