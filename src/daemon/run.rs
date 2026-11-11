//! Daemon entry point (ARCHITECTURE section 2): starts every thread, runs the UI loop on the
//! main thread, and joins the threads in order on the way out.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

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

/// How long the exit waits for the whisper worker. A model load cannot be cut and a run stops at
/// its next cancel check; past this the process ends without it (nothing of it is saved anyway).
const WHISPER_EXIT: Duration = Duration::from_secs(2);
/// One deadline for the whole exit when Windows ends the session: the coordinator's quit grace
/// (2 s), the workers' last commands (a history save, the clipboard) and the whisper join each
/// get what is left of it. Under the 5 s Windows gives before it reports "preventing shutdown".
const END_SESSION_WAIT: Duration = Duration::from_millis(3500);
const _: () = assert!(END_SESSION_WAIT.as_millis() <= 3500 && WHISPER_EXIT.as_millis() < 3500);
/// The ids of our Win+V entries, for the next session's first paste (`paste::spawn`).
const PASTE_ENTRIES_FILE: &str = "clipboard-entries.txt";

pub fn run() -> anyhow::Result<()> {
    let layout = oxi_platform::dirs::layout().context("no data directory")?;
    oxi_platform::dirs::ensure(&layout).context("cannot create the data directories")?;
    let _log = log::init(&layout.logs_dir);
    // An error is a refusal to start; main prints it to stderr and exits non-zero. A GUI exe
    // started at logon has no stderr: the log file is where the reason stays (t11 finding 3).
    let done = serve(&layout);
    if let Err(e) = &done {
        tracing::error!("daemon stopped by an error: {e:#}");
    }
    done
}

