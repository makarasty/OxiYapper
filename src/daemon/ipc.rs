//! IPC server thread (ARCHITECTURE section 5).
//!
//! One thread accepts connections; each connection gets one thread of its own, so a client
//! that stops reading stalls only itself. The first message decides a connection's role:
//! `OpenSettings` is acted on and the connection closed (a second daemon launch waits for that
//! close); `Hello` makes it *the* settings connection; anything else closes it. A new `Hello`
//! replaces the old settings connection.
//!
//! Nothing outside the connection thread writes to a socket. Other threads (io progress, hook
//! keys, the watcher) hand a [`Push`] to [`Pushes::send`], which only queues it and never
//! blocks; the connection thread writes the queue with a write timeout. A full queue or a
//! failed write ends that connection. The coordinator thread is never asked to wait for a client.
//!
//! Dictated text never reaches this module; log lines carry kinds and counts only.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use oxi_core::hotkey::Key;
use oxi_core::ipc::{PROTO, Push, Request};
use oxi_platform::ipc::{Conn, Error, Server, default_name, forward_open_settings};

use super::cmd::{Inbox, IoCmd, IpcAsk, StatusReport};
use super::lock;

/// Concurrent connections; more are closed unanswered.
const MAX_CONNS: usize = 8;
/// Pushes waiting for one connection's thread.
const QUEUE: usize = 64;
/// How long a connection thread waits for a message before it looks at its queue again, so
/// this is also the delay of a push.
const POLL: Duration = Duration::from_millis(50);
/// A connection that sends no `Hello` within this long is closed.
const FIRST_MESSAGE: Duration = Duration::from_secs(5);
/// A write the client does not read within this long ends the connection.
const WRITE_LIMIT: Duration = Duration::from_secs(2);
/// How long a `Status` waits for the coordinator before it is dropped.
const ASK_LIMIT: Duration = Duration::from_secs(2);
/// `Status` asks waiting for the coordinator, per connection; more are ignored.
const MAX_ASKS: usize = 4;
/// How often capture checks that the settings window still has the focus, on a thread of its
/// own: a stalled send or a slow ask on the connection must not delay turning capture off.
const FOCUS_POLL: Duration = Duration::from_millis(20);

/// What the server needs from the OS and the rest of the daemon; fake in the tests.
pub trait Host: Send + Sync + 'static {
    /// Hotkey capture on or off; `swallow` while the settings window is foreground.
    fn set_capture(&self, on: bool, swallow: bool);
    /// Is the foreground window one of process `client`, the settings window that holds the
    /// connection (never one of this daemon's own windows, such as the tray menu).
    fn settings_foreground(&self, client: u32) -> bool;
    /// Lets the windows of process `client` take the foreground, before a `Raise`.
    fn allow_foreground(&self, client: u32) -> io::Result<()>;
    /// The `Devices` push.
    fn devices(&self) -> Push;
    /// Start a settings window.
    fn open_settings(&self) -> io::Result<()>;
}

/// Queue to the settings connection, shared by every thread that pushes. Cheap to clone.
#[derive(Clone, Default)]
pub struct Pushes(Arc<Mutex<Slot>>);

#[derive(Default)]
struct Slot {
    next_id: u64,
    current: Option<(u64, u32, SyncSender<Push>)>,
}

impl Pushes {
    /// Queues `push` for the settings connection. `false` when there is none, or when its
    /// queue is full (it is then disconnected, so a stuck window cannot grow memory).
    pub fn send(&self, push: Push) -> bool {
        let mut slot = lock(&self.0);
        let Some((_, _, tx)) = &slot.current else {
            return false;
        };
        match tx.try_send(push) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                tracing::warn!("settings window not reading: connection dropped");
                slot.current = None;
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                slot.current = None;
                false
            }
        }
    }

    /// A new settings connection; the old one, if any, ends.
    pub(super) fn claim(&self, client: u32) -> (u64, Receiver<Push>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let mut slot = lock(&self.0);
        slot.next_id += 1;
        let id = slot.next_id;
        slot.current = Some((id, client, tx));
        (id, rx)
    }

    /// Process id of the settings client, if one is connected.
    fn client(&self) -> Option<u32> {
        lock(&self.0).current.as_ref().map(|&(_, client, _)| client)
    }

    fn release(&self, id: u64) {
        let mut slot = lock(&self.0);
        if slot.current.as_ref().is_some_and(|(cur, _, _)| *cur == id) {
            slot.current = None;
        }
    }
}

/// Outcome of [`bind`].
pub enum Start {
    /// This process is the daemon: serve on it.
    Serve(Server),
    /// Another daemon runs; it was asked to open settings and this process should exit.
    Forwarded,
}

/// Binds this user's socket. An error here is a refusal to start: its text says why and
/// goes to the log and to stderr; the process must not exit silently.
pub fn bind() -> anyhow::Result<Start> {
    bind_at(&default_name().context("no IPC socket name")?)
}

