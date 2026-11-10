//! Daemon entry point (ARCHITECTURE section 2): starts every thread, runs the UI loop on the
//! main thread, and joins the threads in order on the way out.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use ab_glyph::FontRef;
use anyhow::Context;
use oxi_core::config::{Config, ConfigError, ConfigFile, Hotkey};
use oxi_core::hotkey::Chord;
use oxi_core::ipc::Push;
use oxi_core::machine::{Notice, PhaseCell};
use oxi_core::paths::CONFIG_FILE;
use oxi_core::tray::MenuItem;
use oxi_core::types::Millis;
use oxi_platform::capture::LevelMeter;
use oxi_platform::os::hook::{Hook, HookHandle};
use oxi_platform::os::overlay::Overlay;
use oxi_platform::os::tray::{MainLoop, Waker};

use super::cmd::{ChordHook, Inbox, UiCmd, Workers};
use super::coordinator::Coordinator;
use super::ipc::{Pushes, Start, SystemHost};
use super::ui::Ui;
use super::{audio, history, io, ipc, paste, sound, watch, whisper};
use crate::log;

#[allow(
    clippy::too_many_lines,
    reason = "start-up wiring reads top to bottom, one thread after another"
)]
pub fn run() -> anyhow::Result<()> {
    let layout = oxi_platform::dirs::layout().context("no data directory")?;
    oxi_platform::dirs::ensure(&layout).context("cannot create the data directories")?;
    let _log = log::init(&layout.logs_dir);
    // An error is a refusal to start; main prints it to stderr and exits non-zero.
    let server = match ipc::bind()? {
        Start::Serve(server) => server,
        Start::Forwarded => return Ok(()),
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "daemon started");
    watch::ensure_config(&layout).context("cannot write the default config.toml")?;
    let (config, warnings, config_error) = load_config(&layout.config_file());
    // A config that does not parse is no answer: autostart stays as it is until it is fixed.
    if config_error.is_none() {
        sync_autostart(config.system.autostart);
    }

    // The overlay first: it makes the process per-monitor DPI aware (v2) before any window exists.
    let meter = LevelMeter::default();
    let overlay = Overlay::new(meter.clone(), label_font()).context("overlay window")?;
    let mut tray = MainLoop::new().context("tray icon")?;
    let waker = tray.waker();
    let (ui_tx, ui_rx) = mpsc::channel();
    let notify = notifier(&ui_tx, &waker);

    let (inbox, inbox_rx) = mpsc::channel();
    if let Some(e) = config_error {
        notify(Notice::ConfigInvalid);
        let _ = inbox.send(Inbox::ConfigError(e));
    } else if !warnings.is_empty() {
        notify(Notice::ConfigAdjusted);
    }

    let phase = PhaseCell::default();
    let (events_tx, events_rx) = mpsc::channel();
    let (keys_tx, keys_rx) = mpsc::channel();
    let hook = Arc::new(
        Hook::spawn(
            start_chord(&config.hotkey),
            phase.clone(),
            events_tx,
            keys_tx,
        )
        .context("keyboard hook")?,
    );
    forward("hook-events", events_rx, inbox.clone(), Inbox::Event)?;

    let (audio_tx, audio_rx) = mpsc::channel();
    let (whisper_tx, whisper_rx) = mpsc::channel();
    let (paste_tx, paste_rx) = mpsc::channel();
    let (sound_tx, sound_rx) = mpsc::channel();
    let (history_tx, history_rx) = mpsc::channel();
    let (io_tx, io_rx) = mpsc::channel();
    let workers = [
        audio::spawn(audio_rx, inbox.clone(), meter)?,
        whisper::spawn(whisper_rx, inbox.clone(), layout.clone(), phase.clone())?,
        paste::spawn(paste_rx, inbox.clone())?,
        sound::spawn(sound_rx, inbox.clone())?,
        history::spawn(history_rx, inbox.clone(), layout.history_dir())?,
    ];
    let pushes = Pushes::default();
    // Before any download can resume: a model `.part` untouched for 7 days is dropped.
    sweep_partials(&layout.models_dir());
    // Never joined: the IPC threads hold its sender until the process ends.
    io::spawn(io_rx, pushes.clone(), layout.models_dir())?;
    let host = Arc::new(SystemHost::new(Arc::clone(&hook)));
    let quit_host = Arc::clone(&host);
    ipc::spawn(server, inbox.clone(), io_tx, host, pushes.clone(), keys_rx)?;

    let (watch_tx, watch_rx) = mpsc::channel();
    let watcher = watch::spawn(&layout, watch_tx, notify).context("config watcher")?;
    // A parse error also goes to the settings window, which shows its line.
    forward("config-events", watch_rx, inbox.clone(), move |msg| {
        match &msg {
            Inbox::ConfigError(e) => {
                pushes.send(config_error_push(e));
            }
            Inbox::Config { config, .. } => sync_autostart(config.system.autostart),
            _ => {}
        }
        msg
    })?;

    let ui_config = config.clone();
    let coordinator = {
        let workers = Workers {
            audio: audio_tx,
            whisper: whisper_tx,
            paste: paste_tx,
            sound: sound_tx,
            history: history_tx,
            ui: ui_tx,
            wake_ui: wake_ui(&waker),
        };
        let (hook, quit) = (HookChord(Arc::clone(&hook)), QuitOnDrop(waker.clone()));
        spawn("coordinator", move || {
            // Its end (IPC Shutdown, tray Quit, a panic) is the daemon's end.
            let _quit = quit;
            let start = Instant::now();
            let now = || Millis(u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX));
            Coordinator::new(config, warnings, workers, hook, phase).run(&inbox_rx, now);
        })?
    };

    let mut ui = Ui::new(ui_rx, overlay, ui_config, waker.clone());
    let session = inbox.clone();
    tray.run(
        |tray| ui.on_wake(tray),
        |_, item| on_menu(item, &inbox, &waker, &quit_host),
        move |event| {
            let _ = session.send(Inbox::Event(event));
        },
    );

    // Whatever ended the loop (tray Quit, IPC Shutdown, a panic), no settings window starts now.
    quit_host.stop_opening_settings();
    // In order: the coordinator, whose end closed every worker's channel, then the workers,
    // then the watcher. The tray icon and the overlay go with `tray` and `ui`.
    join(coordinator);
    for worker in workers {
        join(worker);
    }
    drop(watcher);
    drop(ui);
    drop(tray);
    tracing::info!("daemon stopped");
    Ok(())
}

