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
use oxi_platform::ipc::{Client, Error, default_name};

/// How long the worker waits on the pipe before it looks at the commands again.
// shortcut: `Conn` cannot be split into a reader and a writer, so a timed-out `recv` is
// retried instead of dropping the connection; split it in oxi-platform if the latency matters.
const POLL: Duration = Duration::from_millis(100);
/// `Hello` write and reply, and the wait for the old daemon to close on a restart.
const REPLY_WAIT: Duration = Duration::from_secs(2);
const MIN_BACKOFF: Duration = Duration::from_millis(250);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
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
        Event::Link(Link::Offline) => (
            "OxiYapper is not running. Settings are saved to the config file.".to_owned(),
            Some(Action::Start),
        ),
        Event::Link(Link::Mismatch { .. }) => (
            "Restart OxiYapper to finish the update".to_owned(),
            Some(Action::Restart),
        ),
        Event::Link(Link::Busy) => ("OxiYapper is running but not answering".to_owned(), None),
        Event::Link(Link::Untrusted) => (
            "The OxiYapper pipe is held by an untrusted program. Not connecting.".to_owned(),
            None,
        ),
        Event::Link(Link::Foreign) => (
            "The OxiYapper pipe is held by another account. Not connecting.".to_owned(),
            None,
        ),
        Event::Push(Push::ConfigError {
            file,
            line,
            message,
        }) => (format!("{file}, line {line}: {message}"), None),
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
}

impl Options {
    /// This user's daemon: its pipe, started from this executable.
    pub fn daemon() -> io::Result<Self> {
        Ok(Self {
            name: default_name()?,
            spawn: Box::new(spawn_daemon),
            grace: Duration::from_secs(2),
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
    out.set(Link::Connecting);
    loop {
        match open(&o.name) {
            Ok((mut conn, daemon_proto)) => {
                backoff = MIN_BACKOFF;
                let mismatch = daemon_proto != PROTO;
                out.set(if mismatch {
                    Link::Mismatch { daemon_proto }
                } else {
                    Link::Online
                });
                match serve(&mut conn, rx, &out.post, mismatch) {
                    Served::Stopped => return,
                    Served::Restart => {
                        launch(o, &mut grace);
                        out.set(Link::Connecting);
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
        let pause = if in_grace(grace) {
            STARTUP_RETRY
        } else {
            backoff
        };
        match wait(rx, pause) {
            Waited::Stopped => return,
            Waited::Start => {
                launch(o, &mut grace);
                out.set(Link::Connecting);
            }
            Waited::Elapsed => backoff = (backoff * 2).min(MAX_BACKOFF),
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
        io::ErrorKind::TimedOut | io::ErrorKind::InvalidData
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
    subs: RefCell<Vec<Subscriber>>,
    link: Cell<Link>,
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

    /// Calls `f` for every event from now on, on the UI thread. Read [`Ipc::link`] for the
    /// state before the call.
    pub fn subscribe(&self, f: impl Fn(&Event) + 'static) {
        self.0.subs.borrow_mut().push(Rc::new(f));
    }

    pub fn deliver(&self, event: &Event) {
        if let Event::Link(link) = event {
            self.0.link.set(*link);
        }
        let subs = self.0.subs.borrow().clone();
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
        assert_eq!(n.text, "Restart OxiYapper to finish the update");
        assert_eq!(n.action, Some(Action::Restart));
        let n = notice(&Event::Link(Link::Untrusted)).unwrap();
        assert!(n.text.contains("untrusted") && n.action.is_none());
        assert!(notice(&Event::Link(Link::Online)).is_none());
    }
}
