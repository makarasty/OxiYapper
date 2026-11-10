//! Config: load with migrations, unknown keys kept, newer version read-only
//! (ARCHITECTURE section 6). The atomic file write is I/O and lives in the settings process.

mod schema;

pub use schema::{
    AppLanguage, Audio, Config, Debug, History, Hotkey, Model, OtherKey, Overlay, OverlayPosition,
    Paste, PasteMode, PasteRule, Sounds, System, Theme, Ui,
};

use crate::paths::CONFIG_FILE;
use std::borrow::Cow;
use std::ops::Range;
use toml_edit::{
    Array, ArrayOfTables, Decor, DocumentMut, InlineTable, Item, Table, TableLike, Value,
};

/// Schema version this build writes. Equals `MIGRATIONS.len() + 1` (checked by a test).
pub const CURRENT_VERSION: i64 = 2;

/// A pure step `vN -> vN+1` on the raw document. It does not touch `version`; `parse_with` does.
pub type Migration = fn(&mut DocumentMut);

/// `MIGRATIONS[i]` turns version `i + 1` into `i + 2`.
pub const MIGRATIONS: &[Migration] = &[history_on];

/// v1 -> v2 (R1.3): history was off by default in v1, so no v1 file can have chosen "off"
/// on purpose; an explicit `false` becomes `true`. A later `false` (v2) is left alone.
fn history_on(doc: &mut DocumentMut) {
    if let Some(h) = doc.get_mut("history").and_then(Item::as_table_like_mut)
        && h.get("enabled").and_then(Item::as_bool) == Some(false)
    {
        put(h, "enabled", &toml_edit::value(true));
    }
}

fn history_enabled(doc: &DocumentMut) -> Option<bool> {
    doc.get("history")?
        .as_table_like()?
        .get("enabled")?
        .as_bool()
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `line` and `column` are 1-based; `column` counts characters, not bytes.
    #[error("{file} line {line}, column {column}: {message}", file = CONFIG_FILE)]
    Syntax {
        line: usize,
        column: usize,
        message: String,
    },
    #[error("{file}: {0}", file = CONFIG_FILE)]
    Invalid(String),
    #[error("{file} is from a newer version and is read-only", file = CONFIG_FILE)]
    ReadOnly,
}

/// A loaded `config.toml`: typed settings plus the raw document, so comments, key order and
/// keys this build does not know survive a write.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools, reason = "independent load flags")]
pub struct ConfigFile {
    pub config: Config,
    /// True when the file's `version` is newer than this build: never write it back.
    pub read_only: bool,
    /// Values the load had to correct, for the tray and settings to show.
    pub warnings: Vec<String>,
    /// True when this load migrated `history.enabled` from an explicit `false` to `true`, so
    /// the daemon can show a one-time notice.
    pub history_turned_on: bool,
    raw: DocumentMut,
    /// The file started with a UTF-8 byte order mark; written back with it.
    bom: bool,
    /// Most lines of the file ended in CRLF; every line is written back with CRLF.
    crlf: bool,
}

impl Default for ConfigFile {
    /// Every key with its default, for a new file.
    fn default() -> Self {
        // Serialising our own `Config` does not fail; an empty document is still a valid file.
        let mut raw = typed_doc(&Config::default()).unwrap_or_default();
        raw.insert("version", toml_edit::value(CURRENT_VERSION));
        Self {
            config: Config::default(),
            read_only: false,
            warnings: Vec::new(),
            history_turned_on: false,
            raw,
            bom: false,
            crlf: false,
        }
    }
}