/// Makes the OS autostart entry follow `on` for this exe (item 54). A test daemon (the pipe
/// suffix set) never touches it: it would point the user's entry at a temp copy.
fn sync_autostart(on: bool) {
    use oxi_core::autostart::{Action, decide};
    use oxi_platform::autostart::{NAME, query, register, unregister};
    // A debug build runs from a cargo target dir; it must never repoint the user's Run entry there.
    if cfg!(debug_assertions) || std::env::var_os(oxi_platform::ipc::PIPE_SUFFIX_ENV).is_some() {
        return;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return tracing::warn!("autostart not checked, no exe path: {e}"),
    };
    let current = match query(NAME) {
        Ok(current) => current,
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => return,
        // Unreadable (another type, say): treated as absent, so autostart on writes it anew.
        Err(e) => {
            tracing::warn!("autostart entry unreadable: {e}");
            None
        }
    };
    let done = match decide(on, current.as_deref(), &exe) {
        Action::Keep => return,
        Action::Write(command) => register(NAME, &command),
        Action::Remove => unregister(NAME),
    };
    match done {
        Ok(()) => tracing::info!(on, "autostart entry updated"),
        Err(e) => tracing::warn!("autostart entry not updated: {e}"),
    }
}

/// The start config. One that does not parse starts on the defaults and is reported like a
/// bad reload; the watcher applies the file once it is fixed.
fn load_config(path: &Path) -> (Config, Vec<String>, Option<ConfigError>) {
    let parsed = watch::read_with_retry(path)
        .map_err(|e| ConfigError::Invalid(e.kind().to_string()))
        .and_then(|text| ConfigFile::parse(&text));
    match parsed {
        Ok(file) => (file.config, file.warnings, None),
        Err(e) => {
            tracing::warn!("config not applied, starting on the defaults: {e}");
            (Config::default(), Vec::new(), Some(e))
        }
    }
}

