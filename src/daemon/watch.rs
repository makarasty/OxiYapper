//! Config file watcher (ARCHITECTURE section 6): watches the config directory, not the file, so
//! an atomic rename-over is seen. A bad `config.toml` never replaces the last good one.
//!
//! The `config-watch` thread owns the debouncer; `run.rs` keeps the returned [`Watcher`] alive for the daemon's
//! life. `notify` is how a tray notice reaches the UI (`run.rs` wraps `UiCmd::Notify` and the
//! UI wake-up): once per reload that failed (`ConfigInvalid`) or was corrected (`ConfigAdjusted`).

use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{
    DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache, file_id, new_debouncer,
};
use oxi_core::config::{ConfigError, ConfigFile};
use oxi_core::ipc::{Push, WatchedFile};
use oxi_core::machine::Notice;
use oxi_core::paths::{CONFIG_FILE, DICTIONARY_FILE, Layout, REPLACE_FILE};

use super::cmd::Inbox;

/// How long a read that failed for another reason than "gone" or "not UTF-8" (an editor holding
/// the file at its flush) is retried: `READ_RETRIES` more tries, `READ_RETRY_DELAY` apart.
const READ_RETRIES: u32 = 3;
const READ_RETRY_DELAY: Duration = Duration::from_millis(100);
/// Watching again after an error or the directory's removal: the first `REWATCH_TRIES` waits
/// start at `REWATCH_FIRST` and double up to `REWATCH_CAP`; after that, one try per `REWATCH_SLOW`.
const REWATCH_TRIES: u32 = 8;
const REWATCH_FIRST: Duration = Duration::from_millis(100);
const REWATCH_CAP: Duration = Duration::from_secs(2);
const REWATCH_SLOW: Duration = Duration::from_secs(30);

const DEBOUNCE: Duration = Duration::from_millis(200);
/// How often the thread checks that the watched directory is still the one it watches: Windows
/// reports no event when it is deleted, and the watch is then dead. A delete and re-create inside
/// one period keeps the NTFS file id and is not noticed (shortcut; upgrade: stat-snapshot).
const DIR_CHECK: Duration = Duration::from_secs(2);

/// What the debouncer and the owner say to the watcher thread.
enum Msg {
    Events(DebounceEventResult),
    Stop,
}

/// Keeps the watch running; dropping it stops it.
pub struct Watcher {
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

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
    start(layout, DEBOUNCE, DIR_CHECK, inbox, notify)
}

fn start(
    layout: &Layout,
    debounce: Duration,
    dir_check: Duration,
    inbox: Sender<Inbox>,
    notify: impl Fn(Notice) + Send + 'static,
) -> notify_debouncer_full::notify::Result<Watcher> {
    let (tx, rx) = mpsc::channel();
    let events_tx = tx.clone();
    let mut debouncer = new_debouncer(debounce, None, move |result| {
        let _ = events_tx.send(Msg::Events(result));
    })?;
    let dir = layout.config_dir.clone();
    debouncer.watch(&dir, RecursiveMode::NonRecursive)?;
    let config_file = layout.config_file();
    let thread = thread::Builder::new()
        .name("config-watch".into())
        .spawn(move || {
            run(
                &rx,
                &mut debouncer,
                &dir,
                dir_check,
                &config_file,
                &inbox,
                &notify,
            );
        })
        .map_err(notify_debouncer_full::notify::Error::io)?;
    Ok(Watcher {
        tx,
        thread: Some(thread),
    })
}

/// The watcher thread: reload what changed; after a watch error, an overflow, or the removal
/// of the directory, watch again and reload everything once.
fn run(
    rx: &Receiver<Msg>,
    debouncer: &mut Debouncer<RecommendedWatcher, RecommendedCache>,
    dir: &Path,
    dir_check: Duration,
    config_file: &Path,
    inbox: &Sender<Inbox>,
    notify: &impl Fn(Notice),
) {
    let mut dir_id = file_id::get_file_id(dir).ok();
    loop {
        let (changed, lost) = match rx.recv_timeout(dir_check) {
            Ok(Msg::Events(Ok(events))) => {
                let lost = dir_removed(&events, dir);
                let changed = if lost {
                    Changed::ALL
                } else {
                    Changed::from_events(&events)
                };
                (changed, lost)
            }
            Ok(Msg::Events(Err(errors))) => {
                for e in &errors {
                    tracing::warn!("config watch error: {:?}", e.kind);
                }
                (Changed::ALL, true)
            }
            Err(RecvTimeoutError::Timeout) => {
                if file_id::get_file_id(dir).ok() == dir_id {
                    continue;
                }
                (Changed::ALL, true)
            }
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
        };
        if lost {
            if !rewatch(rx, debouncer, dir) {
                return;
            }
            dir_id = file_id::get_file_id(dir).ok();
        }
        if changed.config {
            reload_config(config_file, inbox, notify);
            super::ipc::announce(Push::FileChanged {
                file: WatchedFile::Config,
            });
        }
        if changed.assets {
            let _ = inbox.send(Inbox::AssetsChanged);
            // One flag for both files: the window checks each of them.
            for file in [WatchedFile::Dictionary, WatchedFile::Replace] {
                super::ipc::announce(Push::FileChanged { file });
            }
        }
    }
}