impl ConfigFile {
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        Self::parse_with(text, MIGRATIONS)
    }

    /// `parse` with an explicit migration list (tests ship their own).
    pub fn parse_with(text: &str, migrations: &[Migration]) -> Result<Self, ConfigError> {
        let bom = text.starts_with('\u{feff}');
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        // A mixed file is normalised to its dominant ending; a tie goes to CRLF in a BOM file.
        let crlf_lines = text.matches("\r\n").count();
        let lf_lines = text.matches('\n').count() - crlf_lines;
        let crlf = crlf_lines > lf_lines || (crlf_lines == lf_lines && bom);
        // The document is kept with LF; `to_toml_string` puts the CRLF back.
        let text = if crlf_lines > 0 {
            Cow::Owned(text.replace("\r\n", "\n"))
        } else {
            Cow::Borrowed(text)
        };
        let text = &*text;
        let mut raw: DocumentMut = text
            .parse()
            .map_err(|e: toml_edit::TomlError| located(text, e.span(), e.message()))?;
        let current = i64::try_from(migrations.len() + 1).unwrap_or(i64::MAX);
        let version = match raw.get("version") {
            None => 1,
            Some(item) => match item.as_integer() {
                Some(v) if v >= 1 => v,
                _ => {
                    return Err(ConfigError::Invalid(
                        "`version` must be an integer >= 1".into(),
                    ));
                }
            },
        };
        let read_only = version > current;
        // `version >= 1`, so the index is non-negative and at most `migrations.len()`.
        let done = usize::try_from(version - 1).unwrap_or(0);
        let pending = if read_only {
            &[][..]
        } else {
            &migrations[done..]
        };
        let history_before = history_enabled(&raw);
        for migration in pending {
            migration(&mut raw);
        }
        let history_turned_on =
            history_before == Some(false) && history_enabled(&raw) == Some(true);
        // Typed values come from the migrated text, so error lines point into it; with no
        // migration that text is the file itself.
        let migrated = if pending.is_empty() {
            Cow::Borrowed(text)
        } else {
            Cow::Owned(raw.to_string())
        };
        let mut config = toml::from_str::<Config>(&migrated)
            .map_err(|e| located(&migrated, e.span(), e.message()))?;
        let warnings = config
            .audio
            .clamp_meter_sensitivity()
            .into_iter()
            .chain(config.model.normalise_recent_languages())
            .chain(config.sounds.normalise())
            .collect();
        if !read_only {
            put(raw.as_table_mut(), "version", &toml_edit::value(current));
        }
        Ok(Self {
            config,
            read_only,
            warnings,
            history_turned_on,
            raw,
            bom,
            crlf,
        })
    }

    /// The raw document with the typed settings written into its existing items, so an
    /// unchanged file comes back byte-identical (BOM and CRLF included) and a changed value
    /// changes only its text. A key the file lacks is added only when it is not the default.
    /// Errors with `ReadOnly` for a file from a newer version.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        if self.read_only {
            return Err(ConfigError::ReadOnly);
        }
        let mut typed = typed_doc(&self.config)?;
        unplace(typed.as_table_mut());
        let default = typed_doc(&Config::default())?;
        let mut out = self.raw.clone();
        merge(
            out.as_table_mut(),
            typed.as_table(),
            Some(default.as_table()),
        );
        let mut text = out.to_string();
        if self.crlf {
            text = text.replace('\n', "\r\n");
        }
        if self.bom {
            text.insert(0, '\u{feff}');
        }
        Ok(text)
    }
}

fn typed_doc(config: &Config) -> Result<DocumentMut, ConfigError> {
    toml::to_string(config)
        .map_err(|e| ConfigError::Invalid(e.to_string()))?
        .parse()
        .map_err(|e: toml_edit::TomlError| ConfigError::Invalid(e.to_string()))
}

/// Tables whose keys are user data (a map), not a fixed set: a key the typed config lacks is
/// removed from the file. Any other table keeps the keys this build does not know.
const MAP_TABLES: [&str; 1] = ["app_names"];

