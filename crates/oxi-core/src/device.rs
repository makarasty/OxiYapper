//! GPU choice: the config value and the pure pick among the devices whisper.cpp lists
//! (`oxi-whisper` asks ggml for the list).

use serde::{Deserialize, Serialize};

/// One GPU that whisper.cpp can run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// ggml device name, e.g. `Vulkan0`. Stable key for the config.
    pub name: String,
    /// Marketing name, e.g. `NVIDIA GeForce RTX 4070 SUPER`.
    pub description: String,
    /// Integrated GPU sharing system memory.
    pub uma: bool,
}

/// What the user picked: `[model] gpu = "auto" | "cpu" | "<device name>"`, or `--gpu`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum DeviceChoice {
    #[default]
    Auto,
    Cpu,
    Named(String),
}

impl From<String> for DeviceChoice {
    fn from(s: String) -> Self {
        match s.as_str() {
            "auto" => Self::Auto,
            "cpu" => Self::Cpu,
            _ => Self::Named(s),
        }
    }
}

impl From<DeviceChoice> for String {
    fn from(c: DeviceChoice) -> Self {
        match c {
            DeviceChoice::Auto => "auto".into(),
            DeviceChoice::Cpu => "cpu".into(),
            DeviceChoice::Named(name) => name,
        }
    }
}

/// Result of resolving a [`DeviceChoice`] against the device list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// Index in the device list, `None` = CPU.
    pub index: Option<usize>,
    /// A named device was not found and `Auto` was used instead.
    pub missing: bool,
}

/// Auto: first discrete GPU, then first integrated one, then CPU.
fn pick_auto(devices: &[Device]) -> Option<usize> {
    devices
        .iter()
        .position(|d| !d.uma)
        .or_else(|| devices.iter().position(|d| d.uma))
}

/// Resolve `choice` against `devices`, which are in ggml's order.
pub fn pick(devices: &[Device], choice: &DeviceChoice) -> Picked {
    match choice {
        DeviceChoice::Auto => Picked {
            index: pick_auto(devices),
            missing: false,
        },
        DeviceChoice::Cpu => Picked {
            index: None,
            missing: false,
        },
        DeviceChoice::Named(name) => match devices.iter().position(|d| &d.name == name) {
            Some(i) => Picked {
                index: Some(i),
                missing: false,
            },
            None => Picked {
                index: pick_auto(devices),
                missing: true,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, uma: bool) -> Device {
        Device {
            name: name.into(),
            description: String::new(),
            uma,
        }
    }

    /// Discrete first, iGPU second, like ggml's Vulkan backend lists them.
    fn two() -> Vec<Device> {
        vec![dev("Vulkan0", false), dev("Vulkan1", true)]
    }

    fn named(n: &str) -> DeviceChoice {
        DeviceChoice::Named(n.into())
    }

    #[test]
    fn pick_table() {
        let igpu_first = vec![dev("Vulkan0", true), dev("Vulkan1", false)];
        let cases: Vec<(Vec<Device>, DeviceChoice, Option<usize>, bool)> = vec![
            (two(), DeviceChoice::Auto, Some(0), false),
            (igpu_first, DeviceChoice::Auto, Some(1), false),
            (
                vec![dev("Vulkan0", true)],
                DeviceChoice::Auto,
                Some(0),
                false,
            ),
            (vec![], DeviceChoice::Auto, None, false),
            (two(), DeviceChoice::Cpu, None, false),
            (two(), named("Vulkan1"), Some(1), false),
            (two(), named("Gone"), Some(0), true),
            (vec![], named("Gone"), None, true),
        ];
        for (devices, choice, index, warns) in cases {
            let got = pick(&devices, &choice);
            assert_eq!(got.index, index, "{devices:?} {choice:?}");
            assert_eq!(got.missing, warns, "{devices:?} {choice:?}");
        }
    }

    #[test]
    fn choice_round_trips_through_its_string() {
        for (text, choice) in [
            ("auto", DeviceChoice::Auto),
            ("cpu", DeviceChoice::Cpu),
            ("Vulkan0", named("Vulkan0")),
            ("CPU", named("CPU")),
        ] {
            assert_eq!(DeviceChoice::from(text.to_owned()), choice);
            assert_eq!(String::from(choice.clone()), text);
            let json = serde_json::to_string(&choice).unwrap();
            assert_eq!(json, format!("\"{text}\""));
            assert_eq!(serde_json::from_str::<DeviceChoice>(&json).unwrap(), choice);
        }
    }
}
