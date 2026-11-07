//! Link to the daemon (ARCHITECTURE section 5).
//!
//! One worker thread owns the connection: it connects, says `Hello`, writes what the pages
//! `send` and reads pushes. Both go through channels, so the UI thread never touches the pipe.
//! Pushes reach [`Ipc`] subscribers on the UI thread through `slint::invoke_from_event_loop`.
//! With no daemon the window keeps working from the config file and shows [`Link::Offline`].

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::{Rc, Weak};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::ipc::{PROTO, Push, Request};
use oxi_core::strings;
use oxi_platform::ipc::{Client, Error, default_name};

/// How long the worker waits on the pipe before it looks at the commands again. Pushes wake
/// it at once; only a request from a page can wait this long (the first one after idle), and
/// each wait costs a watchdog thread in `Conn::recv` on Windows.
// shortcut: `Conn` cannot be split into a reader and a writer, so a timed-out `recv` is
// retried instead of dropping the connection; split it in oxi-platform if the latency matters.
const POLL: Duration = Duration::from_millis(250);
/// `Hello` write and reply, and the wait for the old daemon to close on a restart.
const REPLY_WAIT: Duration = Duration::from_secs(2);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// Pause between probes of a pipe that should be going away.
const PROBE_PAUSE: Duration = Duration::from_millis(50);
/// Retry interval while a daemon we just started comes up.
const STARTUP_RETRY: Duration = Duration::from_millis(100);

/// What the window knows about the daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Link {
    Connecting,
    Online,
    /// No daemon answers; offer Start.
    #[default]
    Offline,
    /// The daemon speaks another protocol version (exe updated, old daemon still running).
    Mismatch {
        daemon_proto: u32,
    },
    /// The pipe exists but nothing answers in time.
    Busy,
    /// A restart closed the old daemon's connection but its pipe never freed up.
    Stuck,
    /// A low-integrity program holds the pipe. Nothing is sent to it.
    Untrusted,
    /// Another account holds the pipe. Nothing is sent to it.
    Foreign,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Link(Link),
    Push(Push),
}

/// What a page or the banner asks of the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// Dropped while there is no connection or the protocol differs.
    Send(Request),
    /// Spawn the daemon and reconnect.
    Start,
    /// Send `Shutdown`, wait for the old daemon to close, then spawn a new one.
    Restart,
}

/// A banner text and the button that goes with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub action: Option<Action>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// "Start": [`Ipc::start_daemon`].
    Start,
    /// "Restart": [`Ipc::restart_daemon`].
    Restart,
}

/// The banner an event calls for; `None` leaves the banner as it is.
pub fn notice(event: &Event) -> Option<Notice> {
    let (text, action) = match event {
        Event::Link(Link::Offline) => (strings::DAEMON_OFFLINE.to_owned(), Some(Action::Start)),
        Event::Link(Link::Mismatch { .. }) => {
            (strings::DAEMON_MISMATCH.to_owned(), Some(Action::Restart))
        }
        Event::Link(Link::Busy) => (strings::DAEMON_BUSY.to_owned(), None),
        Event::Link(Link::Stuck) => (strings::DAEMON_STUCK.to_owned(), Some(Action::Start)),
        Event::Link(Link::Untrusted) => (strings::PIPE_UNTRUSTED.to_owned(), None),
        Event::Link(Link::Foreign) => (strings::PIPE_FOREIGN.to_owned(), None),
        Event::Push(Push::ConfigError {
            file,
            line,
            message,
        }) => (strings::config_error(file, *line, message), None),
        _ => return None,
    };
    Some(Notice { text, action })
}

pub struct Options {
    /// Pipe or socket name.
    pub name: String,
    /// Starts a detached daemon. Called when nothing answers at first and on Start.
    pub spawn: Box<dyn Fn() -> io::Result<()> + Send>,
    /// How long to keep retrying after `spawn` before reporting [`Link::Offline`].
    pub grace: Duration,
    /// How long a restart (or Start while [`Link::Stuck`]) waits for the old daemon's pipe to
    /// go away before it gives up; also how often a stuck link looks again.
    pub exit_wait: Duration,
}

