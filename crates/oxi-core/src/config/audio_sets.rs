//! The named audio choices of the Audio page (hold item 41): noise suppression strength,
//! "Don't cut my words" and the switches remembered per microphone.

use super::extra::{AudioChannel, VadSensitivity};
use super::schema::Audio;
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// `audio.denoise_strength`: how much of the cleaned signal replaces the original.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DenoiseStrength {
    Light,
    Medium,
    #[default]
    Strong,
    /// Per dictation, by the room noise in its first 300 ms ([`Noise::for_floor`]).
    Auto,
}

/// The one noise choice the Audio page shows: `denoise` and `denoise_strength` together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Noise {
    Off,
    Light,
    Medium,
    Strong,
    Auto,
}

impl Noise {
    pub const ALL: [Self; 5] = [
        Self::Off,
        Self::Light,
        Self::Medium,
        Self::Strong,
        Self::Auto,
    ];

    /// Share of the cleaned signal in the mix; `None` for Auto (decided per dictation).
    #[must_use]
    pub const fn mix(self) -> Option<f32> {
        match self {
            Self::Off => Some(0.0),
            Self::Light => Some(0.35),
            Self::Medium => Some(0.65),
            Self::Strong => Some(1.0),
            Self::Auto => None,
        }
    }

    /// What a room this noisy needs (its floor in dBFS): a quiet room none (the bench shows
    /// denoise can make Whisper worse), a louder one a blend. Starting points, not measured.
    #[must_use]
    pub const fn for_floor(floor_db: i16) -> Self {
        if floor_db < -55 {
            Self::Off
        } else if floor_db < -45 {
            Self::Light
        } else {
            Self::Medium
        }
    }
}

/// "Don't cut my words": named pairs of `vad_sensitivity` and `tail_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgePreset {
    Normal,
    /// Hears quieter starts and ends, keeps recording longer after the key is let go.
    Careful,
    Tight,
    /// Any other pair, set by hand.
    Custom,
}

impl EdgePreset {
    pub const ALL: [Self; 4] = [Self::Normal, Self::Careful, Self::Tight, Self::Custom];
    const PAIRS: [(Self, VadSensitivity, u32); 3] = [
        (Self::Normal, VadSensitivity::Normal, 100),
        (Self::Careful, VadSensitivity::High, 300),
        (Self::Tight, VadSensitivity::Low, 50),
    ];
}

/// The switches remembered per microphone (`audio.device_sets`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeviceSet {
    pub gain_db: f64,
    pub denoise: bool,
    pub denoise_strength: DenoiseStrength,
    pub cut_rumble: bool,
    pub channel: AudioChannel,
    pub vad_sensitivity: VadSensitivity,
}

impl Default for DeviceSet {
    fn default() -> Self {
        Audio::default().device_set()
    }
}

impl Audio {
    #[must_use]
    pub fn noise(&self) -> Noise {
        match (self.denoise, self.denoise_strength) {
            (false, _) => Noise::Off,
            (true, DenoiseStrength::Light) => Noise::Light,
            (true, DenoiseStrength::Medium) => Noise::Medium,
            (true, DenoiseStrength::Strong) => Noise::Strong,
            (true, DenoiseStrength::Auto) => Noise::Auto,
        }
    }

    /// Off keeps the strength, so turning noise suppression on again brings it back.
    pub fn set_noise(&mut self, noise: Noise) {
        self.denoise = noise != Noise::Off;
        self.denoise_strength = match noise {
            Noise::Off => self.denoise_strength,
            Noise::Light => DenoiseStrength::Light,
            Noise::Medium => DenoiseStrength::Medium,
            Noise::Strong => DenoiseStrength::Strong,
            Noise::Auto => DenoiseStrength::Auto,
        };
    }

    #[must_use]
    pub fn edges(&self) -> EdgePreset {
        EdgePreset::PAIRS
            .iter()
            .find(|&&(_, vad, tail)| vad == self.vad_sensitivity && tail == self.tail_ms)
            .map_or(EdgePreset::Custom, |&(p, ..)| p)
    }

