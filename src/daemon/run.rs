//! Daemon entry point (ARCHITECTURE section 2): starts every thread, runs the UI loop on the
//! main thread, and joins the threads in order on the way out.

use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::Context;
use oxi_core::config::{Config, ConfigError, ConfigFile, Hotkey};
use oxi_core::hotkey::Chord;
use oxi_core::ipc::Push;
use oxi_core::machine::{Notice, PhaseCell};
use oxi_core::paths::CONFIG_FILE;
use oxi_core::types::Millis;
use oxi_platform::capture::LevelMeter;
use oxi_platform::os::hook::{Hook, HookHandle};
use oxi_platform::os::overlay::Overlay;
use oxi_platform::os::tray::{MainLoop, MenuItem, Waker};

use super::cmd::{ChordHook, Inbox, UiCmd, Workers};
use super::coordinator::Coordinator;
use super::ipc::{Pushes, Start, SystemHost};
use super::ui::Ui;
use super::{audio, io, ipc, paste, watch, whisper};
use crate::log;

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

    // The overlay first: it makes the process per-monitor DPI aware (v2) before any window exists.
    let meter = LevelMeter::default();
    let overlay = Overlay::new(meter.clone(), None).context("overlay window")?;
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
    let (io_tx, io_rx) = mpsc::channel();
    let workers = [
        audio::spawn(audio_rx, inbox.clone(), meter)?,
        whisper::spawn(whisper_rx, inbox.clone(), layout.clone())?,
        paste::spawn(paste_rx, inbox.clone())?,
    ];
    let pushes = Pushes::default();
    // Never joined: the IPC threads hold its sender until the process ends.
    io::spawn(io_rx, pushes.clone(), layout.models_dir())?;
    let host = Arc::new(SystemHost::new(Arc::clone(&hook), Box::new(backend)));
    ipc::spawn(server, inbox.clone(), io_tx, host, pushes.clone(), keys_rx)?;

    let (watch_tx, watch_rx) = mpsc::channel();
    let watcher = watch::spawn(&layout, watch_tx, notify).context("config watcher")?;
    // A parse error also goes to the settings window, which shows its line.
    forward("config-events", watch_rx, inbox.clone(), move |msg| {
        if let Inbox::ConfigError(e) = &msg {
            pushes.send(config_error_push(e));
        }
        msg
    })?;

    let ui_config = config.clone();
    let coordinator = {
        let workers = Workers {
            audio: audio_tx,
            whisper: whisper_tx,
            paste: paste_tx,
            ui: ui_tx,
            wake_ui: Box::new({
                let waker = waker.clone();
                move || {
                    let _ = waker.wake();
                }
            }),
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
        |_, item| on_menu(item, &inbox, &waker),
        move |event| {
            let _ = session.send(Inbox::Event(event));
        },
    );

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

/// The start config. One that does not parse starts on the defaults and is reported like a
/// bad reload; the watcher applies the file once it is fixed.
fn load_config(path: &Path) -> (Config, Vec<String>, Option<ConfigError>) {
    let parsed = std::fs::read_to_string(path)
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

fn on_menu(item: MenuItem, inbox: &Sender<Inbox>, waker: &Waker) {
    match item {
        MenuItem::Settings => open_settings(),
        MenuItem::CopyLast => {
            let _ = inbox.send(Inbox::CopyLastText);
        }
        MenuItem::Quit => {
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

/// `Status.backend`.
// shortcut: the compiled backend, not the device the model loaded on; upgrade when the whisper
// worker publishes `LoadInfo.device`.
fn backend() -> String {
    let compiled = if cfg!(feature = "cuda") {
        "cuda"
    } else if cfg!(feature = "vulkan") {
        "vulkan"
    } else if cfg!(feature = "metal") {
        "metal"
    } else {
        "cpu"
    };
    compiled.to_owned()
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
