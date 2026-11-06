//! Local socket server and client for daemon <-> settings (ARCHITECTURE section 5).
//!
//! Messages, framing and names come from `oxi_core::ipc`; this module only moves bytes.

use std::io::{self, Read, Write};

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
pub enum BindError {
    /// Another process of this user already serves the name.
    #[error("another instance is already running")]
    AlreadyRunning,
    #[error(transparent)]
    Io(#[from] io::Error),
}

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
    /// Binds `name` (a pipe path on Windows, a socket path on Unix). A live server on the
    /// name gives [`BindError::AlreadyRunning`]; a stale Unix socket (connect refused) is
    /// removed and bound again.
    pub fn bind(name: &str) -> Result<Self, BindError> {
        let err = match listen(name) {
            Ok(listener) => return Ok(Self { listener }),
            Err(e) => e,
        };
        match Conn::connect(name) {
            Ok(_) => Err(BindError::AlreadyRunning),
            // Only a socket file left by a crash refuses; a missing Windows pipe is NotFound.
            Err(c) if c.kind() == io::ErrorKind::ConnectionRefused => {
                std::fs::remove_file(name)?;
                Ok(Self {
                    listener: listen(name)?,
                })
            }
            Err(_) => Err(err.into()),
        }
    }

    /// Waits for the next connection.
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
}

/// The settings window and a second launch connect with [`Conn::connect`].
pub type Client = Conn;

impl Conn {
    fn new(stream: Stream) -> Self {
        Self {
            stream,
            dec: Decoder::new(),
        }
    }

    pub fn connect(name: &str) -> io::Result<Self> {
        Stream::connect(name.to_fs_name::<GenericFilePath>()?).map(Self::new)
    }

    pub fn send<T: Message>(&mut self, msg: &T) -> io::Result<()> {
        self.stream.write_all(msg.encode().as_bytes())?;
        self.stream.flush()
    }

    /// The next message, `None` once the peer has closed. Any `Err` (a line over
    /// `MAX_LINE`, bad JSON: `InvalidData`) means drop this connection.
    pub fn recv<T: Message>(&mut self) -> io::Result<Option<T>> {
        let mut buf = [0u8; 16 * 1024];
        loop {
            if let Some(msg) = T::decode(&mut self.dec) {
                return Ok(Some(msg?));
            }
            match self.stream.read(&mut buf)? {
                0 => return Ok(None),
                n => self.dec.push(&buf[..n]),
            }
        }
    }
}

/// Second launch: ask the daemon serving `name` to open settings, then wait until it closes
/// the connection. The daemon closes a connection whose first message is `OpenSettings`
/// (the settings window starts with `Hello`).
pub fn forward_open_settings(name: &str) -> io::Result<()> {
    let mut conn = Conn::connect(name)?;
    conn.send(&Request::OpenSettings)?;
    // A pipe closed before the daemon accepts it is discarded unread, so hold it open.
    while conn.recv::<Push>()?.is_some() {}
    Ok(())
}
