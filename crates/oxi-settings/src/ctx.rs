//! What every page binds against: the file layout, the loaded config and the daemon link.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
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
}

impl Ctx {
    /// Load `config.toml`; a missing file gives the defaults. The second value is what the banner
    /// shows: `ReadOnly` for a file from a newer build, a parse error for a broken one. A broken
    /// file loads as locked defaults, so nothing but Reset overwrites the user's hand edit.
    pub fn load(layout: Layout) -> io::Result<(Self, Option<ConfigError>)> {
        let (file, problem) = match fs::read_to_string(layout.config_file()) {
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
        };
        let ctx = Self {
            layout,
            config: Rc::new(RefCell::new(file)),
            ipc: Ipc::default(),
        };
        Ok((ctx, problem))
    }

    /// Write the config atomically. Fails with `ReadOnly` while the file is locked.
    pub fn save(&self) -> Result<(), Error> {
        let text = self.config.borrow().to_toml_string()?;
        oxi_platform::fs::write_atomic(&self.layout.config_file(), text.as_bytes())?;
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

/// Defaults that refuse to be written, shown under a parse-error banner.
fn locked_defaults() -> ConfigFile {
    let mut file = ConfigFile::default();
    file.read_only = true;
    file
}
