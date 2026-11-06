//! Config file watcher (ARCHITECTURE section 6): watches the config directory, not the file, so
//! an atomic rename-over is seen. A bad `config.toml` never replaces the last good one.
//!
//! The debouncer owns its thread; `run.rs` keeps the returned [`Watcher`] alive for the daemon's
//! life. `notify` is how a tray notice reaches the UI (`run.rs` wraps `UiCmd::Notify` and the
//! UI wake-up): once per reload that failed (`ConfigInvalid`) or was corrected (`ConfigAdjusted`).

#![allow(
    dead_code,
    reason = "run.rs (t40) starts the watcher; until then only the tests do"
)]

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::Duration;

use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use oxi_core::config::{ConfigError, ConfigFile};
use oxi_core::machine::Notice;
use oxi_core::paths::{CONFIG_FILE, DICTIONARY_FILE, Layout, REPLACE_FILE};

use super::cmd::Inbox;

/// Keeps the watch running; dropping it stops it.
pub type Watcher = Debouncer<RecommendedWatcher, RecommendedCache>;

const DEBOUNCE: Duration = Duration::from_millis(200);

/// Write the default `config.toml` when there is none. `create_new`, so a file that exists (or
/// appears meanwhile) is never touched.
pub fn ensure_config(layout: &Layout) -> io::Result<()> {
    fs::create_dir_all(&layout.config_dir)?;
    let text = ConfigFile::default()
        .to_toml_string()
        .map_err(io::Error::other)?;
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(layout.config_file())
    {
        Ok(mut f) => f.write_all(text.as_bytes()),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(()),
        Err(e) => Err(e),
    }
}

/// Start watching `layout.config_dir`.
pub fn spawn(
    layout: &Layout,
    inbox: Sender<Inbox>,
    notify: impl Fn(Notice) + Send + 'static,
) -> notify_debouncer_full::notify::Result<Watcher> {
    start(layout, DEBOUNCE, inbox, notify)
}

fn start(
    layout: &Layout,
    debounce: Duration,
    inbox: Sender<Inbox>,
    notify: impl Fn(Notice) + Send + 'static,
) -> notify_debouncer_full::notify::Result<Watcher> {
    let config_file = layout.config_file();
    let mut debouncer = new_debouncer(debounce, None, move |result: DebounceEventResult| {
        let Ok(events) = result else { return };
        let mut changed = Changed::default();
        for path in events
            .iter()
            .filter(|e| !matches!(e.kind, EventKind::Access(_)))
            .flat_map(|e| &e.paths)
        {
            changed.note(path);
        }
        if changed.config {
            reload_config(&config_file, &inbox, &notify);
        }
        if changed.assets {
            let _ = inbox.send(Inbox::AssetsChanged);
        }
    })?;
    debouncer.watch(&layout.config_dir, RecursiveMode::NonRecursive)?;
    Ok(debouncer)
}

/// Which watched files a batch of events touched.
#[derive(Default)]
struct Changed {
    config: bool,
    assets: bool,
}

impl Changed {
    fn note(&mut self, path: &Path) {
        match path.file_name().and_then(|n| n.to_str()) {
            Some(CONFIG_FILE) => self.config = true,
            Some(DICTIONARY_FILE | REPLACE_FILE) => self.assets = true,
            _ => {}
        }
    }
}

/// Read and parse `config.toml`, send the result to the coordinator. A file that is gone or
/// unreadable right now (mid-rename) is skipped: the last good config stays.
fn reload_config(path: &Path, inbox: &Sender<Inbox>, notify: &impl Fn(Notice)) {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => return,
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            send_error(
                inbox,
                notify,
                ConfigError::Invalid("not valid UTF-8".into()),
            );
            return;
        }
        Err(e) => {
            tracing::warn!("config.toml not read: {:?}", e.kind());
            return;
        }
    };
    match ConfigFile::parse(&text) {
        Ok(file) => {
            if !file.warnings.is_empty() {
                notify(Notice::ConfigAdjusted);
            }
            let _ = inbox.send(Inbox::Config {
                config: Box::new(file.config),
                warnings: file.warnings,
            });
        }
        Err(e) => send_error(inbox, notify, e),
    }
}