impl Options {
    /// This user's daemon: its pipe, started from this executable.
    pub fn daemon() -> io::Result<Self> {
        Ok(Self {
            name: default_name()?,
            spawn: Box::new(spawn_daemon),
            grace: Duration::from_secs(2),
            exit_wait: Duration::from_secs(3),
        })
    }
}

/// `current_exe` with no argument is the daemon.
fn spawn_daemon() -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW);
    }
    cmd.spawn().map(drop)
}

/// Starts the worker. `post` runs on the worker thread for every event; the worker ends when
/// the last clone of the returned sender is dropped.
pub fn spawn_worker(
    options: Options,
    post: impl Fn(Event) + Send + 'static,
) -> io::Result<Sender<Cmd>> {
    let (tx, rx) = mpsc::channel();
    let out = Out {
        post,
        link: Cell::new(None),
    };
    thread::Builder::new()
        .name("settings-ipc".into())
        .spawn(move || run(&options, &rx, &out))?;
    Ok(tx)
}

/// Posts a link event only when the link changed.
struct Out<F> {
    post: F,
    link: Cell<Option<Link>>,
}

impl<F: Fn(Event)> Out<F> {
    fn set(&self, link: Link) {
        if self.link.replace(Some(link)) != Some(link) {
            (self.post)(Event::Link(link));
        }
    }
}

fn run(o: &Options, rx: &Receiver<Cmd>, out: &Out<impl Fn(Event)>) {
    let mut backoff = MIN_BACKOFF;
    let mut grace: Option<Instant> = None;
    let mut launched = false;
    let mut stuck = false;
    out.set(Link::Connecting);
    loop {
        if stuck {
            // Keep the note until the name frees or the user acts; no probing of the old
            // daemon, which would replace it with Busy or Mismatch.
            let start = match wait(rx, o.exit_wait) {
                Waited::Stopped => return,
                Waited::Start => true,
                Waited::Elapsed => false,
            };
            match pipe_freed(&o.name, rx, o.exit_wait) {
                Probe::Stopped => return,
                Probe::Held => {}
                Probe::Free => {
                    stuck = false;
                    if start {
                        launch(o, &mut grace);
                        out.set(Link::Connecting);
                    } else {
                        out.set(Link::Offline);
                    }
                }
            }
            continue;
        }
        match open(&o.name) {
            Ok((mut conn, daemon_proto)) => {
                backoff = MIN_BACKOFF;
                // From now on a missing daemon was quit on purpose: show Start, never relaunch.
                launched = true;
                let mismatch = daemon_proto != PROTO;
                out.set(if mismatch {
                    Link::Mismatch { daemon_proto }
                } else {
                    Link::Online
                });
                match serve(&mut conn, rx, &out.post, mismatch) {
                    Served::Stopped => return,
                    Served::Restart => {
                        out.set(Link::Connecting);
                        // A new daemon started while the old one still holds the name would
                        // find it, forward OpenSettings to it and exit.
                        match pipe_freed(&o.name, rx, o.exit_wait) {
                            Probe::Stopped => return,
                            Probe::Free => launch(o, &mut grace),
                            Probe::Held => {
                                stuck = true;
                                out.set(Link::Stuck);
                                continue;
                            }
                        }
                    }
                    Served::Dropped => out.set(Link::Offline),
                }
            }
            Err(e) => {
                let mut link = link_of(&e);
                // A missing daemon is started once; a dropped one may have been quit on purpose.
                if link == Link::Offline && !launched {
                    launched = true;
                    launch(o, &mut grace);
                }
                if link == Link::Offline && in_grace(grace) {
                    link = Link::Connecting;
                }
                out.set(link);
            }
        }
        let graced = in_grace(grace);
        let pause = if graced { STARTUP_RETRY } else { backoff };
        match wait(rx, pause) {
            Waited::Stopped => return,
            Waited::Start => {
                launch(o, &mut grace);
                out.set(Link::Connecting);
            }
            Waited::Elapsed if !graced => backoff = (backoff * 2).min(MAX_BACKOFF),
            Waited::Elapsed => {}
        }
    }
}