fn bind_at(name: &str) -> anyhow::Result<Start> {
    match Server::bind(name) {
        Ok(server) => Ok(Start::Serve(server)),
        Err(Error::AlreadyRunning) => {
            // This launch has the user's input; without passing it on, the settings window the
            // daemon raises or starts stays behind the app in front.
            if let Err(e) = oxi_platform::os::foreground::allow_any() {
                tracing::debug!(kind = ?e.kind(), "foreground not passed on");
            }
            forward_open_settings(name).map_err(refusal)?;
            Ok(Start::Forwarded)
        }
        Err(e) => Err(refusal(e)),
    }
}

/// The plain message for an error of binding or forwarding.
fn refusal(e: Error) -> anyhow::Error {
    let msg = match e {
        Error::AlreadyRunning => "another OxiYapper is already running".to_owned(),
        Error::Busy => format!(
            "{}; end it in Task Manager and start again",
            oxi_core::strings::DAEMON_BUSY
        ),
        Error::ForeignServer => {
            "cannot start: the OxiYapper IPC pipe is held by another account or program".to_owned()
        }
        Error::Untrusted => {
            "cannot start: the OxiYapper IPC pipe is held by an untrusted (low-integrity) program"
                .to_owned()
        }
        Error::Io(e) if e.kind() == io::ErrorKind::TimedOut => {
            "OxiYapper is running but did not answer in time".to_owned()
        }
        Error::Io(e) => format!("IPC failed: {e}"),
    };
    tracing::error!("{msg}");
    anyhow::anyhow!(msg)
}

/// Starts the accept thread and the thread that turns captured keys into `Key` pushes.
/// Neither ends before the process does.
pub fn spawn(
    server: Server,
    inbox: Sender<Inbox>,
    io: Sender<IoCmd>,
    host: Arc<dyn Host>,
    pushes: Pushes,
    keys: Receiver<(Key, bool)>,
) -> io::Result<JoinHandle<()>> {
    let key_pushes = pushes.clone();
    thread::Builder::new()
        .name("ipc-keys".into())
        .spawn(move || {
            for (code, down) in keys {
                key_pushes.send(Push::Key { code, down });
            }
        })?;
    let ctx = Arc::new(Ctx {
        inbox,
        io,
        host,
        pushes,
    });
    thread::Builder::new()
        .name("ipc".into())
        .spawn(move || accept_loop(&server, &ctx))
}

fn accept_loop(server: &Server, ctx: &Arc<Ctx>) {
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        let conn = match server.accept() {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(kind = ?e.kind(), "ipc accept failed");
                thread::sleep(POLL); // an error that repeats must not spin
                continue;
            }
        };
        if live.fetch_add(1, Ordering::AcqRel) >= MAX_CONNS {
            live.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!("ipc connection refused: {MAX_CONNS} are open");
            continue; // dropping `conn` closes it
        }
        let (ctx, slot) = (ctx.clone(), Live(live.clone()));
        let spawned = thread::Builder::new()
            .name("ipc-conn".into())
            .spawn(move || {
                let _slot = slot;
                ctx.connection(conn);
            });
        if let Err(e) = spawned {
            tracing::warn!(kind = ?e.kind(), "ipc connection thread not started");
        }
    }
}

/// Counts a connection while it lives.
struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Ctx {
    inbox: Sender<Inbox>,
    io: Sender<IoCmd>,
    host: Arc<dyn Host>,
    pushes: Pushes,
}

/// What one connection has become.
#[derive(Default)]
struct Session {
    /// Set by `Hello`: this is the settings connection.
    settings: Option<(u64, Receiver<Push>)>,
    /// Process id of the settings client; 0 when unknown (capture is then refused).
    client: u32,
    /// Hotkey capture this connection switched on.
    capture: Option<FocusGuard>,
    /// `Status` asks the coordinator has not answered yet, oldest first.
    asks: VecDeque<(Receiver<StatusReport>, Instant)>,
}

/// Turns capture off the moment the settings window loses the foreground.
struct FocusGuard {
    stop: Arc<AtomicBool>,
    /// The guard turned capture off itself.
    lost: Arc<AtomicBool>,
}

