//! Local socket server and client for daemon <-> settings (ARCHITECTURE section 5).
//!
//! Messages, framing and names come from `oxi_core::ipc`; this module only moves bytes.

use std::io::{self, Read, Write};
use std::time::{Duration, Instant};

use interprocess::local_socket::{GenericFilePath, Listener, ListenerOptions, Stream, prelude::*};
use oxi_core::ipc::{DecodeError, Decoder, Push, Request};

/// A message type of `oxi_core::ipc`. Kept here so this crate needs no serde of its own.
pub trait Message: Sized {
    fn encode(&self) -> String;
    fn decode(dec: &mut Decoder) -> Option<Result<Self, DecodeError>>;
}

macro_rules! message {
    ($($t:ty),*) => {$(
        impl Message for $t {
            fn encode(&self) -> String {
                oxi_core::ipc::encode(self)
            }
            fn decode(dec: &mut Decoder) -> Option<Result<Self, DecodeError>> {
                dec.decode()
            }
        }
    )*};
}
message!(Request, Push);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// From [`Server::bind`]: a daemon of this user already serves the name.
    #[error("another instance is already running")]
    AlreadyRunning,
    /// The name exists but no instance came free in time: a stalled daemon, or a squatter
    /// that cannot be told apart from one.
    #[error("OxiYapper is running but not answering")]
    Busy,
    /// The name is held by a server of another account, or by one that denies us (a
    /// squatter). Refuse to start or to talk to it; never treat it as a running daemon.
    #[error("the IPC pipe is held by another account")]
    ForeignServer,
    /// The server runs as us but below medium integrity: a sandboxed or low-integrity
    /// program squatting the name (the daemon always labels its pipe medium).
    #[error("the OxiYapper pipe is held by an untrusted program")]
    Untrusted,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// How long a connect waits while every pipe instance is busy.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// The whole second launch: connect, send `OpenSettings`, wait for the daemon to close.
const FORWARD_WAIT: Duration = Duration::from_secs(3);

/// Test seam: a non-empty value gives the pipe `<default name>-<value>`, so a test daemon never
/// meets the user's real one. Children inherit it, so a daemon and its settings agree.
pub const PIPE_SUFFIX_ENV: &str = "OXI_PIPE_SUFFIX";

/// This user's socket name: `\\.\pipe\OxiYapper-<SID>` on Windows.
#[cfg(windows)]
pub fn default_name() -> io::Result<String> {
    let name = oxi_core::ipc::pipe_name(&crate::os::pipe::user_sid()?);
    Ok(match std::env::var(PIPE_SUFFIX_ENV) {
        Ok(suffix) if !suffix.is_empty() => format!("{name}-{suffix}"),
        _ => name,
    })
}

// shortcut: Unix has no per-user socket directory yet, fill it with the Unix platform module.
#[cfg(not(windows))]
pub fn default_name() -> io::Result<String> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no IPC socket directory on this OS yet",
    ))
}

/// Listening end, owned by the daemon.
#[derive(Debug)]
pub struct Server {
    listener: Listener,
}

impl Server {
    /// Binds `name` (a pipe path on Windows, a socket path on Unix). A live server of this
    /// user on the name gives [`Error::AlreadyRunning`] (or [`Error::Busy`] when it frees
    /// no instance), a squatter [`Error::ForeignServer`] or [`Error::Untrusted`]; a
    /// stale Unix socket (connect refused) is removed and bound again.
    pub fn bind(name: &str) -> Result<Self, Error> {
        let mut retried = false;
        loop {
            let err = match listen(name) {
                Ok(listener) => return Ok(Self { listener }),
                Err(e) => e,
            };
            return match Conn::connect(name) {
                Ok(_) => Err(Error::AlreadyRunning),
                // The server exited between the failed listen and the probe: try once more.
                Err(Error::Io(c)) if c.kind() == io::ErrorKind::NotFound && !retried => {
                    retried = true;
                    continue;
                }
                // shortcut: two launches racing on one stale socket can both unlink it and one
                // loses the name; when Unix lands, guard with a lock file or bind a temp path
                // and rename it over.
                // Only a socket file left by a crash refuses; a missing Windows pipe is NotFound.
                Err(Error::Io(c)) if c.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(name)?;
                    Ok(Self {
                        listener: listen(name)?,
                    })
                }
                // The name exists and its server denies us; ours always admits its user.
                Err(Error::Io(c)) if c.kind() == io::ErrorKind::PermissionDenied => {
                    Err(Error::ForeignServer)
                }
                Err(Error::Io(_)) => Err(err.into()),
                Err(e) => Err(e),
            };
        }
    }

    /// Waits for the next connection. A client that connected and closed before this call
    /// (such as the probe in [`Server::bind`]) is skipped or reads as an empty connection.
    pub fn accept(&self) -> io::Result<Conn> {
        self.listener.accept().map(Conn::new)
    }
}

