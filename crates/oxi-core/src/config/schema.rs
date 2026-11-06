//! Config schema v1 (ARCHITECTURE section 6). Every field has a default, so a partial
//! or empty file loads. Unknown keys are not rejected here; `ConfigFile` keeps them.

use crate::types::Language;
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HotkeyMode {
    Push,
    Toggle,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkey {
    /// Key names; "Win" is Cmd on macOS.
    pub chord: Vec<String>,
    pub mode: HotkeyMode,
}

impl Default for Hotkey {
    fn default() -> Self {
        Self {
            chord: vec!["Ctrl".into(), "Win".into()],
            mode: HotkeyMode::Push,
        }
    }
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
            max_seconds: 600,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Model {
    /// Registry id, or "custom".
    pub id: String,
    pub custom_path: String,
    /// "auto", "cpu" or a device name.
    pub gpu: String,
    pub language: Language,
    /// 0 means never unload.
    pub idle_unload_minutes: u32,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            id: "turbo".into(),
            custom_path: String::new(),
            gpu: "auto".into(),
            language: Language::Ru,
            idle_unload_minutes: 5,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PasteRule {
    pub app: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PasteMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chord: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_delay_ms: Option<u32>,
}

impl PasteRule {
    /// Every key a rule can have; a save drops the optional ones a rule no longer sets.
    pub(crate) const KEYS: [&str; 4] = ["app", "mode", "chord", "restore_delay_ms"];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paste {
    pub mode: PasteMode,
    pub restore_delay_ms: u32,
    pub trailing_space: bool,
    pub rules: Vec<PasteRule>,
}

impl Default for Paste {
    fn default() -> Self {
        Self {
            mode: PasteMode::Clipboard,
            restore_delay_ms: 300,
            trailing_space: true,
            // Remote desktop drops a synthetic Ctrl+V, so type there.
            rules: vec![PasteRule {
                app: "mstsc.exe".into(),
                mode: Some(PasteMode::Type),
                chord: None,
                restore_delay_ms: None,
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
    pub volume: f64,
}

impl Default for Sounds {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: 0.5,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub enabled: bool,
    pub max_bytes: u64,
}

impl Default for History {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: 2_147_483_648,
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

#[cfg(test)]
mod tests {
    use super::{PasteMode, PasteRule};

    #[test]
    fn rule_keys_name_every_field() {
        // A new field fails to compile here until it is set, then fails here until it is in KEYS.
        let full = PasteRule {
            app: String::new(),
            mode: Some(PasteMode::Type),
            chord: Some(Vec::new()),
            restore_delay_ms: Some(0),
        };
        let table = toml::Table::try_from(&full).unwrap();
        let keys: Vec<&str> = table.keys().map(String::as_str).collect();
        let mut want = PasteRule::KEYS.to_vec();
        want.sort_unstable();
        assert_eq!(keys, want);
    }
}
