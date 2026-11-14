//! The Audio page's suggestions (hold item 41): the loudness numbers of the newest history
//! entries, read off the UI thread, and the words each row's lightbulb says. What to suggest is
//! decided in `oxi_core::suggest`; nothing changes until the person presses Apply.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use oxi_core::audio::Levels;
use oxi_core::config::{EdgePreset, Noise};
use oxi_core::history::Meta;
use oxi_core::learn::dismiss_key;
use oxi_core::suggest::{AudioAdvice, MicLevel, RECENT};

/// At most this many entry files are opened for one pass, numbers or not (older entries have
/// none): a few hundred small json files, once per settings window.
const MAX_READ: usize = 400;

/// The rows a suggestion can sit on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Slot {
    Noise,
    Gain,
    Edges,
    Rumble,
}

/// What each row's lightbulb says; empty: no suggestion there.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Tips {
    pub noise: String,
    pub gain: String,
    pub edges: String,
    pub rumble: String,
}

/// The [`Levels`] of the newest entries in the history folder `dir`, newest first, at most
/// [`RECENT`]. Folders and files that are not the app's are skipped; a missing folder is none.
pub(super) fn recent_levels(dir: &Path) -> Vec<Levels> {
    let mut found = Vec::new();
    super::super::recent::newest(dir, MAX_READ, |day, stem| {
        let levels = fs::read_to_string(day.join(format!("{stem}.json")))
            .ok()
            .and_then(|json| Meta::from_json(&json).ok())
            .and_then(|meta| meta.levels);
        found.extend(levels);
        found.len() < RECENT
    });
    found
}

fn noise_name(n: Noise) -> &'static str {
    match n {
        Noise::Off => "Off",
        Noise::Light => "Light",
        Noise::Medium => "Medium",
        Noise::Strong => "Strong",
        Noise::Auto => "Auto",
    }
}

fn signed(db: f64) -> String {
    if db > 0.0 {
        format!("+{db:.0} dB")
    } else {
        format!("{db:.0} dB")
    }
}

/// The key "Not now" keeps for the suggestion in `slot` of `advice` (`recent::DISMISSED_FILE`):
/// it names the value, so another value suggested later is shown again. `None`: none there.
pub(super) fn key(slot: Slot, advice: &AudioAdvice) -> Option<String> {
    let (kind, value) = match slot {
        Slot::Noise => ("audio-noise", noise_name(advice.noise?.value).to_owned()),
        Slot::Gain => ("audio-gain", format!("{:.0}", advice.gain_db?.value)),
        Slot::Edges => ("audio-edges", format!("{:?}", advice.edges?.value)),
        Slot::Rumble => ("audio-rumble", format!("{}", advice.rumble?.value)),
    };
    Some(dismiss_key(kind, &value))
}

/// `advice` without the suggestions whose key is in `dismissed`.
pub(super) fn without(advice: &AudioAdvice, dismissed: &HashSet<String>) -> AudioAdvice {
    let gone = |slot| key(slot, advice).is_some_and(|k| dismissed.contains(&k));
    let (noise, gain, edges, rumble) = (
        gone(Slot::Noise),
        gone(Slot::Gain),
        gone(Slot::Edges),
        gone(Slot::Rumble),
    );
    AudioAdvice {
        noise: advice.noise.filter(|_| !noise),
        gain_db: advice.gain_db.filter(|_| !gain),
        edges: advice.edges.filter(|_| !edges),
        rumble: advice.rumble.filter(|_| !rumble),
    }
}

/// The words of the suggestions in `advice`, the last mic test's offer (`mic`: what it heard
/// and the boost to set) first on the boost row. `gain`: the boost now (a lower suggested boost
/// is "too loud", whatever its sign).
pub(super) fn tips(advice: &AudioAdvice, mic: Option<(MicLevel, f64)>, gain: f64) -> Tips {
    let recordings = |hits: usize, of: usize| format!("{hits} of your last {of} recordings");
    let noise = advice.noise.map(|a| {
        let room = match a.value {
            Noise::Off => "were made in a quiet room",
            Noise::Light => "have a little background noise",
            _ => "have background noise",
        };
        format!(
            "{} {room}. Set this to {}?",
            recordings(a.hits, a.of),
            noise_name(a.value)
        )
    });
    let gain = match mic {
        Some((MicLevel::Quiet, to)) => Some(format!(
            "The test sounded quiet. Raise the boost to {}?",
            signed(to)
        )),
        Some((MicLevel::Loud, to)) => Some(format!(
            "The test was too loud. Lower the boost to {}?",
            signed(to)
        )),
        Some((MicLevel::Good, _)) | None => advice.gain_db.map(|a| {
            let (what, verb) = if a.value < gain {
                ("were too loud", "Lower")
            } else {
                ("were quiet", "Raise")
            };
            format!(
                "{} {what}. {verb} the boost to {}?",
                recordings(a.hits, a.of),
                signed(a.value)
            )
        }),
    };
    let edges = advice
        .edges
        .filter(|a| a.value == EdgePreset::Careful)
        .map(|a| {
            format!(
                "In {} your speech reached the very start or end. Switch to Careful?",
                recordings(a.hits, a.of)
            )
        });
    let rumble = advice.rumble.map(|a| {
        format!(
            "{} have a deep hum or thumps. Turn this on?",
            recordings(a.hits, a.of)
        )
    });
    Tips {
        noise: noise.unwrap_or_default(),
        gain: gain.unwrap_or_default(),
        edges: edges.unwrap_or_default(),
        rumble: rumble.unwrap_or_default(),
    }
}

