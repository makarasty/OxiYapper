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
    /// The name is held by a server of another account, a lower integrity level, or one that
    /// denies us or stays busy (a squatter). Refuse to start or to
    /// talk to it; never treat it as a running daemon.
    #[error("the IPC pipe is held by another account or an unverifiable server")]
    ForeignServer,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// How long a connect waits while every pipe instance is busy.
const BUSY_WAIT: Duration = Duration::from_secs(2);
/// How long a second launch waits for the daemon to take `OpenSettings` and close.
const FORWARD_WAIT: Duration = Duration::from_secs(3);

/// This user's socket name: `\\.\pipe\OxiYapper-<SID>` on Windows.
#[cfg(windows)]
pub fn default_name() -> io::Result<String> {
    Ok(oxi_core::ipc::pipe_name(&crate::os::pipe::user_sid()?))
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
    /// user on the name gives [`Error::AlreadyRunning`], one of another account
    /// [`Error::ForeignServer`]; a stale Unix socket (connect refused) is removed and bound
    /// again.
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
                // The name exists but its server denies us or never frees an instance; ours
                // admits us and always keeps one free, so this one is not verifiably ours.
                Err(Error::Io(c))
                    if matches!(
                        c.kind(),
                        io::ErrorKind::PermissionDenied | io::ErrorKind::TimedOut
                    ) =>
                {
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
    #[cfg(windows)]
    read_timeout: Option<Duration>,
}

/// The settings window and a second launch connect with [`Conn::connect`].
pub type Client = Conn;

impl Conn {
    fn new(stream: Stream) -> Self {
        Self {
            stream,
            dec: Decoder::new(),
            #[cfg(windows)]
            read_timeout: None,
        }
    }

    /// Connects to the server on `name` after checking it belongs to the current user at our integrity level or above.
    #[cfg(windows)]
    pub fn connect(name: &str) -> Result<Self, Error> {
        use interprocess::os::windows::named_pipe::local_socket::Stream as PipeStream;
        use std::os::windows::io::AsHandle;

        let handle = crate::os::pipe::connect(name, BUSY_WAIT)?;
        if !crate::os::pipe::trusted_server(handle.as_handle())? {
            return Err(Error::ForeignServer);
        }
        let stream = PipeStream::try_from(handle).map_err(|e| {
            e.cause
                .unwrap_or_else(|| io::Error::other("not a byte pipe"))
        })?;
        Ok(Self::new(stream.into()))
    }

    // shortcut: no peer-credential check on Unix; the 0700 socket directory is the guard
    // until the Unix module lands.
    #[cfg(not(windows))]
    pub fn connect(name: &str) -> Result<Self, Error> {
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

    pub fn send<T: Message>(&mut self, msg: &T) -> io::Result<()> {
        self.stream.write_all(msg.encode().as_bytes())?;
        self.stream.flush()
    }

    /// The next message, `None` once the peer has closed. Any `Err` (a line over
    /// `MAX_LINE`, bad JSON: `InvalidData`; `TimedOut`) means drop this connection.
    pub fn recv<T: Message>(&mut self) -> io::Result<Option<T>> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some(msg) = T::decode(&mut self.dec) {
                return Ok(Some(msg?));
            }
            match self.read(&mut buf)? {
                0 => return Ok(None),
                n => self.dec.push(&buf[..n]),
            }
        }
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(windows)]
        if let Some(limit) = self.read_timeout {
            use std::os::windows::io::AsHandle;
            let Stream::NamedPipe(pipe) = &self.stream;
            return crate::os::pipe::with_deadline(pipe.inner().as_handle(), limit, || {
                (&self.stream).read(buf)
            });
        }
        (&self.stream).read(buf)
    }
}

/// Second launch: ask the daemon serving `name` to open settings, then wait (a few seconds
/// in total) until it closes the connection. The daemon closes a connection whose first
/// message is `OpenSettings` after acting on it (the settings window starts with `Hello`).
pub fn forward_open_settings(name: &str) -> Result<(), Error> {
    let deadline = Instant::now() + FORWARD_WAIT;
    let mut conn = Conn::connect(name)?;
    conn.send(&Request::OpenSettings)?;
    // A pipe closed before the daemon accepts it is discarded unread, so hold it open.
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "daemon did not close").into());
        }
        conn.set_read_timeout(Some(left))?;
        if conn.recv::<Push>()?.is_none() {
            return Ok(());
        }
    }
}