fn listen(name: &str) -> io::Result<Listener> {
    let opts = ListenerOptions::new().name(name.to_fs_name::<GenericFilePath>()?);
    #[cfg(windows)]
    let opts = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        // interprocess sets PIPE_REJECT_REMOTE_CLIENTS itself (`accept_remote` is false).
        opts.security_descriptor(crate::os::pipe::owner_only()?)
    };
    opts.create_sync()
}

/// One connection, either end.
#[derive(Debug)]
pub struct Conn {
    stream: Stream,
    dec: Decoder,
    broken: bool,
    #[cfg(windows)]
    read_timeout: Option<Duration>,
    #[cfg(windows)]
    write_timeout: Option<Duration>,
}

/// The settings window and a second launch connect with [`Conn::connect`].
pub type Client = Conn;

impl Conn {
    fn new(stream: Stream) -> Self {
        Self {
            stream,
            dec: Decoder::new(),
            broken: false,
            #[cfg(windows)]
            read_timeout: None,
            #[cfg(windows)]
            write_timeout: None,
        }
    }

    /// Connects to the server on `name` after checking it belongs to the current user at
    /// medium integrity or above.
    pub fn connect(name: &str) -> Result<Self, Error> {
        Self::connect_within(name, BUSY_WAIT)
    }

    #[cfg(windows)]
    fn connect_within(name: &str, busy_wait: Duration) -> Result<Self, Error> {
        use crate::os::pipe::{Verdict, check_server};
        use interprocess::os::windows::named_pipe::local_socket::Stream as PipeStream;
        use std::os::windows::io::AsHandle;

        let handle = match crate::os::pipe::connect(name, busy_wait) {
            Err(e) if e.kind() == io::ErrorKind::TimedOut => return Err(Error::Busy),
            r => r?,
        };
        match check_server(handle.as_handle())? {
            Verdict::Trusted => {}
            Verdict::OtherAccount => return Err(Error::ForeignServer),
            Verdict::LowIntegrity => return Err(Error::Untrusted),
        }
        let stream = PipeStream::try_from(handle).map_err(|e| {
            e.cause
                .unwrap_or_else(|| io::Error::other("not a byte pipe"))
        })?;
        Ok(Self::new(stream.into()))
    }

    // shortcut: no peer-credential check or busy wait on Unix; the 0700 socket directory is
    // the guard until the Unix module lands.
    #[cfg(not(windows))]
    fn connect_within(name: &str, _busy_wait: Duration) -> Result<Self, Error> {
        Ok(Self::new(Stream::connect(
            name.to_fs_name::<GenericFilePath>()?,
        )?))
    }