fn in_grace(grace: Option<Instant>) -> bool {
    grace.is_some_and(|end| Instant::now() < end)
}

fn launch(o: &Options, grace: &mut Option<Instant>) {
    // A failed spawn shows as Offline with Start once the grace runs out; there is no
    // channel for the reason yet.
    let _ = (o.spawn)();
    *grace = Some(Instant::now() + o.grace);
}

enum Probe {
    Free,
    Held,
    Stopped,
}

/// Polls until no server answers on `name`, at most `limit` in all (a probe that blocks on a
/// busy pipe is abandoned on its own thread). Dropped commands do not end the wait; a closed
/// command channel does.
fn pipe_freed(name: &str, rx: &Receiver<Cmd>, limit: Duration) -> Probe {
    let end = Instant::now() + limit;
    loop {
        let (done, result) = mpsc::channel();
        let name = name.to_owned();
        thread::spawn(move || {
            let gone = matches!(
                Client::connect(&name),
                Err(Error::Io(e)) if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                )
            );
            let _ = done.send(gone);
        });
        match result.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(true) => return Probe::Free,
            Ok(false) => {}
            Err(_) => return Probe::Held,
        }
        match wait(
            rx,
            PROBE_PAUSE.min(end.saturating_duration_since(Instant::now())),
        ) {
            Waited::Stopped => return Probe::Stopped,
            Waited::Start | Waited::Elapsed => {}
        }
        if Instant::now() >= end {
            return Probe::Held;
        }
    }
}

enum Waited {
    Stopped,
    Start,
    Elapsed,
}

/// Sleeps `pause` while offline; commands for the daemon are dropped.
fn wait(rx: &Receiver<Cmd>, pause: Duration) -> Waited {
    let end = Instant::now() + pause;
    loop {
        match rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
            Ok(Cmd::Start | Cmd::Restart) => return Waited::Start,
            Ok(Cmd::Send(_)) => {}
            Err(RecvTimeoutError::Timeout) => return Waited::Elapsed,
            Err(RecvTimeoutError::Disconnected) => return Waited::Stopped,
        }
    }
}

/// Connects and says `Hello`; returns the connection and the daemon's protocol version.
fn open(name: &str) -> Result<(Client, u32), Error> {
    let mut conn = Client::connect(name)?;
    conn.set_write_timeout(Some(REPLY_WAIT))?;
    conn.set_read_timeout(Some(REPLY_WAIT))?;
    conn.send(&Request::Hello {
        proto: PROTO,
        version: env!("CARGO_PKG_VERSION").into(),
    })?;
    let Some(Push::HelloAck { proto, .. }) = conn.recv()? else {
        return Err(io::Error::from(io::ErrorKind::InvalidData).into());
    };
    conn.set_read_timeout(Some(POLL))?;
    Ok((conn, proto))
}

fn link_of(e: &Error) -> Link {
    match e {
        Error::Untrusted => Link::Untrusted,
        Error::ForeignServer => Link::Foreign,
        Error::Busy => Link::Busy,
        Error::Io(e) => match e.kind() {
            io::ErrorKind::PermissionDenied => Link::Foreign,
            io::ErrorKind::TimedOut => Link::Busy,
            _ => Link::Offline,
        },
        Error::AlreadyRunning => Link::Offline,
    }
}

enum Served {
    Stopped,
    Restart,
    Dropped,
}

/// A read that found nothing yet, or one bad line (the decoder carries on with the next).
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        // WouldBlock is how a Unix socket timeout reads.
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock | io::ErrorKind::InvalidData
    )
}

