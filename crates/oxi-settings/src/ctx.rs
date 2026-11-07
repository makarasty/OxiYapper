//! What every page binds against: the file layout, the loaded config and the daemon link.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::SystemTime;
use std::{fs, io};

use oxi_core::config::{ConfigError, ConfigFile};
use oxi_core::paths::{CONFIG_FILE, Layout};

use crate::Error;
use crate::ipc::Ipc;

/// Cheap to clone: clones share one config.
#[derive(Debug, Clone)]
pub struct Ctx {
    pub layout: Layout,
    pub config: Rc<RefCell<ConfigFile>>,
    pub ipc: Ipc,
    /// The file as last read or written by this window; another stamp on disk is an edit from
    /// outside.
    stamp: Rc<Cell<Option<Stamp>>>,
}

/// Modification time and length of `config.toml`.
type Stamp = (SystemTime, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

impl Ctx {
    /// Load `config.toml`; a missing file gives the defaults. The second value is what the banner
    /// shows: `ReadOnly` for a file from a newer build, a parse error for a broken one. A broken
    /// file loads as locked defaults, so nothing but Reset overwrites the user's hand edit.
    pub fn load(layout: Layout) -> io::Result<(Self, Option<ConfigError>)> {
        let stamp = stamp(&layout.config_file());
        let (file, problem) = read(&layout.config_file())?;
        let ctx = Self {
            layout,
            config: Rc::new(RefCell::new(file)),
            ipc: Ipc::default(),
            stamp: Rc::new(Cell::new(stamp)),
        };
        Ok((ctx, problem))
    }

    /// Load the file again if it was changed outside this window since it was last read or
    /// written; `Some` carries the new banner problem, as [`Ctx::load`]. A file that is gone or
    /// unreadable right now (an editor's rename-over) is left for the next call.
    pub fn reload_if_changed(&self) -> Option<Option<ConfigError>> {
        let path = self.layout.config_file();
        let now = stamp(&path)?;
        if self.stamp.get() == Some(now) {
            return None;
        }
        let (file, problem) = read(&path).ok()?;
        // Changed or gone while it was read: a half-written file must not replace the config.
        if stamp(&path) != Some(now) {
            return None;
        }
        self.stamp.set(Some(now));
        *self.config.borrow_mut() = file;
        Some(problem)
    }

    /// Write the config atomically. Fails with `ReadOnly` while the file is locked.
    pub fn save(&self) -> Result<(), Error> {
        let text = self.config.borrow().to_toml_string()?;
        let path = self.layout.config_file();
        oxi_platform::fs::write_atomic(&path, text.as_bytes())?;
        self.stamp.set(stamp(&path));
        Ok(())
    }

    /// Copy the current file to `config.toml.bak`, then write the defaults over it.
    pub fn reset(&self) -> Result<(), Error> {
        match fs::copy(self.layout.config_file(), self.backup_file()) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        *self.config.borrow_mut() = ConfigFile::default();
        self.save()
    }

    pub fn backup_file(&self) -> PathBuf {
        self.layout.config_dir.join(format!("{CONFIG_FILE}.bak"))
    }
}

/// The file and its banner problem, as [`Ctx::load`] describes.
fn read(path: &Path) -> io::Result<(ConfigFile, Option<ConfigError>)> {
    Ok(match fs::read_to_string(path) {
        Ok(text) => match ConfigFile::parse(&text) {
            Ok(file) => {
                let problem = file.read_only.then_some(ConfigError::ReadOnly);
                (file, problem)
            }
            Err(e) => (locked_defaults(), Some(e)),
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => (ConfigFile::default(), None),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => (
            locked_defaults(),
            Some(ConfigError::Invalid("the file is not UTF-8".into())),
        ),
        Err(e) => return Err(e),
    })
}

/// Defaults that refuse to be written, shown under a parse-error banner.
fn locked_defaults() -> ConfigFile {
    let mut file = ConfigFile::default();
    file.read_only = true;
    file
}