    /// Makes a [`Conn::recv`] that waits longer than `limit` fail with `TimedOut`.
    pub fn set_read_timeout(&mut self, limit: Option<Duration>) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.read_timeout = limit;
            Ok(())
        }
        #[cfg(not(windows))]
        self.stream.set_recv_timeout(limit)
    }

    /// Makes a [`Conn::send`] blocked longer than `limit` (the peer stopped reading and the
    /// pipe buffer is full) fail with `TimedOut`.
    pub fn set_write_timeout(&mut self, limit: Option<Duration>) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.write_timeout = limit;
            Ok(())
        }
        #[cfg(not(windows))]
        self.stream.set_send_timeout(limit)
    }

    /// Writes one framed message. After an `Err` part of it may be on the wire, so the
    /// connection is poisoned: every later `send` and `recv` fails with `BrokenPipe`. No
    /// flush: the local socket `flush` is a no-op and interprocess flushes on drop.
    pub fn send<T: Message>(&mut self, msg: &T) -> io::Result<()> {
        self.check_broken()?;
        let line = msg.encode();
        #[cfg(windows)]
        let limit = self.write_timeout;
        #[cfg(not(windows))]
        let limit = None;
        let stream = &self.stream;
        let result = timed(stream, limit, || {
            let mut stream = stream;
            stream.write_all(line.as_bytes())
        });
        if result.is_err() {
            self.broken = true;
            // Unsent bytes would make interprocess's drop wait on a FlushFileBuffers that a
            // peer which never reads never ends, holding its linger thread.
            #[cfg(windows)]
            {
                let Stream::NamedPipe(pipe) = &self.stream;
                pipe.inner().assume_flushed();
            }
        }
        result
    }

    /// The next message, `None` once the peer has closed. The read timeout covers the whole
    /// call, however the peer splits the line. `TimedOut` leaves the connection usable: bytes
    /// of a half-read line stay buffered and a later `recv` finishes it, so a loop may poll
    /// with a short timeout. Any other `Err` (a line over `MAX_LINE`, bad JSON: `InvalidData`)
    /// means drop this connection.
    pub fn recv<T: Message>(&mut self) -> io::Result<Option<T>> {
        self.check_broken()?;
        let mut buf = [0u8; 16 * 1024];
        // shortcut: on Unix the socket timeout applies per read; make it per call when the
        // Unix module lands.
        #[cfg(windows)]
        let deadline = self.read_timeout.map(|limit| Instant::now() + limit);
        #[cfg(not(windows))]
        let deadline: Option<Instant> = None;
        loop {
            if let Some(msg) = T::decode(&mut self.dec) {
                return Ok(Some(msg?));
            }
            let left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if left == Some(Duration::ZERO) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC I/O timed out"));
            }
            let stream = &self.stream;
            match timed(stream, left, || {
                let mut stream = stream;
                stream.read(&mut buf)
            })? {
                0 => return Ok(None),
                n => self.dec.push(&buf[..n]),
            }
        }
    }

    /// Process id of the client, on the server end of a connection from [`Server::accept`].
    #[cfg(windows)]
    pub fn peer_pid(&self) -> io::Result<u32> {
        use std::os::windows::io::AsHandle;
        let Stream::NamedPipe(pipe) = &self.stream;
        crate::os::pipe::client_pid(pipe.inner().as_handle())
    }

    // shortcut: no peer-credential lookup on Unix until its module lands.
    #[cfg(not(windows))]
    pub fn peer_pid(&self) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no peer process id on this OS yet",
        ))
    }

    fn check_broken(&self) -> io::Result<()> {
        if self.broken {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "IPC connection dropped after a failed send",
            ));
        }
        Ok(())
    }
}

/// Runs blocking I/O on `stream`, cancelled after `limit` on Windows (Unix sets the socket
/// timeouts instead, so `limit` is `None` there).
fn timed<R>(
    stream: &Stream,
    limit: Option<Duration>,
    op: impl FnOnce() -> io::Result<R>,
) -> io::Result<R> {
    #[cfg(windows)]
    if let Some(limit) = limit {
        use std::os::windows::io::AsHandle;
        let Stream::NamedPipe(pipe) = stream;
        return crate::os::pipe::with_deadline(pipe.inner().as_handle(), limit, op);
    }
    let _ = (stream, limit);
    op()
}

/// Second launch: ask the daemon serving `name` to open settings, then wait until it closes
/// the connection; all of it together takes at most a few seconds. The daemon answers a
/// connection whose first message is `OpenSettings` with `HelloAck` and closes it after
/// acting on it (the settings window starts with `Hello`). A close without that ack (a daemon
/// that refused the connection for want of a free slot, or an older one) is [`Error::Busy`]:
/// the request may not have been acted on.
pub fn forward_open_settings(name: &str) -> Result<(), Error> {
    let deadline = Instant::now() + FORWARD_WAIT;
    let left = || match deadline.saturating_duration_since(Instant::now()) {
        Duration::ZERO => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "daemon did not close",
        )),
        left => Ok(left),
    };
    let mut conn = Conn::connect_within(name, BUSY_WAIT.min(left()?))?;
    conn.set_write_timeout(Some(left()?))?;
    match conn.send(&Request::OpenSettings) {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Err(Error::Busy),
        r => r?,
    }
    // A pipe closed before the daemon accepts it is discarded unread, so hold it open.
    let mut acked = false;
    loop {
        conn.set_read_timeout(Some(left()?))?;
        match conn.recv::<Push>()? {
            None if acked => return Ok(()),
            None => return Err(Error::Busy),
            Some(Push::HelloAck { .. }) => acked = true,
            Some(_) => {}
        }
    }
}