/// Runs one connection until it closes, fails, restarts or the commands end. On a protocol
/// mismatch nothing is sent or passed on except the restart.
fn serve(conn: &mut Client, rx: &Receiver<Cmd>, post: &impl Fn(Event), mismatch: bool) -> Served {
    loop {
        loop {
            match rx.try_recv() {
                Ok(Cmd::Send(request)) => {
                    if !mismatch && conn.send(&request).is_err() {
                        return Served::Dropped;
                    }
                }
                Ok(Cmd::Restart) => {
                    shutdown(conn);
                    return Served::Restart;
                }
                Ok(Cmd::Start) => {}
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return Served::Stopped,
            }
        }
        match conn.recv::<Push>() {
            Ok(Some(push)) => {
                if !mismatch {
                    post(Event::Push(push));
                }
            }
            Err(e) if is_transient(&e) => {}
            Ok(None) | Err(_) => return Served::Dropped,
        }
    }
}

/// Asks the daemon to exit and waits until it closes the connection.
fn shutdown(conn: &mut Client) {
    if conn.send(&Request::Shutdown).is_err() {
        return;
    }
    let end = Instant::now() + REPLY_WAIT;
    while Instant::now() < end {
        match conn.recv::<Push>() {
            Ok(Some(_)) => {}
            Err(e) if is_transient(&e) => {}
            Ok(None) | Err(_) => return,
        }
    }
}

type Subscriber = Rc<dyn Fn(&Event)>;

#[derive(Default)]
struct Inner {
    tx: Option<Sender<Cmd>>,
    /// Each with the id its [`Subscription`] removes it by.
    subs: RefCell<Vec<(u64, Subscriber)>>,
    next_sub: Cell<u64>,
    link: Cell<Link>,
}

/// Keeps an [`Ipc::subscribe`] callback alive; dropping it unsubscribes.
#[must_use = "dropping the subscription unsubscribes at once"]
pub struct Subscription {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.subs.borrow_mut().retain(|(id, _)| *id != self.id);
        }
    }
}

thread_local! {
    /// The `Ipc` started on this (UI) thread; worker events are delivered to it.
    // shortcut: one started link per UI thread, which is all the settings window has.
    static CURRENT: RefCell<Weak<Inner>> = const { RefCell::new(Weak::new()) };
}

/// The window's handle on the daemon; clones share one link. `Ipc::default()` is offline and
/// never connects. Use from the UI thread only.
#[derive(Clone, Default)]
pub struct Ipc(Rc<Inner>);

impl std::fmt::Debug for Ipc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ipc")
            .field("link", &self.link())
            .finish_non_exhaustive()
    }
}