/// Writes every key of `src` into `dst`: tables merge key by key, arrays of tables merge
/// element by element on `app`, values replace only when they differ and keep their decor.
/// `default` is the same place in the default config (`None` inside a rule).
fn merge(dst: &mut dyn TableLike, src: &dyn TableLike, default: Option<&dyn TableLike>) {
    for (key, new) in src.iter() {
        let fallback = default.and_then(|d| d.get(key));
        let Some(old) = dst.get_mut(key) else {
            insert_changed(dst, key, new, fallback);
            continue;
        };
        if is_table_array(new) && is_table_array(old) {
            if old.is_array_of_tables() && new.as_array().is_some_and(Array::is_empty) {
                // An empty `[[x]]` prints nothing, and a missing key loads the default rules;
                // a fresh key gets plain `x = []` spacing.
                dst.remove(key);
                dst.insert(key, new.clone());
            } else {
                merge_rules(old, new);
            }
        } else if let (Some(o), Some(n)) = (old.as_table_like_mut(), new.as_table_like()) {
            merge(o, n, fallback.and_then(Item::as_table_like));
            if MAP_TABLES.contains(&key) {
                let gone: Vec<String> = o
                    .iter()
                    .map(|(k, _)| k.to_owned())
                    .filter(|k| !n.contains_key(k))
                    .collect();
                for k in gone {
                    o.remove(&k);
                }
            }
        } else {
            put(dst, key, new);
        }
    }
}

/// A key the file does not have is written only where it differs from the default, so a
/// minimal file stays minimal and keeps following future defaults.
fn insert_changed(dst: &mut dyn TableLike, key: &str, new: &Item, default: Option<&Item>) {
    match (new, default) {
        (Item::Table(n), Some(Item::Table(d))) => {
            let mut t = Table::new();
            merge(&mut t, n, Some(d));
            if !t.is_empty() {
                dst.insert(key, Item::Table(t));
            }
        }
        (_, Some(d)) if same_item(new, d) => {}
        _ => {
            dst.insert(key, new.clone());
        }
    }
}

/// `same` for items, with `[table]` and `[[array]]` compared as their inline forms.
fn same_item(a: &Item, b: &Item) -> bool {
    match (a.clone().into_value(), b.clone().into_value()) {
        (Ok(a), Ok(b)) => same(&a, &b),
        _ => false,
    }
}

/// Sets `key` to `new` unless it already holds the same value; a replaced value keeps the
/// old one's spacing and trailing comment.
fn put(dst: &mut dyn TableLike, key: &str, new: &Item) {
    match (dst.get_mut(key), new) {
        (Some(Item::Value(old)), Item::Value(new)) => {
            if !same(old, new) {
                let decor = old.decor().clone();
                *old = new.clone();
                *old.decor_mut() = decor;
            }
        }
        _ => {
            dst.insert(key, new.clone());
        }
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value().to_bits() == b.value().to_bits(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same(a, b))
        }
        (Value::InlineTable(a), Value::InlineTable(b)) => {
            a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|w| same(v, w)))
        }
        _ => false,
    }
}

/// `[[x]]` or `x = [{ ... }, ...]` (an empty array counts: it may hold rules later).
fn is_table_array(item: &Item) -> bool {
    match item {
        Item::ArrayOfTables(_) => true,
        Item::Value(Value::Array(a)) => a.iter().all(Value::is_inline_table),
        _ => false,
    }
}

fn app(rule: &dyn TableLike) -> Option<&str> {
    rule.get("app").and_then(Item::as_str)
}

