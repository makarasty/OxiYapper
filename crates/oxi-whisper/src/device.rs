//! GPU device list from ggml's backend registry, and the choice among them.

use std::ffi::CStr;

use whisper_rs::whisper_rs_sys as sys;

/// One GPU that whisper.cpp can run on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// ggml device name, e.g. `Vulkan0`. Stable key for the config.
    pub name: String,
    /// Marketing name, e.g. `NVIDIA GeForce RTX 4070 SUPER`.
    pub description: String,
    /// Integrated GPU sharing system memory.
    pub uma: bool,
}

/// What the user picked in the config (`gpu = "auto" | "cpu" | "<device name>"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceChoice {
    Auto,
    Cpu,
    Named(String),
}

/// Result of resolving a [`DeviceChoice`] against the device list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picked {
    /// Index in the device list, `None` = CPU.
    pub index: Option<usize>,
    /// Set when a named device was not found and `Auto` was used instead.
    pub warning: Option<String>,
}

/// GPU and iGPU devices in ggml's order, with the UMA flag. This is the order
/// whisper.cpp's `gpu_device` counts over (it skips CPU and accelerator devices).
fn gpus() -> impl Iterator<Item = (sys::ggml_backend_dev_t, bool)> {
    // SAFETY: ggml returns a valid device pointer for every index below the count.
    (0..unsafe { sys::ggml_backend_dev_count() })
        .map(|i| unsafe { sys::ggml_backend_dev_get(i) })
        .filter_map(|dev| {
            // SAFETY: `dev` is a valid registry device.
            let kind = unsafe { sys::ggml_backend_dev_type(dev) };
            let uma = kind == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_IGPU;
            let gpu = kind == sys::ggml_backend_dev_type_GGML_BACKEND_DEVICE_TYPE_GPU;
            (gpu || uma).then_some((dev, uma))
        })
}

/// GPUs in ggml's order; the index into this list is whisper.cpp's `gpu_device`.
pub fn list_devices() -> Vec<Device> {
    gpus()
        // SAFETY: name/description point to NUL-terminated strings owned by the registry.
        .map(|(dev, uma)| unsafe {
            Device {
                name: cstr(sys::ggml_backend_dev_name(dev)),
                description: cstr(sys::ggml_backend_dev_description(dev)),
                uma,
            }
        })
        .collect()
}

/// Whether ggml can start a backend on GPU `index` of [`list_devices`].
///
/// whisper.cpp makes the same `ggml_backend_dev_init` call when it creates a state,
/// and on null it logs and runs on CPU without an error; the state does not say
/// which backend it got. So the load asks first. A device that fails between this
/// probe and whisper's own call still slips through to CPU unreported.
pub(crate) fn gpu_starts(index: usize) -> bool {
    let Some((dev, _)) = gpus().nth(index) else {
        return false;
    };
    // SAFETY: `dev` is a valid registry device and null params is ggml's default;
    // a non-null backend is ours and freed once.
    unsafe {
        let backend = sys::ggml_backend_dev_init(dev, std::ptr::null());
        if backend.is_null() {
            return false;
        }
        sys::ggml_backend_free(backend);
    }
    true
}

/// # Safety
/// `p` must be null or point to a NUL-terminated string.
unsafe fn cstr(p: *const std::os::raw::c_char) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: not null, NUL-terminated by the caller's contract.
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}

/// Auto: first discrete GPU, then first integrated one, then CPU.
fn pick_auto(devices: &[Device]) -> Option<usize> {
    devices
        .iter()
        .position(|d| !d.uma)
        .or_else(|| devices.iter().position(|d| d.uma))
}

/// Pure choice logic; `devices` is the list from [`list_devices`].
pub fn pick(devices: &[Device], choice: &DeviceChoice) -> Picked {
    match choice {
        DeviceChoice::Auto => Picked {
            index: pick_auto(devices),
            warning: None,
        },
        DeviceChoice::Cpu => Picked {
            index: None,
            warning: None,
        },
        DeviceChoice::Named(name) => match devices.iter().position(|d| &d.name == name) {
            Some(i) => Picked {
                index: Some(i),
                warning: None,
            },
            None => Picked {
                index: pick_auto(devices),
                warning: Some(format!("GPU {name:?} not found, using auto")),
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
            assert_eq!(got.warning.is_some(), warns, "{devices:?} {choice:?}");
        }
    }

    #[test]
    fn list_devices_does_not_crash() {
        // CPU-only builds return an empty list; GPU builds list real hardware.
        let _ = list_devices();
        assert!(!gpu_starts(usize::MAX));
    }

    #[cfg(any(feature = "vulkan", feature = "cuda", feature = "metal"))]
    #[test]
    fn gpu_build_lists_a_gpu_that_starts() {
        let devices = list_devices();
        eprintln!("devices: {devices:?}");
        assert!(!devices.is_empty(), "GPU build sees no GPU");
        assert!(devices.iter().all(|d| !d.name.is_empty()));
        assert!(gpu_starts(0));
    }
}