fn sweep_partials(models_dir: &Path) {
    match oxi_platform::fs::sweep_partials(models_dir, std::time::SystemTime::now()) {
        Ok(0) => {}
        Ok(n) => tracing::info!(removed = n, "removed stale partial model downloads"),
        Err(e) => tracing::warn!(error = %e, "cannot sweep partial model downloads"),
    }
}

/// Wakes the UI loop from any thread.
fn wake_ui(waker: &Waker) -> Box<dyn Fn() + Send> {
    let waker = waker.clone();
    Box::new(move || {
        let _ = waker.wake();
    })
}

/// The overlay label font (decision D4): the system Segoe UI. Without it the pill shows its icon
/// and meter only.
fn label_font() -> Option<FontRef<'static>> {
    let windir = std::env::var_os("WINDIR").unwrap_or_default();
    font_from(&Path::new(&windir).join("Fonts").join("segoeui.ttf"))
}

fn font_from(path: &Path) -> Option<FontRef<'static>> {
    let bytes = std::fs::read(path)
        .map_err(|e| tracing::warn!(kind = ?e.kind(), "overlay font not read, labels off"))
        .ok()?;
    // Leaked once: the overlay that borrows it lives until the process ends.
    FontRef::try_from_slice(Box::leak(bytes.into_boxed_slice()))
        .map_err(|e| tracing::warn!("overlay font not usable, labels off: {e}"))
        .ok()
}

/// The hook's first chord; a chord that does not parse falls back to the default one, as a
/// later `set_hotkey` keeps the old chord.
fn start_chord(hotkey: &Hotkey) -> Chord {
    Chord::parse(&hotkey.chord).unwrap_or_else(|e| {
        tracing::warn!("hotkey.chord not used ({e}), starting on the default chord");
        Chord::parse(&Hotkey::default().chord).unwrap_or_else(|_| unreachable!("default chord"))
    })
}

/// A tray balloon from any thread: the notice goes to the UI channel, then the UI wakes.
fn notifier(ui: &Sender<UiCmd>, waker: &Waker) -> impl Fn(Notice) + Send + 'static {
    let (ui, waker) = (ui.clone(), waker.clone());
    move |notice| {
        let _ = ui.send(UiCmd::Notify(notice));
        let _ = waker.wake();
    }
}

fn on_menu(item: MenuItem, inbox: &Sender<Inbox>, waker: &Waker, host: &SystemHost) {
    match item {
        MenuItem::Settings => open_settings(),
        MenuItem::CopyLast => {
            let _ = inbox.send(Inbox::CopyLastText);
        }
        MenuItem::Quit => {
            host.close_settings();
            if inbox.send(Inbox::Shutdown).is_err() {
                let _ = waker.quit(); // no coordinator left to end the loop
            }
        }
    }
}

/// The path of a second launch: `OpenSettings` on our own pipe raises an open settings window
/// or starts one. Off the UI thread, since it waits for the IPC thread's answer.
fn open_settings() {
    let forwarded = spawn("open-settings", || {
        let sent = oxi_platform::ipc::default_name()
            .map_err(oxi_platform::ipc::Error::Io)
            .and_then(|name| oxi_platform::ipc::forward_open_settings(&name));
        if let Err(e) = sent {
            tracing::warn!("settings not opened: {e}");
        }
    });
    if let Err(e) = forwarded {
        tracing::warn!("settings not opened: {e}");
    }
}

/// `Push::ConfigError` for the settings window: the parser's own line and message.
fn config_error_push(e: &ConfigError) -> Push {
    let (line, message) = match e {
        ConfigError::Syntax { line, message, .. } => (*line, message.clone()),
        ConfigError::Invalid(message) => (0, message.clone()),
        ConfigError::ReadOnly => (0, e.to_string()),
    };
    Push::ConfigError {
        file: CONFIG_FILE.to_owned(),
        line,
        message,
    }
}

/// The coordinator's calls into the hook thread.
struct HookChord(Arc<HookHandle>);

impl ChordHook for HookChord {
    /// The hook thread answers with `ChordPolled` itself once it resynced its key state; until
    /// then `held: true` is a no-op for the machine.
    fn chord_held(&self) -> bool {
        self.0.poll();
        true
    }