/// The array of tables (only `paste.rules`) keyed by `app`: a rule that stays keeps its
/// formatting and unknown keys, a known optional key the typed rule no longer has is removed,
/// rules the typed list dropped go.
fn merge_rules(old: &mut Item, new: &Item) {
    let news: Vec<&dyn TableLike> = match new {
        Item::ArrayOfTables(a) => a.iter().map(|t| t as &dyn TableLike).collect(),
        Item::Value(Value::Array(a)) => a
            .iter()
            .filter_map(Value::as_inline_table)
            .map(|t| t as &dyn TableLike)
            .collect(),
        _ => Vec::new(),
    };
    match old {
        Item::ArrayOfTables(old) => {
            // shortcut: a kept `[[...]]` rule keeps its place in the file, so a reordered list
            // prints in file order; fine while rules are keyed by app, not by position.
            let mut pool: Vec<Option<Table>> = old.iter().cloned().map(Some).collect();
            let mut out = ArrayOfTables::new();
            for n in news {
                let mut t = take(&mut pool, app(n)).unwrap_or_default();
                merge_rule(&mut t, n);
                out.push(t);
            }
            *old = out;
        }
        Item::Value(Value::Array(old)) => {
            let mut pool: Vec<Option<InlineTable>> = old
                .iter()
                .filter_map(Value::as_inline_table)
                .cloned()
                .map(Some)
                .collect();
            let mut out = Vec::new();
            for n in news {
                let mut t = take(&mut pool, app(n)).unwrap_or_default();
                let keys: Vec<String> = t.iter().map(|(k, _)| k.to_owned()).collect();
                merge_rule(&mut t, n);
                if !t.iter().map(|(k, _)| k).eq(keys.iter().map(String::as_str)) {
                    // A key came or went: respace the braces and commas of this rule only.
                    t.fmt();
                }
                out.push(Value::InlineTable(t));
            }
            // Spacing between elements belongs to the slot, not the rule; `clear` keeps the
            // array's own spacing and trailing comma.
            let slots: Vec<Decor> = old.iter().map(|v| v.decor().clone()).collect();
            old.clear();
            for (i, mut v) in out.into_iter().enumerate() {
                if let Some(decor) = slots.get(i) {
                    *v.decor_mut() = decor.clone();
                    old.push_formatted(v);
                } else {
                    old.push(v);
                }
            }
        }
        _ => {}
    }
}

fn take<T: TableLike>(pool: &mut [Option<T>], app_name: Option<&str>) -> Option<T> {
    pool.iter_mut()
        .find(|t| t.as_ref().is_some_and(|t| app(t) == app_name))
        .and_then(Option::take)
}

fn merge_rule(dst: &mut dyn TableLike, src: &dyn TableLike) {
    merge(dst, src, None);
    for key in PasteRule::KEYS {
        if !src.contains_key(key) {
            dst.remove(key);
        }
    }
}

/// Tables this build adds have no place in the file yet: let them follow their neighbours.
fn unplace(t: &mut Table) {
    t.set_position(None);
    for (_, item) in t.iter_mut() {
        match item {
            Item::Table(t) => unplace(t),
            Item::ArrayOfTables(a) => a.iter_mut().for_each(unplace),
            _ => {}
        }
    }
}

fn located(text: &str, span: Option<Range<usize>>, message: &str) -> ConfigError {
    let at = span.map_or(0, |s| s.start).min(text.len());
    let before = text.get(..at).unwrap_or_default();
    let line = before.matches('\n').count() + 1;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    ConfigError::Syntax {
        line,
        column: before[line_start..].chars().count() + 1,
        message: message.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_removed_app_name_leaves_the_file_and_other_keys_and_comments_stay() {
        let text = "version = 1
# keep me
future = 1
[ui]
theme = \"dark\" # mine
unknown = 2

[ui.app_names]
\"a.exe\" = \"Aaa\" # note
\"b.exe\" = \"Bbb\"
";
        let mut file = ConfigFile::parse(text).unwrap();
        assert_eq!(file.config.ui.app_names.len(), 2);
        file.config.ui.app_names.remove("a.exe");
        let out = file.to_toml_string().unwrap();
        assert!(!out.contains("a.exe") && !out.contains("Aaa"), "{out}");
        for kept in [
            "# keep me",
            "future = 1",
            "theme = \"dark\" # mine",
            "unknown = 2",
            "\"b.exe\" = \"Bbb\"",
        ] {
            assert!(
                out.contains(kept),
                "{kept} lost:
{out}"
            );
        }
        let back = ConfigFile::parse(&out).unwrap();
        assert_eq!(
            back.config.ui.app_names.keys().collect::<Vec<_>>(),
            ["b.exe"]
        );
    }
}