impl Ipc {
    /// Starts the worker; its events arrive through [`Ipc::subscribe`]. Needs a running
    /// Slint event loop to deliver them.
    pub fn connect(options: Options) -> io::Result<Self> {
        let tx = spawn_worker(options, |event| {
            // Err means the event loop is gone: the window is closing.
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(inner) = CURRENT.with_borrow(Weak::upgrade) {
                    Ipc(inner).deliver(&event);
                }
            });
        })?;
        let ipc = Self(Rc::new(Inner {
            tx: Some(tx),
            link: Cell::new(Link::Connecting),
            ..Inner::default()
        }));
        CURRENT.set(Rc::downgrade(&ipc.0));
        Ok(ipc)
    }

    /// An `Ipc` with no worker, for tests: the receiver gets what would go to the daemon, and
    /// [`Ipc::deliver`] plays the daemon's side.
    pub fn fake() -> (Self, Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel();
        let inner = Inner {
            tx: Some(tx),
            link: Cell::new(Link::Online),
            ..Inner::default()
        };
        (Self(Rc::new(inner)), rx)
    }

    pub fn link(&self) -> Link {
        self.0.link.get()
    }

    /// Calls `f` for every event from now on, on the UI thread, until the returned
    /// [`Subscription`] is dropped. Read [`Ipc::link`] for the state before the call.
    pub fn subscribe(&self, f: impl Fn(&Event) + 'static) -> Subscription {
        let id = self.0.next_sub.get();
        self.0.next_sub.set(id + 1);
        self.0.subs.borrow_mut().push((id, Rc::new(f)));
        Subscription {
            inner: Rc::downgrade(&self.0),
            id,
        }
    }

    #[cfg(test)]
    pub(crate) fn subscribers(&self) -> usize {
        self.0.subs.borrow().len()
    }

    pub fn deliver(&self, event: &Event) {
        if let Event::Link(link) = event {
            self.0.link.set(*link);
        }
        // A copy: a callback may subscribe or drop a subscription.
        let subs: Vec<Subscriber> = self
            .0
            .subs
            .borrow()
            .iter()
            .map(|(_, f)| f.clone())
            .collect();
        for sub in subs {
            sub(event);
        }
    }

    /// Queues `request` for the daemon; dropped unless the link is [`Link::Online`].
    pub fn send(&self, request: Request) {
        self.cmd(Cmd::Send(request));
    }

    /// The Start button.
    pub fn start_daemon(&self) {
        self.cmd(Cmd::Start);
    }

    /// The update banner's button: shut the old daemon down and start a new one.
    pub fn restart_daemon(&self) {
        self.cmd(Cmd::Restart);
    }

    fn cmd(&self, cmd: Cmd) {
        if let Some(tx) = &self.0.tx {
            // Err: the worker ended, which only happens once every sender is gone.
            let _ = tx.send(cmd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_links() {
        let io = |k| Error::Io(io::Error::from(k));
        let table = [
            (Error::Untrusted, Link::Untrusted),
            (Error::ForeignServer, Link::Foreign),
            (Error::Busy, Link::Busy),
            (io(io::ErrorKind::PermissionDenied), Link::Foreign),
            (io(io::ErrorKind::TimedOut), Link::Busy),
            (io(io::ErrorKind::NotFound), Link::Offline),
            (io(io::ErrorKind::UnexpectedEof), Link::Offline),
        ];
        for (error, link) in table {
            assert_eq!(link_of(&error), link, "{error}");
        }
    }

    #[test]
    fn notices_for_the_blocked_states() {
        let n = notice(&Event::Link(Link::Mismatch { daemon_proto: 9 })).unwrap();
        assert_eq!(n.text, strings::DAEMON_MISMATCH);
        assert_eq!(n.action, Some(Action::Restart));
        let n = notice(&Event::Link(Link::Untrusted)).unwrap();
        assert!(n.text.contains("untrusted") && n.action.is_none());
        assert!(notice(&Event::Link(Link::Online)).is_none());
    }

    #[test]
    fn dropping_a_subscription_unsubscribes() {
        let (ipc, _rx) = Ipc::fake();
        let seen = Rc::new(Cell::new(0));
        let s = Rc::clone(&seen);
        let first = ipc.subscribe(move |_| s.set(s.get() + 1));
        let s = Rc::clone(&seen);
        let second = ipc.subscribe(move |_| s.set(s.get() + 10));
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(seen.get(), 11);
        drop(first);
        ipc.deliver(&Event::Link(Link::Online));
        assert_eq!(seen.get(), 21);
        assert_eq!(ipc.subscribers(), 1);
        drop(second);
        assert_eq!(ipc.subscribers(), 0);
    }

    #[test]
    fn unix_and_windows_read_timeouts_are_transient() {
        for (kind, transient) in [
            (io::ErrorKind::TimedOut, true),
            (io::ErrorKind::WouldBlock, true),
            (io::ErrorKind::InvalidData, true),
            (io::ErrorKind::BrokenPipe, false),
        ] {
            assert_eq!(is_transient(&io::Error::from(kind)), transient, "{kind:?}");
        }
    }
}