    /// Custom changes nothing: it only names a hand-set pair.
    pub fn set_edges(&mut self, preset: EdgePreset) {
        if let Some(&(_, vad, tail)) = EdgePreset::PAIRS.iter().find(|(p, ..)| *p == preset) {
            self.vad_sensitivity = vad;
            self.tail_ms = tail;
        }
    }

    #[must_use]
    pub fn device_set(&self) -> DeviceSet {
        DeviceSet {
            gain_db: self.gain_db,
            denoise: self.denoise,
            denoise_strength: self.denoise_strength,
            cut_rumble: self.cut_rumble,
            channel: self.channel,
            vad_sensitivity: self.vad_sensitivity,
        }
    }

    /// The `device_sets` key of the microphone in use: the picked one, or with "System default"
    /// the default's name now (`default_input`; `None`, not known: the empty key).
    fn set_key<'a>(&'a self, default_input: Option<&'a str>) -> &'a str {
        if self.device.is_empty() {
            default_input.unwrap_or_default()
        } else {
            &self.device
        }
    }

    /// Saves the switches for the microphone in use ([`Self::set_key`]), while `per_device` is on.
    pub fn remember_device(&mut self, default_input: Option<&str>) {
        if self.per_device {
            let key = self.set_key(default_input).to_owned();
            self.device_sets.insert(key, self.device_set());
        }
    }

    /// Puts back the switches saved for the microphone in use ([`Self::set_key`]). False when it
    /// has none (or `per_device` is off): the current switches stay, and are saved for it.
    pub fn recall_device(&mut self, default_input: Option<&str>) -> bool {
        let saved = if self.per_device {
            self.device_sets.get(self.set_key(default_input)).cloned()
        } else {
            None
        };
        let Some(set) = saved else {
            self.remember_device(default_input);
            return false;
        };
        self.put(&set);
        true
    }

    fn put(&mut self, set: &DeviceSet) {
        self.gain_db = set.gain_db;
        self.denoise = set.denoise;
        self.denoise_strength = set.denoise_strength;
        self.cut_rumble = set.cut_rumble;
        self.channel = set.channel;
        self.vad_sensitivity = set.vad_sensitivity;
    }

