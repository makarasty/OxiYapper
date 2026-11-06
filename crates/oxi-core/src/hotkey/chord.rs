use super::key::{Key, Modifier};

/// Why a list of key names is not a usable hotkey.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChordError {
    #[error("{0:?} is not a hotkey key name (modifiers are Ctrl, Shift, Alt, Win)")]
    Unknown(String),
    #[error("the hotkey has no keys")]
    Empty,
    #[error("Esc cancels a recording and cannot be part of the hotkey")]
    Esc,
    #[error("{0:?} is in the hotkey twice")]
    Duplicate(String),
}

/// One element of a chord: a modifier on either side, or one exact key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Part {
    Mod(Modifier),
    Key(Key),
}

impl Part {
    fn of(key: Key) -> Self {
        key.modifier().map_or(Part::Key(key), Part::Mod)
    }

    fn name(self) -> &'static str {
        match self {
            Part::Mod(m) => m.as_str(),
            Part::Key(k) => k.as_str(),
        }
    }

    fn named(name: &str) -> Result<Self, ChordError> {
        if let Some(m) = Modifier::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(name))
        {
            return Ok(Part::Mod(m));
        }
        match Key::from_name(name) {
            Some(Key::Esc) => Err(ChordError::Esc),
            // A sided name ("LCtrl") would quietly mean either side: config spells modifiers once.
            Some(k) if k.modifier().is_none() => Ok(Part::Key(k)),
            _ => Err(ChordError::Unknown(name.to_owned())),
        }
    }
}

/// The hotkey: keys that must all be held. Modifiers match either side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    /// Sorted (modifiers first), no duplicates, never empty, never Esc.
    parts: Vec<Part>,
}

impl Chord {
    /// Builds the chord from config names (`hotkey.chord`, `paste.rules[].chord`).
    pub fn parse(names: &[String]) -> Result<Self, ChordError> {
        let mut parts = Vec::with_capacity(names.len());
        for name in names {
            let part = Part::named(name)?;
            if parts.contains(&part) {
                return Err(ChordError::Duplicate(name.clone()));
            }
            parts.push(part);
        }
        Self::from_parts(parts)
    }

    /// Builds the chord from keys held in the picker; both sides of a modifier are one key.
    pub fn from_keys(keys: &[Key]) -> Result<Self, ChordError> {
        if keys.contains(&Key::Esc) {
            return Err(ChordError::Esc);
        }
        let mut parts: Vec<Part> = keys.iter().copied().map(Part::of).collect();
        parts.sort_unstable();
        parts.dedup();
        Self::from_parts(parts)
    }

    fn from_parts(mut parts: Vec<Part>) -> Result<Self, ChordError> {
        if parts.is_empty() {
            return Err(ChordError::Empty);
        }
        parts.sort_unstable();
        Ok(Self { parts })
    }

    /// Config names, modifiers first.
    pub fn to_names(&self) -> Vec<String> {
        self.parts.iter().map(|p| p.name().to_owned()).collect()
    }

    /// Keys to press in this order (and release in reverse) to send the chord: left modifiers.
    pub fn keys_to_send(&self) -> Vec<Key> {
        self.parts
            .iter()
            .map(|p| match *p {
                Part::Mod(m) => m.sides()[0],
                Part::Key(k) => k,
            })
            .collect()
    }

    /// Whether the whole chord is held, given the physical state of each key.
    pub fn is_held(&self, mut held: impl FnMut(Key) -> bool) -> bool {
        self.parts.iter().all(|p| match *p {
            Part::Mod(m) => m.sides().into_iter().any(&mut held),
            Part::Key(k) => held(k),
        })
    }

    /// Whether `key` is one of the chord's keys (either side for a modifier).
    pub(super) fn contains(&self, key: Key) -> bool {
        self.parts.contains(&Part::of(key))
    }
}
