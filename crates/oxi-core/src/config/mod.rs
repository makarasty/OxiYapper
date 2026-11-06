//! Config: load with migrations, unknown keys kept, newer version read-only
//! (ARCHITECTURE section 6). The atomic file write is I/O and lives in the settings process.

mod schema;

pub use schema::{
    Audio, Config, Debug, History, Hotkey, HotkeyMode, Model, Overlay, Paste, PasteMode, PasteRule,
    Sounds, System,
};

use toml::{Table, Value};

/// Schema version this build writes. Equals `MIGRATIONS.len() + 1` (checked by a test).
pub const CURRENT_VERSION: i64 = 1;

/// A pure step `vN -> vN+1` on the raw table. It does not touch `version`; `parse_with` does.
pub type Migration = fn(&mut Table);

/// `MIGRATIONS[i]` turns version `i + 1` into `i + 2`. Empty while v1 is the only schema.
pub const MIGRATIONS: &[Migration] = &[];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config.toml line {line}, column {column}: {message}")]
    Syntax {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("config.toml: {0}")]
    Invalid(String),
    #[error("config.toml is from a newer version and is read-only")]
    ReadOnly,
}

/// A loaded `config.toml`: typed settings plus the raw table, so keys this build does not
/// know survive a write.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigFile {
    pub config: Config,
    /// True when the file's `version` is newer than this build: never write it back.
    pub read_only: bool,
    raw: Table,
}

impl Default for ConfigFile {
    fn default() -> Self {
        let mut raw = Table::new();
        raw.insert("version".into(), Value::Integer(CURRENT_VERSION));
        Self {
            config: Config::default(),
            read_only: false,
            raw,
        }
    }
}

impl ConfigFile {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Self::parse_with(text, MIGRATIONS)
    }

    /// `parse` with an explicit migration list (tests ship their own).
    pub fn parse_with(text: &str, migrations: &[Migration]) -> Result<Self, ConfigError> {
        let mut raw: Table = text.parse().map_err(|e| syntax_error(text, &e))?;
        let current = i64::try_from(migrations.len() + 1).unwrap_or(i64::MAX);
        let version = match raw.get("version") {
            None => 1,
            Some(Value::Integer(v)) if *v >= 1 => *v,
            Some(_) => {
                return Err(ConfigError::Invalid(
                    "`version` must be an integer >= 1".into(),
                ));
            }
        };
        let read_only = version > current;
        if !read_only {
            // `version >= 1`, so the index is non-negative and at most `migrations.len()`.
            let done = usize::try_from(version - 1).unwrap_or(0);
            for migration in &migrations[done..] {
                migration(&mut raw);
            }
            raw.insert("version".into(), Value::Integer(current));
        }
        let config = raw.clone().try_into().map_err(|e: toml::de::Error| {
            // A Table carries no spans; re-read the text for the line when it fails as is.
            toml::from_str::<Config>(text).map_or_else(
                |located| syntax_error(text, &located),
                |_| ConfigError::Invalid(e.message().to_owned()),
            )
        })?;
        Ok(Self {
            config,
            read_only,
            raw,
        })
    }

    /// Typed settings laid over the raw table, so unknown keys are written back.
    /// Errors with `ReadOnly` for a file from a newer version.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        if self.read_only {
            return Err(ConfigError::ReadOnly);
        }
        let typed =
            Table::try_from(&self.config).map_err(|e| ConfigError::Invalid(e.to_string()))?;
        let mut out = self.raw.clone();
        overlay(&mut out, typed);
        toml::to_string(&out).map_err(|e| ConfigError::Invalid(e.to_string()))
    }
}

/// Recursive merge: tables merge key by key, anything else is replaced.
fn overlay(base: &mut Table, top: Table) {
    for (key, value) in top {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(b)), Value::Table(t)) => overlay(b, t),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn syntax_error(text: &str, e: &toml::de::Error) -> ConfigError {
    let at = e.span().map_or(0, |s| s.start).min(text.len());
    let before = text.get(..at).unwrap_or_default();
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    ConfigError::Syntax {
        line,
        column: at - line_start + 1,
        message: e.message().to_owned(),
    }
}
