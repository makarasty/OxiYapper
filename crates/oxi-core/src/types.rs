//! Shared primitives used by several crates; defined once, here.

use crate::languages::{self, LanguageInfo};
use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};
use std::str::FromStr;

/// A duration or timestamp in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Millis(pub u64);

impl Millis {
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs.saturating_mul(1000))
    }
}

impl Add for Millis {
    type Output = Self;
    /// Saturates at `u64::MAX`: a deadline far in the future never wraps into the past.
    fn add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }
}

impl Sub for Millis {
    type Output = Self;
    /// Saturates at zero: a duration is never negative.
    fn sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

/// Dictation language: `auto` or one code of the whisper table. One pointer, so it is `Copy`;
/// the field is private, so only `auto` and table codes exist. [`Language::as_str`] is its one
/// spelling: config, CLI and whisper.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Language(&'static str);

impl Language {
    pub const AUTO: Self = Self("auto");
    pub const RU: Self = Self("ru");
    pub const EN: Self = Self("en");

    /// The config and CLI value, also whisper.cpp's language code.
    pub const fn as_str(self) -> &'static str {
        self.0
    }

    /// The table row; `None` for auto.
    pub fn info(self) -> Option<&'static LanguageInfo> {
        languages::find(self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown language code {0:?}: expected auto or a whisper code such as ru, en, zh")]
pub struct UnknownLanguage(pub String);

impl FromStr for Language {
    type Err = UnknownLanguage;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == Self::AUTO.0 {
            return Ok(Self::AUTO);
        }
        languages::find(s)
            .map(|i| Self(i.code))
            .ok_or_else(|| UnknownLanguage(s.to_owned()))
    }
}

// By hand: a derive would borrow the `&'static str` field from the input and demand `'de: 'static`.
impl Serialize for Language {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.0)
    }
}

impl<'de> Deserialize<'de> for Language {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn millis_arithmetic_and_order() {
        assert_eq!(Millis::from_secs(2), Millis(2000));
        assert_eq!(Millis(5) + Millis(7), Millis(12));
        assert_eq!(Millis(7) - Millis(5), Millis(2));
        assert_eq!(Millis(5) - Millis(7), Millis(0));
        assert!(Millis(1) < Millis(2));
    }

    #[test]
    fn millis_add_and_from_secs_saturate() {
        assert_eq!(Millis(u64::MAX - 1) + Millis(5), Millis(u64::MAX));
        assert_eq!(Millis::from_secs(u64::MAX), Millis(u64::MAX));
    }

    #[test]
    fn language_serde_lowercase() {
        for (lang, name) in [
            (Language::RU, "ru"),
            (Language::EN, "en"),
            (Language::AUTO, "auto"),
            (Language("zh"), "zh"),
            (Language("haw"), "haw"),
        ] {
            let json = serde_json::to_string(&lang).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<Language>(&json).unwrap(), lang);
        }
        assert!(serde_json::from_str::<Language>("\"Ru\"").is_err());
    }

    #[test]
    fn language_parses_auto_and_every_table_code_only() {
        assert_eq!("auto".parse(), Ok(Language::AUTO));
        assert_eq!(Language::AUTO.info(), None);
        assert_eq!("ru".parse(), Ok(Language::RU));
        assert_eq!("en".parse(), Ok(Language::EN));
        for i in languages::LANGUAGES {
            let lang: Language = i.code.parse().unwrap();
            assert_eq!(lang.as_str(), i.code);
            assert_eq!(lang.info(), Some(&i));
        }
        for bad in ["", "xx", "RU", " ru", "zho", "Auto"] {
            assert_eq!(bad.parse::<Language>(), Err(UnknownLanguage(bad.into())));
        }
    }
}