/// The wait before re-watch try number `attempt` (0-based).
fn rewatch_delay(attempt: u32) -> Duration {
    if attempt >= REWATCH_TRIES {
        return REWATCH_SLOW;
    }
    REWATCH_FIRST.saturating_mul(1 << attempt).min(REWATCH_CAP)
}

/// Watch `dir` again, backing off, until it works. False when the owner stopped us meanwhile.
fn rewatch(
    rx: &Receiver<Msg>,
    debouncer: &mut Debouncer<RecommendedWatcher, RecommendedCache>,
    dir: &Path,
) -> bool {
    let _ = debouncer.unwatch(dir);
    let mut attempt = 0u32;
    loop {
        // The wait is also where a Stop gets through; events queued meanwhile are covered by
        // the full reload that follows.
        match rx.recv_timeout(rewatch_delay(attempt)) {
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return false,
            Ok(Msg::Events(_)) | Err(RecvTimeoutError::Timeout) => {}
        }
        match debouncer.watch(dir, RecursiveMode::NonRecursive) {
            Ok(()) => return true,
            Err(e) => tracing::warn!("config watch not re-armed: {:?}", e.kind),
        }
        if attempt + 1 == REWATCH_TRIES {
            tracing::error!("config directory not watched; retrying every 30 s");
        }
        attempt = attempt.saturating_add(1);
    }
}

/// The watched directory itself was removed (inotify reports it as an event).
fn dir_removed(events: &[DebouncedEvent], dir: &Path) -> bool {
    events
        .iter()
        .any(|e| matches!(e.kind, EventKind::Remove(_)) && e.paths.iter().any(|p| p == dir))
}

/// Which watched files a batch of events touched.
#[derive(Default)]
struct Changed {
    config: bool,
    assets: bool,
}

impl Changed {
    const ALL: Self = Self {
        config: true,
        assets: true,
    };

    /// An overflow (`need_rescan`) means events were lost: treat everything as changed.
    fn from_events(events: &[DebouncedEvent]) -> Self {
        if events.iter().any(|e| e.need_rescan()) {
            return Self::ALL;
        }
        let mut changed = Self::default();
        for path in events
            .iter()
            .filter(|e| !matches!(e.kind, EventKind::Access(_)))
            .flat_map(|e| &e.paths)
        {
            changed.note(path);
        }
        changed
    }

    fn note(&mut self, path: &Path) {
        match path.file_name().and_then(|n| n.to_str()) {
            Some(CONFIG_FILE) => self.config = true,
            Some(DICTIONARY_FILE | REPLACE_FILE) => self.assets = true,
            _ => {}
        }
    }
}

/// `read_to_string`, retried when the failure may pass (a sharing violation).
pub(super) fn read_with_retry(path: &Path) -> io::Result<String> {
    let mut retries = 0;
    loop {
        match fs::read_to_string(path) {
            Err(e)
                if retries < READ_RETRIES
                    && !matches!(e.kind(), ErrorKind::NotFound | ErrorKind::InvalidData) =>
            {
                retries += 1;
                thread::sleep(READ_RETRY_DELAY);
            }
            result => return result,
        }
    }
}