    fn reinstall(&self) {
        self.0.reinstall();
    }

    fn set_hotkey(&self, hotkey: &Hotkey) {
        match Chord::parse(&hotkey.chord) {
            Ok(chord) => self.0.set_chord(chord),
            Err(e) => tracing::warn!("hotkey not changed: {e}"),
        }
    }
}

/// Ends the UI loop when dropped, also by a panic of the thread that holds it.
struct QuitOnDrop(Waker);

impl Drop for QuitOnDrop {
    fn drop(&mut self) {
        let _ = self.0.quit();
    }
}

/// A thread that hands each message of `rx` to the coordinator, until either side is gone.
fn forward<T: Send + 'static>(
    name: &str,
    rx: Receiver<T>,
    inbox: Sender<Inbox>,
    wrap: impl Fn(T) -> Inbox + Send + 'static,
) -> std::io::Result<JoinHandle<()>> {
    spawn(name, move || {
        for msg in rx {
            if inbox.send(wrap(msg)).is_err() {
                return;
            }
        }
    })
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> std::io::Result<JoinHandle<()>> {
    thread::Builder::new().name(name.into()).spawn(f)
}

fn join(handle: JoinHandle<()>) {
    let name = handle.thread().name().unwrap_or("?").to_owned();
    if handle.join().is_err() {
        tracing::error!(thread = %name, "thread panicked");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_error_push_carries_the_parser_line_and_message() {
        let cases = [
            (
                ConfigError::Syntax {
                    line: 3,
                    column: 7,
                    message: "expected `=`".into(),
                },
                3,
                "expected `=`".to_owned(),
            ),
            (
                ConfigError::Invalid("not valid UTF-8".into()),
                0,
                "not valid UTF-8".to_owned(),
            ),
            (ConfigError::ReadOnly, 0, ConfigError::ReadOnly.to_string()),
        ];
        for (error, want_line, want_message) in cases {
            assert_eq!(
                config_error_push(&error),
                Push::ConfigError {
                    file: CONFIG_FILE.to_owned(),
                    line: want_line,
                    message: want_message,
                },
                "{error}"
            );
        }
    }

    #[test]
    fn a_bad_or_missing_config_starts_on_the_defaults_with_its_error() {
        let dir = std::env::temp_dir().join(format!("oxi-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(CONFIG_FILE);

        let (config, _, error) = load_config(&path);
        assert_eq!(config, Config::default());
        assert!(matches!(error, Some(ConfigError::Invalid(_))), "{error:?}");

        std::fs::write(&path, "[hotkey\n").unwrap();
        let (config, _, error) = load_config(&path);
        assert_eq!(config, Config::default());
        assert!(
            matches!(error, Some(ConfigError::Syntax { .. })),
            "{error:?}"
        );

        std::fs::write(&path, "[hotkey]\nchord = [\"Ctrl\", \"Shift\", \"F13\"]\n").unwrap();
        let (config, _, error) = load_config(&path);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(config.hotkey.chord, ["Ctrl", "Shift", "F13"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_font_loads_a_font_file_and_refuses_anything_else() {
        let dir = std::env::temp_dir().join(format!("oxi-font-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(font_from(&dir.join("missing.ttf")).is_none());
        let junk = dir.join("junk.ttf");
        std::fs::write(&junk, b"not a font").unwrap();
        assert!(font_from(&junk).is_none());
        let _ = std::fs::remove_dir_all(&dir);
        // Every Windows install ships Segoe UI.
        if cfg!(windows) {
            assert!(label_font().is_some(), "no Segoe UI");
        }
    }

    #[test]
    fn a_bad_chord_starts_on_the_default_one() {
        let bad = Hotkey {
            chord: vec!["Ctrl".into(), "NoSuchKey".into()],
            ..Hotkey::default()
        };
        let default = Chord::parse(&Hotkey::default().chord).unwrap();
        assert_eq!(start_chord(&bad), default);
        let good = Hotkey {
            chord: vec!["Ctrl".into(), "Shift".into(), "F13".into()],
            ..Hotkey::default()
        };
        assert_ne!(start_chord(&good), default);
    }
}