#[allow(
    clippy::too_many_lines,
    reason = "start-up wiring reads top to bottom, one thread after another"
)]
fn serve(layout: &oxi_core::paths::Layout) -> anyhow::Result<()> {
    let server = match ipc::bind()? {
        Start::Serve(server) => server,
        Start::Forwarded => return Ok(()),
    };
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "daemon started");
    watch::ensure_config(layout).context("cannot write the default config.toml")?;
    let (config, warnings, config_error, history_on) = load_config(&layout.config_file());
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
    if history_on {
        notify(Notice::HistoryOn);
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
    hook.set_esc_cancels(config.hotkey.esc_cancels);
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
        paste::spawn(
            paste_rx,
            inbox.clone(),
            layout.data_dir.join(PASTE_ENTRIES_FILE),
        )?,
        sound::spawn(sound_rx, inbox.clone())?,
        history::spawn(
            history_rx,
            inbox.clone(),
            layout.history_dir(),
            layout.config_file(),
            notifier(&ui_tx, &waker),
        )?,
    ];
    let pushes = Pushes::default();
    // Before any download can resume: a model `.part` untouched for 7 days is dropped.
    sweep_partials(&layout.models_dir());
    // Never joined: the IPC threads hold its sender until the process ends.
    io::spawn(io_rx, inbox.clone(), pushes.clone(), layout.models_dir())?;
    let host = Arc::new(SystemHost::new(Arc::clone(&hook)));
    let quit_host = Arc::clone(&host);

    let (watch_tx, watch_rx) = mpsc::channel();
    let watcher = watch::spawn(layout, watch_tx, notify).context("config watcher")?;
    // A parse error also goes to the settings window, which shows its line.
    let config_pushes = pushes.clone();
    forward("config-events", watch_rx, inbox.clone(), move |msg| {
        match &msg {
            Inbox::ConfigError(e) => {
                config_pushes.send(config_error_push(e));
            }
            Inbox::Config { config, .. } => sync_autostart(config.system.autostart),
            _ => {}
        }
        msg
    })?;

    let ui_config = config.clone();
    let progress = overlay.progress();
    let coordinator = {
        let workers = Workers {
            audio: audio_tx,
            whisper: whisper_tx,
            paste: paste_tx,
            sound: sound_tx,
            history: history_tx,
            ui: ui_tx,
            wake_ui: wake_ui(&waker),
            progress,
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

    // The joins run on their own thread, so Windows ending the session can wait for them too.
    let exit = Arc::new(ExitStage::default());
    let [audio, whisper, paste, sound, history] = workers;
    {
        let exit = Arc::clone(&exit);
        spawn("exit-join", move || {
            join(coordinator);
            exit.reach(COORDINATOR_DONE);
            for worker in [audio, paste, sound, history] {
                join(worker);
            }
            join_within(whisper, exit.left(WHISPER_EXIT));
            exit.reach(WORKERS_DONE);
        })
        .inspect_err(|_| {
            let _ = inbox.send(Inbox::Shutdown); // the coordinator runs: it ends and unhooks
        })?;
    }
    // The IPC listener last: after a start-up error it would stay alive while main's error box
    // waits for OK, and a second launch would be forwarded to this dead daemon and exit silently.
    if let Err(e) = ipc::spawn(server, inbox.clone(), io_tx, host, pushes, keys_rx) {
        let _ = inbox.send(Inbox::Shutdown);
        exit.wait(WORKERS_DONE);
        return Err(e.into());
    }
    {
        // Shutdown, restart or logoff: the same quit as the tray's, finished before Windows ends
        // the process, which it may do as soon as this returns.
        let (inbox, exit, host) = (inbox.clone(), Arc::clone(&exit), Arc::clone(&quit_host));
        tray.on_end_session(move || {
            tracing::info!("windows is ending the session, quitting");
            exit.end_by(Instant::now() + END_SESSION_WAIT);
            host.stop_opening_settings();
            let _ = inbox.send(Inbox::Shutdown);
            if !exit.wait_within(WORKERS_DONE, exit.left(END_SESSION_WAIT)) {
                tracing::warn!("session end: the exit did not finish in time");
            }
        });
    }

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
    // In order: the coordinator, whose end closed every worker's channel, then the tray icon
    // and the overlay (gone at once, not after the workers), then the workers and the watcher.
    exit.wait(COORDINATOR_DONE);
    drop(ui);
    drop(tray);
    exit.wait(WORKERS_DONE);
    drop(watcher);
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
/// bad reload; the watcher applies the file once it is fixed. The last value is true when the
/// load's migration turned history on: the migrated file is written back, so it is true once.
fn load_config(path: &Path) -> (Config, Vec<String>, Option<ConfigError>, bool) {
    let parsed = watch::read_with_retry(path)
        .map_err(|e| ConfigError::Invalid(e.kind().to_string()))
        .and_then(|text| ConfigFile::parse(&text));
    match parsed {
        Ok(file) => {
            if file.history_turned_on {
                save_migrated(path, &file);
            }
            (file.config, file.warnings, None, file.history_turned_on)
        }
        Err(e) => {
            tracing::warn!("config not applied, starting on the defaults: {e}");
            (Config::default(), Vec::new(), Some(e), false)
        }
    }
}

/// Writes the migrated `config.toml` back, so the migration and its notice happen once. A file
/// that cannot be written is migrated again at the next start, notice included.
fn save_migrated(path: &Path, file: &ConfigFile) {
    let written = file
        .to_toml_string()
        .map_err(std::io::Error::other)
        .and_then(|text| oxi_platform::fs::write_atomic(path, text.as_bytes()));
    match written {
        Ok(()) => tracing::info!("config.toml migrated, history turned on"),
        Err(e) => tracing::warn!("migrated config.toml not written: {e}"),
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
            tracing::info!("quit from the tray menu");
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
        self.0.set_esc_cancels(hotkey.esc_cancels);
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

const COORDINATOR_DONE: u8 = 1;
const WORKERS_DONE: u8 = 2;

/// How far the exit got; the UI thread and the end-session handler wait on it. The third field
/// is the end-session deadline, once Windows set one.
#[derive(Default)]
struct ExitStage(Mutex<u8>, Condvar, OnceLock<Instant>);

impl ExitStage {
    /// The whole exit must be done by `deadline` (the first one set stays).
    fn end_by(&self, deadline: Instant) {
        let _ = self.2.set(deadline);
    }

    /// How long the next exit step may wait: `cap`, or less once a deadline is set.
    fn left(&self, cap: Duration) -> Duration {
        self.2
            .get()
            .map_or(cap, |&deadline| left_of(cap, deadline, Instant::now()))
    }
    fn reach(&self, stage: u8) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = stage;
        self.1.notify_all();
    }

    fn wait(&self, stage: u8) {
        let at = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let at = self.1.wait_while(at, |at| *at < stage);
        drop(at.unwrap_or_else(PoisonError::into_inner));
    }

    /// [`wait`](Self::wait) for at most `limit`; true when `stage` was reached.
    fn wait_within(&self, stage: u8, limit: Duration) -> bool {
        let at = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (at, _) = self
            .1
            .wait_timeout_while(at, limit, |at| *at < stage)
            .unwrap_or_else(PoisonError::into_inner);
        *at >= stage
    }
}

/// What a step capped at `cap` may still wait at `now`; nothing once `deadline` passed.
fn left_of(cap: Duration, deadline: Instant, now: Instant) -> Duration {
    cap.min(deadline.saturating_duration_since(now))
}

/// [`join`], for at most `limit`; a thread still running then is left to the process exit.
fn join_within(handle: JoinHandle<()>, limit: Duration) {
    let name = handle.thread().name().unwrap_or("?").to_owned();
    let (done_tx, done) = mpsc::channel();
    let waiter = spawn("exit-wait", move || {
        join(handle);
        let _ = done_tx.send(());
    });
    if waiter.is_err() || done.recv_timeout(limit).is_err() {
        tracing::warn!(thread = %name, "thread still busy at exit, not waited for");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::scratch::scratch_dir;

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
        let dir = scratch_dir(&format!("oxi-run-{}", std::process::id()));
        let path = dir.join(CONFIG_FILE);

        let (config, _, error, _) = load_config(&path);
        assert_eq!(config, Config::default());
        assert!(matches!(error, Some(ConfigError::Invalid(_))), "{error:?}");

        std::fs::write(&path, "[hotkey\n").unwrap();
        let (config, _, error, _) = load_config(&path);
        assert_eq!(config, Config::default());
        assert!(
            matches!(error, Some(ConfigError::Syntax { .. })),
            "{error:?}"
        );

        std::fs::write(&path, "[hotkey]\nchord = [\"Ctrl\", \"Shift\", \"F13\"]\n").unwrap();
        let (config, _, error, _) = load_config(&path);
        assert!(error.is_none(), "{error:?}");
        assert_eq!(config.hotkey.chord, ["Ctrl", "Shift", "F13"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // t14: the v1 to v2 migration turns history on; the daemon writes the migrated file, so the
    // notice shows at one start only.
    #[test]
    fn a_migration_that_turns_history_on_is_written_and_reported_once() {
        let dir = scratch_dir(&format!("oxi-run-history-{}", std::process::id()));
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, "[history]\nenabled = false # old\n").unwrap();

        let (config, _, error, history_on) = load_config(&path);
        assert!(error.is_none(), "{error:?}");
        assert!(config.history.enabled && history_on);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("enabled = true # old"), "{saved}");

        let (config, _, _, history_on) = load_config(&path);
        assert!(config.history.enabled && !history_on, "shown once");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn label_font_loads_a_font_file_and_refuses_anything_else() {
        let dir = scratch_dir(&format!("oxi-font-{}", std::process::id()));
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
    fn the_exit_stage_wakes_its_waiters_and_a_wait_within_ends_at_its_limit() {
        let exit = Arc::new(ExitStage::default());
        assert!(!exit.wait_within(WORKERS_DONE, Duration::from_millis(10)));
        let stages = {
            let exit = Arc::clone(&exit);
            spawn("test-exit", move || {
                exit.reach(COORDINATOR_DONE);
                exit.reach(WORKERS_DONE);
            })
            .unwrap()
        };
        assert!(exit.wait_within(WORKERS_DONE, Duration::from_secs(5)));
        exit.wait(COORDINATOR_DONE); // already past: returns at once
        stages.join().unwrap();
    }

    #[test]
    fn each_exit_step_gets_only_what_is_left_of_the_end_session_deadline() {
        let start = Instant::now();
        let deadline = start + END_SESSION_WAIT;
        // Early on, a step keeps its own cap.
        assert_eq!(left_of(WHISPER_EXIT, deadline, start), WHISPER_EXIT);
        // After the 2 s grace and a slow save, the whisper join gets the rest, not its 2 s.
        let late = start + Duration::from_millis(2900);
        assert_eq!(
            left_of(WHISPER_EXIT, deadline, late),
            Duration::from_millis(600)
        );
        // Past the deadline nothing waits.
        let past = deadline + Duration::from_secs(1);
        assert_eq!(left_of(WHISPER_EXIT, deadline, past), Duration::ZERO);

        let exit = ExitStage::default();
        assert_eq!(
            exit.left(WHISPER_EXIT),
            WHISPER_EXIT,
            "no deadline: the cap"
        );
        exit.end_by(Instant::now());
        exit.end_by(Instant::now() + Duration::from_secs(60)); // the first deadline stays
        assert_eq!(exit.left(WHISPER_EXIT), Duration::ZERO);
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
