//! Paths: config, data, models and log locations (ARCHITECTURE section 7).
//! Pure functions of base directories the caller passes in; the caller asks the OS
//! (`directories`) and checks for the `portable` marker file.

use std::path::{Path, PathBuf};

pub const APP_DIR: &str = "oxi-yapper";
/// A file with this name next to the exe turns on portable mode.
pub const PORTABLE_MARKER: &str = "portable";

pub const CONFIG_FILE: &str = "config.toml";
pub const DICTIONARY_FILE: &str = "dictionary.txt";
pub const REPLACE_FILE: &str = "replace.txt";
pub const STATE_FILE: &str = "state.toml";
pub const MODELS_DIR: &str = "models";
pub const HISTORY_DIR: &str = "history";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    MacOs,
    Linux,
}

impl Os {
    /// The OS this build runs on (other unixes use the Linux layout).
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

/// Per-OS base directories, as `directories` reports them.
///
/// - Windows: `config` = `%APPDATA%`, `data` = `%LOCALAPPDATA%`, `logs` unused.
/// - macOS: `config` = `data` = `~/Library/Application Support`, `logs` = `~/Library/Logs`.
/// - Linux: `config` = `$XDG_CONFIG_HOME`, `data` = `$XDG_DATA_HOME`, `logs` = `$XDG_STATE_HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseDirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub logs: PathBuf,
}

/// Where each kind of file lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `config.toml`, `dictionary.txt`, `replace.txt`.
    pub config_dir: PathBuf,
    /// `state.toml`, `models/`, `history/`.
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
}

impl Layout {
    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join(CONFIG_FILE)
    }
    pub fn dictionary_file(&self) -> PathBuf {
        self.config_dir.join(DICTIONARY_FILE)
    }
    pub fn replace_file(&self) -> PathBuf {
        self.config_dir.join(REPLACE_FILE)
    }
    pub fn state_file(&self) -> PathBuf {
        self.data_dir.join(STATE_FILE)
    }
    pub fn models_dir(&self) -> PathBuf {
        self.data_dir.join(MODELS_DIR)
    }
    pub fn history_dir(&self) -> PathBuf {
        self.data_dir.join(HISTORY_DIR)
    }
}

/// `bundle_id` names the macOS directory only (fixed in ROADMAP M12); other OSes ignore it.
pub fn layout(os: Os, base: &BaseDirs, bundle_id: &str) -> Layout {
    match os {
        Os::Windows => {
            let data_dir = base.data.join(APP_DIR).join("data");
            Layout {
                config_dir: base.config.join(APP_DIR).join("config"),
                logs_dir: data_dir.join("logs"),
                data_dir,
            }
        }
        Os::MacOs => Layout {
            config_dir: base.config.join(bundle_id),
            data_dir: base.data.join(bundle_id),
            logs_dir: base.logs.join(APP_DIR),
        },
        Os::Linux => Layout {
            config_dir: base.config.join(APP_DIR),
            data_dir: base.data.join(APP_DIR),
            logs_dir: base.logs.join(APP_DIR).join("logs"),
        },
    }
}

/// Portable mode: everything under `<exe dir>/data/`.
pub fn portable_layout(exe_dir: &Path) -> Layout {
    let dir = exe_dir.join("data");
    Layout {
        logs_dir: dir.join("logs"),
        config_dir: dir.clone(),
        data_dir: dir,
    }
}
