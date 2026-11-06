//! Shared primitives used by several crates; defined once, here.

use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};

/// A duration or timestamp in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Millis(pub u64);

impl Millis {
    pub const fn from_secs(secs: u64) -> Self {
        Self(secs * 1000)
    }
}

impl Add for Millis {
    type Output = Self;
    fn add(self, rhs: Self) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub for Millis {
    type Output = Self;
    /// Saturates at zero: a duration is never negative.
    fn sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }
}

/// Dictation language.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Ru,
    En,
    #[default]
    Auto,
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
    fn language_serde_lowercase() {
        for (lang, name) in [
            (Language::Ru, "ru"),
            (Language::En, "en"),
            (Language::Auto, "auto"),
        ] {
            let json = serde_json::to_string(&lang).unwrap();
            assert_eq!(json, format!("\"{name}\""));
            assert_eq!(serde_json::from_str::<Language>(&json).unwrap(), lang);
        }
        assert!(serde_json::from_str::<Language>("\"Ru\"").is_err());
    }
}
