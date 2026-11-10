//! Paths: config, data, models and log locations (ARCHITECTURE section 7).
//! Pure functions of base directories the caller passes in; the caller asks the OS
//! (`directories`) and checks for the `portable` marker file.

use std::path::{Path, PathBuf};

use crate::APP_NAME;

/// A file with this name next to the exe turns on portable mode.
pub const PORTABLE_MARKER: &str = "portable";

pub const CONFIG_FILE: &str = "config.toml";
pub const DICTIONARY_FILE: &str = "dictionary.txt";
pub const REPLACE_FILE: &str = "replace.txt";
pub const STATE_FILE: &str = "state.toml";
pub const MODELS_DIR: &str = "models";
pub const HISTORY_DIR: &str = "history";
/// Suffix of a file still being written; the rename that drops it makes the file whole.
pub const PART_SUFFIX: &str = ".part";

/// `<path>.part`, where a download or recording is written before its rename.
pub fn part_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(PART_SUFFIX);
    PathBuf::from(p)
}

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

/// Per-OS base directories, filled by the caller.
///
/// `directories` reports `config`, `data` and the Linux state dir only; the caller builds the
/// rest itself: `~/Library/Logs` on macOS, and on Windows (no state dir) `logs` is not read.
///
/// - Windows: `config` = `%APPDATA%`, `data` = `%LOCALAPPDATA%`, `logs` unused (logs go under `data`).
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
            let data_dir = base.data.join(APP_NAME).join("data");
            Layout {
                config_dir: base.config.join(APP_NAME).join("config"),
                logs_dir: data_dir.join("logs"),
                data_dir,
            }
        }
        Os::MacOs => Layout {
            config_dir: base.config.join(bundle_id),
            data_dir: base.data.join(bundle_id),
            logs_dir: base.logs.join(APP_NAME),
        },
        Os::Linux => Layout {
            config_dir: base.config.join(APP_NAME),
            data_dir: base.data.join(APP_NAME),
            logs_dir: base.logs.join(APP_NAME).join("logs"),
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

/// `path` shortened to at most `max_chars` characters for a field (DS-13.1): the root and the
/// file or folder name stay whole with as many trailing folders as fit, and "…" stands for the
/// rest: `C:\…\models\ggml-large-v3.bin`. Only a name too long on its own is cut, at its end.
#[must_use]
pub fn elide_path(path: &str, max_chars: usize) -> String {
    let len = |s: &str| s.chars().count();
    if len(path) <= max_chars {
        return path.to_owned();
    }
    let is_sep = |c: char| c == '\\' || c == '/';
    let sep = path.chars().rev().find(|&c| is_sep(c)).unwrap_or('\\');
    let (root, rest) = path.split_at(root_len(path));
    let parts: Vec<&str> = rest.split(is_sep).filter(|p| !p.is_empty()).collect();
    let Some((name, folders)) = parts.split_last() else {
        return cut(path, max_chars);
    };
    // The root goes first when room runs out, then the name is cut.
    for head in [format!("{root}…{sep}"), format!("…{sep}")] {
        if len(&head) + len(name) > max_chars {
            continue;
        }
        let mut tail = (*name).to_owned();
        for folder in folders.iter().rev() {
            if len(&head) + len(folder) + 1 + len(&tail) > max_chars {
                break;
            }
            tail = format!("{folder}{sep}{tail}");
        }
        return head + &tail;
    }
    cut(name, max_chars)
}

/// Bytes of the root: `\\server\share\`, `C:\`, `C:` or `/`; 0 for a relative path.
fn root_len(path: &str) -> usize {
    let b = path.as_bytes();
    let sep = |i: usize| b.get(i).is_some_and(|&c| c == b'\\' || c == b'/');
    if sep(0) && sep(1) {
        let mut seen = 0;
        for i in 2..b.len() {
            if sep(i) {
                seen += 1;
                if seen == 2 {
                    return i + 1;
                }
            }
        }
        return b.len();
    }
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return if sep(2) { 3 } else { 2 };
    }
    usize::from(sep(0))
}

/// The first `max_chars - 1` characters and "…".
fn cut(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    s.chars().take(max_chars - 1).chain(['…']).collect()
}

#[cfg(test)]
mod tests {
    use super::elide_path;

    const MODEL: &str = r"C:\Users\someone\AppData\Local\OxiYapper\models\ggml-large-v3.bin";

    #[test]
    fn short_path_stays_whole() {
        assert_eq!(elide_path(r"C:\models\a.bin", 40), r"C:\models\a.bin");
        assert_eq!(elide_path(MODEL, MODEL.chars().count()), MODEL);
    }

    #[test]
    fn long_path_keeps_root_name_and_trailing_folders() {
        assert_eq!(elide_path(MODEL, 30), r"C:\…\models\ggml-large-v3.bin");
        assert_eq!(
            elide_path(MODEL, 39),
            r"C:\…\OxiYapper\models\ggml-large-v3.bin"
        );
        assert_eq!(elide_path(MODEL, 23), r"C:\…\ggml-large-v3.bin");
        for n in 23..80 {
            let got = elide_path(MODEL, n);
            assert!(got.chars().count() <= n, "{n}: {got}");
            assert!(got.ends_with("ggml-large-v3.bin"), "{n}: {got}");
        }
    }

    #[test]
    fn long_name_drops_the_root_then_is_cut_at_its_end() {
        assert_eq!(elide_path(MODEL, 20), r"…\ggml-large-v3.bin");
        assert_eq!(elide_path(MODEL, 10), "ggml-larg…");
        assert_eq!(elide_path(MODEL, 1), "…");
        assert_eq!(elide_path(MODEL, 0), "");
    }

    #[test]
    fn unc_and_slash_paths_keep_their_root_and_separator() {
        let unc = r"\\server\share\team\audio\sounds\start.wav";
        assert_eq!(elide_path(unc, 33), r"\\server\share\…\sounds\start.wav");
        assert_eq!(elide_path(unc, 32), r"\\server\share\…\start.wav");
        assert_eq!(
            elide_path("/home/someone/.local/share/models/x.bin", 20),
            "/…/models/x.bin"
        );
    }

    #[test]
    fn root_alone_is_cut_at_its_end() {
        assert_eq!(elide_path(r"C:\", 3), r"C:\");
        assert_eq!(elide_path(r"\\server\share\", 8), r"\\serve…");
    }
}