/// Read and parse `config.toml`, send the result to the coordinator. A file that is gone or
/// unreadable right now (mid-rename) is skipped: the last good config stays.
fn reload_config(path: &Path, inbox: &Sender<Inbox>, notify: &impl Fn(Notice)) {
    let text = match read_with_retry(path) {
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
    use crate::log::scratch::scratch_dir;

    fn layout(name: &str) -> Layout {
        let dir = scratch_dir(&format!("oxi-watch-{name}-{}", std::process::id()));
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

    /// How long a message that must come may take. `recv_timeout` returns the moment it arrives,
    /// so this costs nothing when the box is idle and covers a starved watcher thread under load.
    const WAIT_MS: u64 = 15_000;

    /// Waits up to `WAIT_MS` for the config with `device`.
    fn config_for(rx: &Receiver<Inbox>, device: &str) -> bool {
        let deadline = std::time::Instant::now() + Duration::from_millis(WAIT_MS);
        while std::time::Instant::now() < deadline {
            if matches!(next(rx, 100), Some(Inbox::Config { config, .. }) if config.audio.device == device)
            {
                return true;
            }
        }
        false
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
    fn rewatch_backs_off_then_goes_slow() {
        let ms = |a| rewatch_delay(a).as_millis();
        assert_eq!(
            [ms(0), ms(1), ms(4), ms(5), ms(7)],
            [100, 200, 1600, 2000, 2000]
        );
        assert_eq!(rewatch_delay(REWATCH_TRIES), REWATCH_SLOW);
        assert_eq!(rewatch_delay(u32::MAX), REWATCH_SLOW);
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
        let _w = start(
            &l,
            Duration::from_millis(50),
            Duration::from_millis(100),
            tx,
            notify,
        )
        .unwrap();
        for device in ["Mic A", "Mic B"] {
            write_atomic(
                &l.config_file(),
                format!("[audio]\ndevice = \"{device}\"\n").as_bytes(),
            )
            .unwrap();
            match next(&rx, WAIT_MS) {
                Some(Inbox::Config { config, warnings }) => {
                    assert_eq!(config.audio.device, device);
                    assert_eq!(warnings, Vec::<String>::new());
                }
                _ => panic!("no config for {device}"),
            }
        }
        assert!(next(&rx, 200).is_none(), "exactly one reload per write");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn broken_config_sends_error_with_line_and_no_config() {
        let l = layout("broken");
        let (tx, rx) = mpsc::channel();
        let (seen, notify) = notices();
        let _w = start(
            &l,
            Duration::from_millis(50),
            Duration::from_millis(100),
            tx,
            notify,
        )
        .unwrap();
        write_atomic(&l.config_file(), b"[audio]\ngain_db = \n").unwrap();
        match next(&rx, WAIT_MS) {
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
        let _w = start(
            &l,
            Duration::from_millis(50),
            Duration::from_millis(100),
            tx,
            notify,
        )
        .unwrap();
        write_atomic(&l.config_file(), b"[audio]\nmeter_sensitivity_db = 99.0\n").unwrap();
        match next(&rx, WAIT_MS) {
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
        let _w = start(
            &l,
            Duration::from_millis(50),
            Duration::from_millis(100),
            tx,
            notify,
        )
        .unwrap();
        write_atomic(&l.dictionary_file(), b"word\n").unwrap();
        assert!(matches!(next(&rx, WAIT_MS), Some(Inbox::AssetsChanged)));
        write_atomic(&l.config_dir.join("other.txt"), b"x").unwrap();
        assert!(next(&rx, 200).is_none());
    }

    #[test]
    fn watch_survives_the_directory_being_removed() {
        let l = layout("rearm");
        let (tx, rx) = mpsc::channel();
        let (_seen, notify) = notices();
        let _w = start(
            &l,
            Duration::from_millis(50),
            Duration::from_millis(100),
            tx,
            notify,
        )
        .unwrap();
        // shortcut: NTFS gives a recreated directory the same file id, so a gap shorter than the
        // check period goes unnoticed. The gap is two periods; if a starved watcher thread still
        // missed it, the cycle is repeated (the test only fails when no cycle ever re-arms).
        let mut got = false;
        for _ in 0..5 {
            let _ = fs::remove_dir_all(&l.config_dir);
            thread::sleep(Duration::from_millis(250));
            fs::create_dir_all(&l.config_dir).unwrap();
            write_atomic(
                &l.config_file(),
                b"[audio]
device = \"Mic R\"
",
            )
            .unwrap();
            got = config_for(&rx, "Mic R");
            if got {
                break;
            }
        }
        assert!(got, "no config after the directory came back");
        // Edits after the re-arm are still seen.
        write_atomic(
            &l.config_file(),
            b"[audio]
device = \"Mic S\"
",
        )
        .unwrap();
        assert!(config_for(&rx, "Mic S"), "edit after re-arm lost");
    }

    /// An editor holding the file without sharing (Windows) fails the read for a moment.
    #[cfg(windows)]
    #[test]
    fn locked_file_is_retried_not_lost() {
        use std::os::windows::fs::OpenOptionsExt;
        let l = layout("locked");
        fs::write(
            l.config_file(),
            "[audio]
device = \"Mic L\"
",
        )
        .unwrap();
        let held = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(l.config_file())
            .unwrap();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let (tx, rx) = mpsc::channel();
        let (_seen, notify) = notices();
        reload_config(&l.config_file(), &tx, &notify);
        release.join().unwrap();
        assert!(
            matches!(next(&rx, 10), Some(Inbox::Config { config, .. }) if config.audio.device == "Mic L")
        );
    }
}