    /// The switches a recording on the device it opened (`opened`, its name) runs with: that
    /// device's saved set while `per_device` is on, else these. Decided at every open, so with
    /// "System default" a default Windows moved to brings its own set, settings open or not.
    #[must_use]
    pub fn for_device(&self, opened: &str) -> Cow<'_, Self> {
        match self.device_sets.get(opened).filter(|_| self.per_device) {
            Some(set) if *set != self.device_set() => {
                let mut audio = self.clone();
                audio.put(set);
                Cow::Owned(audio)
            }
            _ => Cow::Borrowed(self),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigFile;

    #[test]
    fn noise_is_one_choice_over_two_keys_and_old_on_is_strong() {
        let mut a = Audio::default();
        assert_eq!(a.noise(), Noise::Off, "off by default (SPEC)");
        let old = ConfigFile::parse("[audio]\ndenoise = true\n").unwrap();
        assert_eq!(old.config.audio.noise(), Noise::Strong);
        for n in Noise::ALL {
            a.set_noise(n);
            assert_eq!(a.noise(), n);
        }
        a.set_noise(Noise::Light);
        a.set_noise(Noise::Off);
        a.denoise = true;
        assert_eq!(a.noise(), Noise::Light, "off keeps the strength");
        let auto = ConfigFile::parse("[audio]\ndenoise = true\ndenoise_strength = \"auto\"\n");
        assert_eq!(auto.unwrap().config.audio.noise(), Noise::Auto);
    }

    #[test]
    fn floor_picks_the_level_and_mix_grows_with_it() {
        assert_eq!(Noise::for_floor(-70), Noise::Off);
        assert_eq!(Noise::for_floor(-50), Noise::Light);
        assert_eq!(Noise::for_floor(-30), Noise::Medium);
        let mixes: Vec<f32> = [Noise::Off, Noise::Light, Noise::Medium, Noise::Strong]
            .iter()
            .map(|n| n.mix().unwrap())
            .collect();
        assert!(mixes.windows(2).all(|w| w[0] < w[1]), "{mixes:?}");
        assert_eq!(
            Noise::Strong.mix(),
            Some(1.0),
            "Strong is the old full denoise"
        );
        assert_eq!(Noise::Auto.mix(), None);
    }

    #[test]
    fn edge_presets_set_both_keys_and_others_read_custom() {
        let mut a = Audio::default();
        assert_eq!(a.edges(), EdgePreset::Normal, "Normal is today's default");
        a.set_edges(EdgePreset::Careful);
        assert_eq!(
            (a.edges(), a.vad_sensitivity, a.tail_ms),
            (EdgePreset::Careful, VadSensitivity::High, 300)
        );
        a.tail_ms = 250;
        assert_eq!(a.edges(), EdgePreset::Custom);
        a.set_edges(EdgePreset::Custom);
        assert_eq!(a.tail_ms, 250, "Custom changes nothing");
        a.set_edges(EdgePreset::Tight);
        assert_eq!(a.edges(), EdgePreset::Tight);
    }

    #[test]
    fn device_sets_come_back_with_their_microphone_only_while_on() {
        let mut a = Audio {
            gain_db: 6.0,
            ..Audio::default()
        };
        a.remember_device(None);
        assert!(a.device_sets.is_empty(), "nothing is stored while off");
        a.per_device = true;
        a.device = "Headset".into();
        assert!(!a.recall_device(None), "a new mic keeps the switches");
        assert_eq!(
            a.device_sets["Headset"].gain_db, 6.0,
            "and saves them for it"
        );
        a.device = "Laptop".into();
        a.recall_device(Some("Headset"));
        a.gain_db = -3.0;
        a.cut_rumble = true;
        a.remember_device(Some("Headset"));
        a.device = "Headset".into();
        assert!(a.recall_device(None));
        assert_eq!((a.gain_db, a.cut_rumble), (6.0, false));
        a.device = "Laptop".into();
        assert!(a.recall_device(None));
        assert_eq!((a.gain_db, a.cut_rumble), (-3.0, true));
        a.per_device = false;
        a.device = "Headset".into();
        assert!(
            !a.recall_device(None),
            "off: the sets are kept but not used"
        );
        assert_eq!(a.gain_db, -3.0);
    }

    /// d04c: with "System default" the set is the default device's, both on the page (the key is
    /// the default's name) and at the open (`for_device`), so a default Windows moved switches it.
    #[test]
    fn system_default_keys_the_set_by_the_default_device() {
        let mut a = Audio {
            per_device: true,
            gain_db: 6.0,
            ..Audio::default()
        };
        a.remember_device(Some("Headset"));
        a.gain_db = -3.0;
        assert!(
            !a.recall_device(Some("Laptop")),
            "a new default keeps the switches"
        );
        assert_eq!(a.device_sets["Laptop"].gain_db, -3.0);
        assert!(a.recall_device(Some("Headset")), "back to the headset");
        assert_eq!(a.gain_db, 6.0);
        assert!(
            !a.device_sets.contains_key(""),
            "{:?}",
            a.device_sets.keys()
        );
        a.remember_device(None);
        assert!(
            a.device_sets.contains_key(""),
            "default not known: the empty key"
        );

        assert_eq!(
            a.for_device("Laptop").gain_db,
            -3.0,
            "the opened device's set"
        );
        assert_eq!(a.for_device("Headset").gain_db, 6.0);
        assert!(
            matches!(a.for_device("Webcam"), Cow::Borrowed(_)),
            "no set: these"
        );
        a.per_device = false;
        assert_eq!(
            a.for_device("Laptop").gain_db,
            6.0,
            "off: never a saved set"
        );
    }

    #[test]
    fn device_sets_round_trip_and_a_removed_one_leaves_the_file() {
        let mut file = ConfigFile::parse("version = 2\n").unwrap();
        let audio = &mut file.config.audio;
        audio.per_device = true;
        audio.device = "USB Mic".into();
        audio.gain_db = 4.0;
        audio.remember_device(None);
        let text = file.to_toml_string().unwrap();
        let mut again = ConfigFile::parse(&text).unwrap();
        assert_eq!(again.config.audio.device_sets["USB Mic"].gain_db, 4.0);
        again.config.audio.device_sets.clear();
        let text = again.to_toml_string().unwrap();
        assert!(!text.contains("USB Mic\"]"), "{text}");
    }
}