impl FocusGuard {
    fn start(host: &Arc<dyn Host>, client: u32) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let (host, flags) = (host.clone(), (stop.clone(), lost.clone()));
        thread::Builder::new()
            .name("ipc-focus".into())
            .spawn(move || {
                let (stop, lost) = flags;
                loop {
                    thread::sleep(FOCUS_POLL);
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    if !host.settings_foreground(client) {
                        host.set_capture(false, false);
                        lost.store(true, Ordering::Release);
                        return;
                    }
                }
            })?;
        Ok(Self { stop, lost })
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Ctx {
    fn connection(&self, mut conn: Conn) {
        let mut session = Session::default();
        if conn.set_write_timeout(Some(WRITE_LIMIT)).is_ok()
            && conn.set_read_timeout(Some(POLL)).is_ok()
        {
            self.serve(&mut conn, &mut session);
        }
        self.end_capture(&mut session);
        if let Some((id, _)) = session.settings {
            self.pushes.release(id);
        }
    }

    fn serve(&self, conn: &mut Conn, session: &mut Session) {
        let opened = Instant::now();
        loop {
            if !flush(conn, session) {
                return;
            }
            if session
                .capture
                .as_ref()
                .is_some_and(|g| g.lost.load(Ordering::Acquire))
            {
                session.capture = None;
            }
            if !answer_status(conn, session) {
                return;
            }
            match conn.recv::<Request>() {
                // A client that connects and closes (the probe of `Server::bind`) reads as `None`.
                Ok(None) => return,
                Ok(Some(req)) => {
                    if !self.handle(req, conn, session) {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    if session.settings.is_none() && opened.elapsed() > FIRST_MESSAGE {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!(kind = ?e.kind(), "ipc connection ended");
                    return;
                }
            }
        }
    }

    /// Acts on `req`; `false` closes the connection.
    fn handle(&self, req: Request, conn: &mut Conn, session: &mut Session) -> bool {
        let first = session.settings.is_none();
        match req {
            Request::OpenSettings => {
                // Windows only lets a window take the foreground when the process allows it.
                if let Some(client) = self.pushes.client()
                    && let Err(e) = self.host.allow_foreground(client)
                {
                    tracing::debug!(kind = ?e.kind(), "foreground not allowed for settings");
                }
                if !self.pushes.send(Push::Raise)
                    && let Err(e) = self.host.open_settings()
                {
                    tracing::warn!(kind = ?e.kind(), "settings window not started");
                }
                if first {
                    // The second launch waits for this ack and the close after it; a close
                    // without the ack (no free slot) tells it nothing was acted on.
                    let _ = conn.send(&hello_ack());
                }
                !first
            }
            Request::Hello { .. } => {
                session.client = conn.peer_pid().unwrap_or(0);
                if session.settings.is_none() {
                    session.settings = Some(self.pushes.claim(session.client));
                    // A window that opens during a download shows it (no second DownloadModel).
                    let _ = self.io.send(IoCmd::Subscribed);
                }
                conn.send(&hello_ack()).is_ok()
            }
            // Only the settings connection may ask for anything else.
            _ if first => false,
            Request::Status => self.status(session),
            Request::ListDevices => conn.send(&self.host.devices()).is_ok(),
            Request::DownloadModel { id } => self.io.send(IoCmd::Download { id }).is_ok(),
            Request::CancelDownload { id } => self.io.send(IoCmd::CancelDownload { id }).is_ok(),
            Request::CaptureHotkey { on } => {
                self.capture(on, session);
                true
            }
            Request::PreviewOverlay { position } => self
                .inbox
                .send(Inbox::Ipc(IpcAsk::PreviewOverlay(position)))
                .is_ok(),
            Request::Shutdown => {
                let _ = self.inbox.send(Inbox::Shutdown);
                false
            }
        }
    }

    /// Asks the coordinator; the answer is sent by [`answer_status`], so a busy
    /// coordinator never holds up this connection's loop.
    fn status(&self, session: &mut Session) -> bool {
        if session.asks.len() >= MAX_ASKS {
            return true;
        }
        let (tx, rx) = mpsc::channel();
        session.asks.push_back((rx, Instant::now()));
        self.inbox.send(Inbox::Ipc(IpcAsk::Status(tx))).is_ok()
    }

    /// Capture starts only while the settings window is foreground and ends the moment it is not.
    fn capture(&self, on: bool, session: &mut Session) {
        if !on {
            self.end_capture(session);
        } else if session.capture.is_none()
            && session.client != 0
            && self.host.settings_foreground(session.client)
        {
            // On first, guard second: a guard that ran before the `on` could turn capture off and
            // then be overtaken by it, leaving every key swallowed with nobody watching.
            self.host.set_capture(true, true);
            match FocusGuard::start(&self.host, session.client) {
                Ok(guard) => session.capture = Some(guard),
                Err(e) => {
                    tracing::warn!(kind = ?e.kind(), "capture not started");
                    self.host.set_capture(false, false);
                }
            }
        }
    }

    fn end_capture(&self, session: &mut Session) {
        if let Some(guard) = session.capture.take() {
            guard.stop();
            // Always off, even when the guard already did it: off is idempotent, a key
            // swallowed for ever is not.
            self.host.set_capture(false, false);
        }
    }
}

/// Sends the answers that came in; drops asks the coordinator left unanswered too long.
fn answer_status(conn: &mut Conn, session: &mut Session) -> bool {
    while let Some((rx, asked)) = session.asks.front() {
        match rx.try_recv() {
            Ok(report) => {
                session.asks.pop_front();
                if conn.send(&status_push(report)).is_err() {
                    return false;
                }
            }
            Err(TryRecvError::Empty) if asked.elapsed() < ASK_LIMIT => break,
            Err(_) => {
                tracing::warn!("coordinator did not answer Status");
                session.asks.pop_front();
            }
        }
    }
    true
}

/// Writes what other threads queued; `false` when the connection is over.
fn flush(conn: &mut Conn, session: &Session) -> bool {
    let Some((_, queue)) = &session.settings else {
        return true;
    };
    loop {
        match queue.try_recv() {
            Ok(push) => {
                if conn.send(&push).is_err() {
                    return false;
                }
            }
            Err(TryRecvError::Empty) => return true,
            // Replaced by a newer window, or dropped for not reading.
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

fn hello_ack() -> Push {
    Push::HelloAck {
        proto: PROTO,
        version: crate::VERSION.into(),
    }
}

fn status_push(report: StatusReport) -> Push {
    Push::Status {
        phase: report.phase,
        model: report.model,
        device: report.gpu,
        backend: report.backend,
        warnings: report.warnings,
    }
}

/// The foreground window belongs to the settings client, and that client is not this daemon
/// (its tray menu is a foreground window of ours).
fn is_settings(foreground: u32, client: u32, own: u32) -> bool {
    foreground != 0 && foreground == client && foreground != own
}

/// The settings process this daemon started.
#[derive(Default)]
struct SettingsProc {
    /// Kept so a window that is still starting is not started twice.
    child: Mutex<Option<std::process::Child>>,
    /// Set once the daemon is going down: no new window may outlive it.
    closing: AtomicBool,
}

impl SettingsProc {
    /// `start` runs unless a window is still starting or the daemon is going down.
    fn open(&self, start: impl FnOnce() -> io::Result<std::process::Child>) -> io::Result<()> {
        let mut child = lock(&self.child);
        if self.closing.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(running) = child.as_mut()
            && running.try_wait()?.is_none()
        {
            return Ok(()); // still starting; it will connect
        }
        *child = Some(start()?);
        Ok(())
    }

    /// From now on `open` does nothing; returns the running child, if any.
    fn stop_opening(&self) -> Option<std::process::Child> {
        // Under the lock, so an `open` that is running finishes first and its child is taken.
        let mut child = lock(&self.child);
        self.closing.store(true, Ordering::SeqCst);
        child.take()
    }
}

/// How long a window gets to close itself and save before the process is ended.
const CLOSE_WAIT: Duration = Duration::from_millis(1500);

/// Asks `child` to close its windows (`WM_CLOSE`, so the page saves a pending edit), waits up
/// to [`CLOSE_WAIT`], then ends it. A child with no window yet is ended at once.
fn end_child(child: &mut std::process::Child) {
    if close::post_close(child.id()) > 0 {
        let deadline = Instant::now() + CLOSE_WAIT;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `WM_CLOSE` to the top-level windows of a process.
mod close {
    use core::ffi::c_void;

    type Hwnd = *mut c_void;
    const WM_CLOSE: u32 = 0x0010;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(f: unsafe extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
    }

    struct Query {
        pid: u32,
        posted: usize,
    }

    unsafe extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: `lparam` is the `&mut Query` that `post_close` passed and keeps alive.
        let query = unsafe { &mut *(lparam as *mut Query) };
        let mut pid = 0;
        // SAFETY: `hwnd` comes from EnumWindows; `pid` is a valid out-pointer.
        unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
        // SAFETY: a posted message needs only the handle EnumWindows gave.
        if pid == query.pid && unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) } != 0 {
            query.posted += 1;
        }
        1
    }

    /// How many windows of `pid` were asked to close.
    pub fn post_close(pid: u32) -> usize {
        let mut query = Query { pid, posted: 0 };
        // SAFETY: `visit` only uses `query` during this call, which it outlives.
        unsafe { EnumWindows(visit, (&raw mut query) as isize) };
        query.posted
    }
}

/// The real host: the hook handle, the foreground window, `current_exe settings`.
pub struct SystemHost {
    hook: Arc<oxi_platform::os::hook::HookHandle>,
    settings: SettingsProc,
}

impl SystemHost {
    pub fn new(hook: Arc<oxi_platform::os::hook::HookHandle>) -> Self {
        Self {
            hook,
            settings: SettingsProc::default(),
        }
    }

    /// The daemon is going down: a later `OpenSettings` starts nothing. An open settings window
    /// stays (the Stop button's way: it shows the daemon offline and Start).
    pub fn stop_opening_settings(&self) {
        drop(self.settings.stop_opening());
    }

    /// The tray's Quit ends the whole app: no new settings window, and the one this daemon
    /// started closes. The lock is not held while it does.
    pub fn close_settings(&self) {
        if let Some(mut child) = self.settings.stop_opening() {
            end_child(&mut child);
        }
    }
}

impl Host for SystemHost {
    fn set_capture(&self, on: bool, swallow: bool) {
        self.hook.set_capture(on, swallow);
    }

    fn settings_foreground(&self, client: u32) -> bool {
        is_settings(
            oxi_platform::os::pipe::foreground_pid(),
            client,
            std::process::id(),
        )
    }

    fn allow_foreground(&self, client: u32) -> io::Result<()> {
        oxi_platform::os::pipe::allow_foreground(client)
    }

    fn devices(&self) -> Push {
        let mics = oxi_platform::capture::list_input_devices().unwrap_or_else(|e| {
            tracing::warn!("cannot list microphones: {e}");
            Vec::new()
        });
        Push::Devices {
            mics,
            gpus: oxi_whisper::list_devices(),
        }
    }

    fn open_settings(&self) -> io::Result<()> {
        self.settings.open(|| {
            let started = std::process::Command::new(std::env::current_exe()?)
                .arg("settings")
                .spawn()?;
            // Its first window comes to the front only with this permission.
            if let Err(e) = self.allow_foreground(started.id()) {
                tracing::debug!(kind = ?e.kind(), "foreground not allowed for settings");
            }
            Ok(started)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use oxi_core::config::OverlayPosition;
    use oxi_core::device::DeviceChoice;
    use oxi_core::ipc::MAX_LINE;
    use oxi_core::machine::Phase;

    use super::*;

    const WAIT: Duration = Duration::from_secs(3);

    #[derive(Debug, PartialEq)]
    enum Call {
        Capture(bool, bool),
        Open,
        Allow(u32),
    }

    struct Fake {
        foreground: AtomicBool,
        calls: Sender<Call>,
        /// What the hook would be doing now: keys swallowed.
        capturing: AtomicBool,
        /// Milliseconds `set_capture(true, ..)` takes, as when its thread is preempted.
        on_delay: AtomicU64,
    }

    impl Host for Fake {
        fn set_capture(&self, on: bool, swallow: bool) {
            if on {
                thread::sleep(Duration::from_millis(self.on_delay.load(Ordering::Relaxed)));
            }
            self.capturing.store(on, Ordering::SeqCst);
            let _ = self.calls.send(Call::Capture(on, swallow));
        }
        fn settings_foreground(&self, _client: u32) -> bool {
            self.foreground.load(Ordering::Relaxed)
        }
        fn allow_foreground(&self, client: u32) -> io::Result<()> {
            let _ = self.calls.send(Call::Allow(client));
            Ok(())
        }
        fn devices(&self) -> Push {
            Push::Devices {
                mics: vec!["mic".into()],
                gpus: vec![],
            }
        }
        fn open_settings(&self) -> io::Result<()> {
            let _ = self.calls.send(Call::Open);
            Ok(())
        }
    }

    /// A one-process child (`ping` starts no grandchild), so nothing is left behind.
    fn sleeper() -> std::process::Child {
        std::process::Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn end_child_leaves_no_process_behind() {
        let mut child = sleeper();
        end_child(&mut child); // no window: ended at once
        assert!(child.try_wait().unwrap().is_some());
    }

    #[test]
    fn end_child_lets_a_window_close_itself() {
        // A Forms window exits with 0 on WM_CLOSE; a killed process exits with 1.
        let script = "Add-Type -AssemblyName System.Windows.Forms; \
                      [Windows.Forms.Application]::Run((New-Object Windows.Forms.Form))";
        let mut child = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .spawn()
            .unwrap();
        let up = Instant::now() + Duration::from_secs(30);
        while !has_window(child.id()) {
            assert!(Instant::now() < up, "the test window never opened");
            thread::sleep(Duration::from_millis(100));
        }
        end_child(&mut child);
        assert_eq!(child.try_wait().unwrap().and_then(|s| s.code()), Some(0));
    }

    /// Does `pid` own a window yet? Asked of Windows through another process, since
    /// `post_close` would close it.
    fn has_window(pid: u32) -> bool {
        let out = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!("(Get-Process -Id {pid}).MainWindowHandle -ne 0"),
            ])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim() == "True"
    }

    #[test]
    fn settings_proc_opens_once_and_never_after_closing() {
        let settings = SettingsProc::default();
        let started = std::cell::Cell::new(0);
        let start = || {
            started.set(started.get() + 1);
            Ok(sleeper())
        };
        settings.open(start).unwrap();
        settings.open(start).unwrap(); // still running: not started twice
        assert_eq!(started.get(), 1);
        let mut child = settings.stop_opening().unwrap();
        end_child(&mut child);
        settings.open(start).unwrap();
        assert_eq!(started.get(), 1, "opened after the daemon went down");
    }

    /// A line that is no `Request`, written raw.
    struct Raw(String);

    impl oxi_platform::ipc::Message for Raw {
        fn encode(&self) -> String {
            self.0.clone()
        }
        fn decode(
            _: &mut oxi_core::ipc::Decoder,
        ) -> Option<Result<Self, oxi_core::ipc::DecodeError>> {
            None
        }
    }

    struct Rig {
        name: String,
        host: Arc<Fake>,
        pushes: Pushes,
        inbox: Receiver<Inbox>,
        io: Receiver<IoCmd>,
        calls: Receiver<Call>,
        keys: Sender<(Key, bool)>,
    }

    fn rig(tag: &str) -> Rig {
        // The daemon is Windows-only: the name is always a pipe.
        let name = oxi_core::ipc::pipe_name(&format!("test-{}-ipc-{tag}", std::process::id()));
        let Ok(Start::Serve(server)) = bind_at(&name) else {
            panic!("bind failed");
        };
        let (inbox_tx, inbox) = mpsc::channel();
        let (io_tx, io) = mpsc::channel();
        let (calls_tx, calls) = mpsc::channel();
        let (keys, keys_rx) = mpsc::channel();
        let host = Arc::new(Fake {
            foreground: AtomicBool::new(true),
            calls: calls_tx,
            capturing: AtomicBool::new(false),
            on_delay: AtomicU64::new(0),
        });
        let pushes = Pushes::default();
        spawn(
            server,
            inbox_tx,
            io_tx,
            host.clone(),
            pushes.clone(),
            keys_rx,
        )
        .unwrap();
        Rig {
            name,
            host,
            pushes,
            inbox,
            io,
            calls,
            keys,
        }
    }

    fn connect(rig: &Rig) -> Conn {
        let mut conn = Conn::connect(&rig.name).unwrap();
        conn.set_read_timeout(Some(WAIT)).unwrap();
        conn
    }

    /// A connection that said `Hello` and got its ack.
    fn settings(rig: &Rig) -> Conn {
        let mut conn = connect(rig);
        conn.send(&Request::Hello {
            proto: PROTO,
            version: "test".into(),
        })
        .unwrap();
        assert!(matches!(
            conn.recv::<Push>().unwrap(),
            Some(Push::HelloAck { proto: PROTO, .. })
        ));
        conn
    }

    #[test]
    fn status_and_devices_are_answered() {
        let rig = rig("status");
        let mut conn = settings(&rig);
        let report = StatusReport {
            phase: Phase::Recording,
            model: "tiny".into(),
            device: "mic".into(),
            gpu: DeviceChoice::Named("Vulkan0".into()),
            backend: "cpu".into(),
            warnings: vec!["w".into()],
        };
        conn.send(&Request::Status).unwrap();
        let Ok(Inbox::Ipc(IpcAsk::Status(reply))) = rig.inbox.recv_timeout(WAIT) else {
            panic!("no Status ask");
        };
        reply.send(report).unwrap();
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::Status {
                phase: Phase::Recording,
                model: "tiny".into(),
                device: DeviceChoice::Named("Vulkan0".into()),
                backend: "cpu".into(),
                warnings: vec!["w".into()],
            })
        );
        conn.send(&Request::ListDevices).unwrap();
        assert_eq!(conn.recv::<Push>().unwrap(), Some(rig.host.devices()));
    }

    #[test]
    fn second_launch_opens_settings_and_is_closed() {
        let rig = rig("second");
        // `bind_at` on a served name forwards and returns once the daemon closed the connection.
        assert!(matches!(bind_at(&rig.name), Ok(Start::Forwarded)));
        assert_eq!(rig.calls.recv_timeout(WAIT).unwrap(), Call::Open);
    }

    #[test]
    fn open_settings_raises_a_connected_window() {
        let rig = rig("raise");
        let mut window = settings(&rig);
        forward_open_settings(&rig.name).unwrap();
        assert_eq!(window.recv::<Push>().unwrap(), Some(Push::Raise));
        // The window may take the focus: allowed for the client's own pid (here this process).
        assert_eq!(rig.calls.try_recv(), Ok(Call::Allow(std::process::id())));
        assert!(rig.calls.try_recv().is_err(), "no second window");
    }

    #[test]
    fn empty_and_silent_connections_are_survived() {
        let rig = rig("empty");
        drop(Conn::connect(&rig.name).unwrap());
        let _idle = connect(&rig); // sends nothing
        let mut conn = settings(&rig);
        conn.send(&Request::PreviewOverlay {
            position: OverlayPosition::TopLeft,
        })
        .unwrap();
        assert!(matches!(
            rig.inbox.recv_timeout(WAIT),
            Ok(Inbox::Ipc(IpcAsk::PreviewOverlay(OverlayPosition::TopLeft)))
        ));
    }

    #[test]
    fn only_hello_or_open_settings_may_start_a_connection() {
        let rig = rig("stranger");
        let mut conn = connect(&rig);
        conn.send(&Request::Shutdown).unwrap();
        assert!(conn.recv::<Push>().unwrap().is_none());
        assert!(matches!(rig.inbox.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn oversized_line_closes_only_that_connection() {
        let rig = rig("oversize");
        let mut good = settings(&rig);
        let mut bad = connect(&rig);
        let mut line = "x".repeat(MAX_LINE + 2);
        line.push('\n');
        bad.send(&Raw(line)).unwrap();
        assert!(bad.recv::<Push>().map_or(true, |m| m.is_none()));
        good.send(&Request::ListDevices).unwrap();
        assert!(matches!(
            good.recv::<Push>().unwrap(),
            Some(Push::Devices { .. })
        ));
    }

    #[test]
    fn requests_reach_their_owners() {
        let rig = rig("route");
        let mut conn = settings(&rig);
        assert_eq!(rig.io.recv_timeout(WAIT).unwrap(), IoCmd::Subscribed);
        // A second Hello on the same connection subscribes nothing new.
        conn.send(&Request::Hello {
            proto: PROTO,
            version: "test".into(),
        })
        .unwrap();
        assert!(matches!(
            conn.recv::<Push>().unwrap(),
            Some(Push::HelloAck { .. })
        ));
        conn.send(&Request::DownloadModel { id: "tiny".into() })
            .unwrap();
        conn.send(&Request::CancelDownload { id: "tiny".into() })
            .unwrap();
        assert_eq!(
            rig.io.recv_timeout(WAIT).unwrap(),
            IoCmd::Download { id: "tiny".into() }
        );
        assert_eq!(
            rig.io.recv_timeout(WAIT).unwrap(),
            IoCmd::CancelDownload { id: "tiny".into() }
        );
        conn.send(&Request::Shutdown).unwrap();
        assert!(matches!(rig.inbox.recv_timeout(WAIT), Ok(Inbox::Shutdown)));
        assert!(conn.recv::<Push>().unwrap().is_none());
    }

    #[test]
    fn dropping_the_connection_ends_capture() {
        let rig = rig("drop");
        let mut conn = settings(&rig);
        conn.send(&Request::CaptureHotkey { on: true }).unwrap();
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(true, true)
        );
        drop(conn);
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(false, false)
        );
    }

    #[test]
    fn a_reconnected_window_can_capture_again() {
        let rig = rig("recapture");
        for _ in 0..2 {
            let mut conn = settings(&rig);
            conn.send(&Request::CaptureHotkey { on: true }).unwrap();
            assert_eq!(
                rig.calls.recv_timeout(WAIT).unwrap(),
                Call::Capture(true, true)
            );
            drop(conn);
            assert_eq!(
                rig.calls.recv_timeout(WAIT).unwrap(),
                Call::Capture(false, false)
            );
        }
    }

    #[test]
    fn losing_focus_ends_capture_and_blocks_a_start() {
        let rig = rig("focus");
        let mut conn = settings(&rig);
        conn.send(&Request::CaptureHotkey { on: true }).unwrap();
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(true, true)
        );
        rig.host.foreground.store(false, Ordering::Relaxed);
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(false, false)
        );
        conn.send(&Request::CaptureHotkey { on: true }).unwrap();
        conn.send(&Request::ListDevices).unwrap();
        assert!(matches!(
            conn.recv::<Push>().unwrap(),
            Some(Push::Devices { .. })
        ));
        assert!(
            rig.calls.try_recv().is_err(),
            "capture must not start unfocused"
        );
    }

    #[test]
    fn captured_keys_are_pushed() {
        let rig = rig("keys");
        let mut conn = settings(&rig);
        rig.keys.send((Key::F13, true)).unwrap();
        rig.keys.send((Key::F13, false)).unwrap();
        for down in [true, false] {
            assert_eq!(
                conn.recv::<Push>().unwrap(),
                Some(Push::Key {
                    code: Key::F13,
                    down
                })
            );
        }
    }

    #[test]
    fn connections_are_capped() {
        let rig = rig("cap");
        let idle: Vec<Conn> = (0..MAX_CONNS).map(|_| connect(&rig)).collect();
        let mut extra = connect(&rig);
        // A refused connection is closed without reading it.
        let hello = Request::Hello {
            proto: PROTO,
            version: "test".into(),
        };
        let _ = extra.send(&hello);
        assert!(extra.recv::<Push>().map_or(true, |m| m.is_none()));
        // The second launch of a full daemon must not read that as "settings opened".
        assert!(matches!(forward_open_settings(&rig.name), Err(Error::Busy)));
        drop(idle);
        // Slots free up as the threads see the closes.
        let deadline = Instant::now() + WAIT;
        loop {
            let mut conn = connect(&rig);
            if conn.send(&hello).is_ok()
                && matches!(conn.recv::<Push>(), Ok(Some(Push::HelloAck { .. })))
            {
                break;
            }
            assert!(Instant::now() < deadline, "no slot freed");
        }
    }

    #[test]
    fn a_window_that_stops_reading_never_blocks_the_sender() {
        let rig = rig("stall");
        let _stuck = settings(&rig); // never reads again
        let big = Push::Status {
            phase: Phase::Idle,
            model: "tiny".into(),
            device: DeviceChoice::Auto,
            backend: "cpu".into(),
            warnings: vec!["w".repeat(64 * 1024)],
        };
        let start = Instant::now();
        let mut refused = false;
        for _ in 0..4000 {
            refused |= !rig.pushes.send(big.clone());
        }
        assert!(start.elapsed() < Duration::from_secs(1), "send blocked");
        assert!(refused, "the queue never filled");
    }

    #[test]
    fn newer_window_replaces_the_old_one() {
        let rig = rig("replace");
        let mut old = settings(&rig);
        let mut new = settings(&rig);
        assert!(old.recv::<Push>().map_or(true, |m| m.is_none()));
        assert!(rig.pushes.send(Push::Raise));
        assert_eq!(new.recv::<Push>().unwrap(), Some(Push::Raise));
    }

    #[test]
    fn an_unanswered_status_does_not_hold_up_the_focus_check() {
        let rig = rig("slowask");
        let mut conn = settings(&rig);
        conn.send(&Request::CaptureHotkey { on: true }).unwrap();
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(true, true)
        );
        // The coordinator never answers.
        conn.send(&Request::Status).unwrap();
        assert!(matches!(
            rig.inbox.recv_timeout(WAIT),
            Ok(Inbox::Ipc(IpcAsk::Status(_)))
        ));
        let lost = Instant::now();
        rig.host.foreground.store(false, Ordering::Relaxed);
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(false, false)
        );
        assert!(
            lost.elapsed() < Duration::from_millis(500),
            "{:?}",
            lost.elapsed()
        );
    }

    /// Waits until the fake hook stops swallowing.
    fn capture_ends(rig: &Rig) -> bool {
        let deadline = Instant::now() + WAIT;
        while rig.host.capturing.load(Ordering::SeqCst) {
            if Instant::now() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    #[test]
    fn focus_lost_while_capture_turns_on_still_ends_capture() {
        let rig = rig("slowon");
        // The thread that turns capture on is held up (preempted) for longer than a focus poll.
        rig.host.on_delay.store(150, Ordering::Relaxed);
        let mut conn = settings(&rig);
        conn.send(&Request::CaptureHotkey { on: true }).unwrap();
        thread::sleep(Duration::from_millis(30));
        rig.host.foreground.store(false, Ordering::Relaxed);
        thread::sleep(Duration::from_millis(250));
        assert!(capture_ends(&rig), "keys still swallowed after focus loss");
        drop(conn);
        assert!(capture_ends(&rig), "keys still swallowed after disconnect");
    }

    #[test]
    fn capture_is_off_once_the_connection_is_gone() {
        #[derive(Clone, Copy, Debug)]
        enum Op {
            On,
            Off,
            FocusLost,
            FocusBack,
        }
        use Op::{FocusBack, FocusLost, Off, On};
        let sequences: [&[Op]; 7] = [
            &[On],
            &[On, FocusLost],
            &[On, FocusLost, FocusBack, On],
            &[FocusLost, On, FocusBack, On],
            &[On, Off, On],
            &[On, FocusLost, Off, FocusBack, On, Off],
            &[On, On, FocusLost, FocusBack, FocusLost],
        ];
        for (i, ops) in sequences.into_iter().enumerate() {
            let rig = rig(&format!("inv{i}"));
            rig.host.on_delay.store(25, Ordering::Relaxed);
            let mut conn = settings(&rig);
            for op in ops {
                match op {
                    On => conn.send(&Request::CaptureHotkey { on: true }).unwrap(),
                    Off => conn.send(&Request::CaptureHotkey { on: false }).unwrap(),
                    FocusLost => rig.host.foreground.store(false, Ordering::Relaxed),
                    FocusBack => rig.host.foreground.store(true, Ordering::Relaxed),
                }
                thread::sleep(Duration::from_millis(30));
            }
            drop(conn);
            assert!(capture_ends(&rig), "keys swallowed after {ops:?}");
        }
    }

    #[test]
    fn a_late_status_answer_still_arrives() {
        let rig = rig("lateask");
        let mut conn = settings(&rig);
        conn.send(&Request::Status).unwrap();
        let Ok(Inbox::Ipc(IpcAsk::Status(reply))) = rig.inbox.recv_timeout(WAIT) else {
            panic!("no Status ask");
        };
        thread::sleep(Duration::from_millis(300));
        reply
            .send(StatusReport {
                phase: Phase::Idle,
                model: "tiny".into(),
                device: String::new(),
                gpu: DeviceChoice::Auto,
                backend: "none".into(),
                warnings: vec![],
            })
            .unwrap();
        assert!(matches!(
            conn.recv::<Push>().unwrap(),
            Some(Push::Status { .. })
        ));
    }

    #[test]
    fn only_the_settings_client_counts_as_foreground() {
        // (foreground, client, own, expected)
        let cases = [
            (100, 100, 1, true),
            (1, 100, 1, false),   // the daemon's own window, such as the tray menu
            (1, 1, 1, false),     // a client that is the daemon itself
            (200, 100, 1, false), // another app
            (0, 0, 1, false),     // no foreground window, no known client
        ];
        for (fg, client, own, want) in cases {
            assert_eq!(is_settings(fg, client, own), want, "{fg} {client} {own}");
        }
    }

    #[test]
    fn refusals_say_why() {
        let cases = [
            (Error::Busy, "not answering"),
            (Error::ForeignServer, "another account"),
            (Error::Untrusted, "untrusted"),
            (Error::AlreadyRunning, "already running"),
            (
                Error::Io(io::ErrorKind::TimedOut.into()),
                "did not answer in time",
            ),
        ];
        for (error, text) in cases {
            let msg = refusal(error).to_string();
            assert!(msg.contains(text), "{msg:?} lacks {text:?}");
        }
    }
}