/// What the mic test heard, the tag in the meter (it stays until the next test).
pub(super) fn verdict(level: MicLevel) -> &'static str {
    match level {
        MicLevel::Quiet => "Quiet",
        MicLevel::Good => "Good level",
        MicLevel::Loud => "Too loud",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxi_core::suggest::Advice;

    /// A temp history folder, removed when the test ends (AGENTS rule 10).
    struct Folder(std::path::PathBuf);

    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(dir: &Path, day: &str, stem: &str, levels: Option<Levels>) {
        let meta = Meta {
            levels,
            ..Meta::basic(
                &format!("{day}/{stem}"),
                1000,
                oxi_core::history::Status::Done,
                "turbo",
                "ru",
            )
        };
        fs::create_dir_all(dir.join(day)).unwrap();
        fs::write(
            dir.join(day).join(format!("{stem}.json")),
            meta.to_json().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn reads_the_newest_numbers_first_and_skips_what_is_not_an_entry() {
        let dir = std::env::temp_dir().join(format!("oxi-suggest-{}", std::process::id()));
        let _guard = Folder(dir.clone());
        let l = |peak_db| Levels {
            peak_db,
            ..Levels::default()
        };
        entry(&dir, "2026-11-12", "090000-000", Some(l(-3)));
        entry(&dir, "2026-11-13", "080000-000", Some(l(-2)));
        entry(&dir, "2026-11-13", "100000-000", Some(l(-1)));
        entry(&dir, "2026-11-13", "110000-000", None);
        fs::write(dir.join("2026-11-13").join("notes.json"), "{}").unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        let peaks: Vec<i16> = recent_levels(&dir).iter().map(|l| l.peak_db).collect();
        assert_eq!(peaks, [-1, -2, -3]);
        assert_eq!(recent_levels(&dir.join("missing")), Vec::<Levels>::new());
    }

    #[test]
    fn tips_say_counts_and_the_value_and_go_when_dismissed() {
        let advice = AudioAdvice {
            noise: Some(Advice {
                value: Noise::Medium,
                hits: 14,
                of: 20,
            }),
            gain_db: Some(Advice {
                value: 12.0,
                hits: 18,
                of: 20,
            }),
            edges: Some(Advice {
                value: EdgePreset::Careful,
                hits: 7,
                of: 20,
            }),
            rumble: None,
        };
        let t = tips(&advice, None, 0.0);
        assert_eq!(
            t.noise,
            "14 of your last 20 recordings have background noise. Set this to Medium?"
        );
        assert_eq!(
            t.gain,
            "18 of your last 20 recordings were quiet. Raise the boost to +12 dB?"
        );
        assert!(t.edges.ends_with("Switch to Careful?"), "{}", t.edges);
        assert_eq!(t.rumble, "");
        let noise_key = key(Slot::Noise, &advice).unwrap();
        let left = without(&advice, &[noise_key.clone()].into());
        let mic = tips(&left, Some((MicLevel::Loud, -6.0)), 0.0);
        assert_eq!(mic.gain, "The test was too loud. Lower the boost to -6 dB?");
        assert_eq!(mic.noise, "", "Not now hides it");
        assert!(
            left.gain_db.is_some() && left.edges.is_some(),
            "only that one"
        );
        let mut light = advice.clone();
        light.noise = Some(Advice {
            value: Noise::Light,
            hits: 14,
            of: 20,
        });
        assert!(
            without(&light, &[noise_key].into()).noise.is_some(),
            "another value is a new suggestion"
        );
        assert_eq!(key(Slot::Rumble, &advice), None);
        assert_eq!(tips(&AudioAdvice::default(), None, 0.0), Tips::default());
    }
}