fn send_error(inbox: &Sender<Inbox>, notify: &impl Fn(Notice), e: ConfigError) {
    notify(Notice::ConfigInvalid);
    let _ = inbox.send(Inbox::ConfigError(e));
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};

    use oxi_platform::fs::write_atomic;

    use super::*;

    fn layout(name: &str) -> Layout {
        let dir = std::env::temp_dir().join(format!("oxi-watch-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Layout {
            config_dir: dir.clone(),
            data_dir: dir.clone(),
            logs_dir: dir,
        }
    }

    fn notices() -> (Arc<Mutex<Vec<Notice>>>, impl Fn(Notice) + Send + 'static) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        (seen, move |n| sink.lock().unwrap().push(n))
    }

    /// The next message within `ms`, or `None`.
    fn next(rx: &Receiver<Inbox>, ms: u64) -> Option<Inbox> {
        rx.recv_timeout(Duration::from_millis(ms)).ok()
    }

    #[test]
    fn ensure_config_creates_defaults_and_never_overwrites() {
        let l = layout("ensure");
        let path = l.config_file();
        ensure_config(&l).unwrap();
        assert!(ConfigFile::parse(&fs::read_to_string(&path).unwrap()).is_ok());
        fs::write(&path, "# mine\n").unwrap();
        ensure_config(&l).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "# mine\n");
    }

    #[test]
    fn changed_picks_watched_names_only() {
        let mut c = Changed::default();
        c.note(Path::new("d/config.toml.tmp"));
        c.note(Path::new("d/notes.txt"));
        assert!(!c.config && !c.assets);
        c.note(Path::new("d/replace.txt"));
        assert!(c.assets && !c.config);
        c.note(Path::new("d/config.toml"));
        assert!(c.config);
    }

    #[test]
    fn good_config_reloads_after_each_rename() {
        let l = layout("good");
        let (tx, rx) = mpsc::channel();
        let (seen, notify) = notices();
        let _w = start(&l, Duration::from_millis(50), tx, notify).unwrap();
        for device in ["Mic A", "Mic B"] {
            write_atomic(
                &l.config_file(),
                format!("[audio]\ndevice = \"{device}\"\n").as_bytes(),
            )
            .unwrap();
            match next(&rx, 1000) {
                Some(Inbox::Config { config, warnings }) => {
                    assert_eq!(config.audio.device, device);
                    assert_eq!(warnings, Vec::<String>::new());
                }
                _ => panic!("no config for {device}"),
            }
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn broken_config_sends_error_with_line_and_no_config() {
        let l = layout("broken");
        let (tx, rx) = mpsc::channel();
        let (seen, notify) = notices();
        let _w = start(&l, Duration::from_millis(50), tx, notify).unwrap();
        write_atomic(&l.config_file(), b"[audio]\ngain_db = \n").unwrap();
        match next(&rx, 1000) {
            Some(Inbox::ConfigError(ConfigError::Syntax { line, .. })) => assert_eq!(line, 2),
            _ => panic!("expected a syntax error"),
        }
        assert!(next(&rx, 200).is_none(), "no Config follows an error");
        assert_eq!(*seen.lock().unwrap(), [Notice::ConfigInvalid]);
    }

    #[test]
    fn corrected_values_notify_once_and_carry_warnings() {
        let l = layout("warn");
        let (tx, rx) = mpsc::channel();
        let (seen, notify) = notices();
        let _w = start(&l, Duration::from_millis(50), tx, notify).unwrap();
        write_atomic(&l.config_file(), b"[audio]\nmeter_sensitivity_db = 99.0\n").unwrap();
        match next(&rx, 1000) {
            Some(Inbox::Config { warnings, .. }) => assert_eq!(warnings.len(), 1),
            _ => panic!("expected a config"),
        }
        assert_eq!(*seen.lock().unwrap(), [Notice::ConfigAdjusted]);
    }

    #[test]
    fn text_files_send_assets_changed_and_nothing_else() {
        let l = layout("assets");
        let (tx, rx) = mpsc::channel();
        let (_seen, notify) = notices();
        let _w = start(&l, Duration::from_millis(50), tx, notify).unwrap();
        write_atomic(&l.dictionary_file(), b"word\n").unwrap();
        assert!(matches!(next(&rx, 1000), Some(Inbox::AssetsChanged)));
        write_atomic(&l.config_dir.join("other.txt"), b"x").unwrap();
        assert!(next(&rx, 200).is_none());
    }
}
