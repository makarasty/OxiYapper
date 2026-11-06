//! Local socket: single instance, round trip, oversized line, owner-only pipe.

use std::io;
use std::thread;
use std::time::Duration;

use oxi_core::ipc::{MAX_LINE, PROTO, Push, Request};
use oxi_platform::ipc::{Client, Error, Message, Server, forward_open_settings};

/// Bytes written as they are, to split a line on the wire.
struct Raw(String);

impl Message for Raw {
    fn encode(&self) -> String {
        self.0.clone()
    }
    fn decode(_: &mut oxi_core::ipc::Decoder) -> Option<Result<Self, oxi_core::ipc::DecodeError>> {
        None
    }
}

/// A name no other test or running daemon uses.
fn name(tag: &str) -> String {
    let id = format!("OxiYapper-test-{}-{tag}", std::process::id());
    if cfg!(windows) {
        format!(r"\\.\pipe\{id}")
    } else {
        std::env::temp_dir().join(id).to_string_lossy().into_owned()
    }
}

fn hello() -> Request {
    Request::Hello {
        proto: PROTO,
        version: "0.0.0-test".into(),
    }
}

/// Serves one Hello on the next connection and returns what the client got back.
fn hello_round_trip(server: &Server, name: &str) -> Option<Push> {
    thread::scope(|s| {
        s.spawn(|| {
            let mut conn = server.accept().unwrap();
            let Some(Request::Hello { proto, version }) = conn.recv().unwrap() else {
                panic!("expected Hello");
            };
            conn.send(&Push::HelloAck { proto, version }).unwrap();
        });
        let mut client = Client::connect(name).unwrap();
        client.send(&hello()).unwrap();
        client.recv().unwrap()
    })
}

#[test]
fn second_bind_is_already_running() {
    let n = name("twice");
    let _first = Server::bind(&n).unwrap();
    assert!(matches!(Server::bind(&n), Err(Error::AlreadyRunning)));
}

#[test]
fn hello_gets_hello_ack() {
    let n = name("hello");
    let server = Server::bind(&n).unwrap();
    assert_eq!(
        hello_round_trip(&server, &n),
        Some(Push::HelloAck {
            proto: PROTO,
            version: "0.0.0-test".into(),
        })
    );
}

#[test]
fn oversized_line_closes_only_that_connection() {
    let n = name("big");
    let server = Server::bind(&n).unwrap();
    let err = thread::scope(|s| {
        s.spawn(|| {
            let mut client = Client::connect(&n).unwrap();
            // The server drops the connection mid-line, so this write may fail; that is fine.
            let _ = client.send(&Request::DownloadModel {
                id: "a".repeat(MAX_LINE),
            });
        });
        server.accept().unwrap().recv::<Request>().unwrap_err()
    });
    assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    // The server keeps serving new connections.
    assert!(matches!(
        hello_round_trip(&server, &n),
        Some(Push::HelloAck { .. })
    ));
}

#[test]
fn second_launch_forwards_open_settings() {
    let n = name("forward");
    let server = Server::bind(&n).unwrap();
    let got = thread::scope(|s| {
        let daemon = s.spawn(|| {
            let mut conn = server.accept().unwrap();
            let got = conn.recv::<Request>().unwrap();
            conn.send(&Push::HelloAck {
                proto: PROTO,
                version: "0.0.0-test".into(),
            })
            .unwrap();
            got
        });
        // Returns only after the daemon side has read the line, acked and closed.
        forward_open_settings(&n).unwrap();
        daemon.join().unwrap()
    });
    assert_eq!(got, Some(Request::OpenSettings));
}

#[test]
fn close_without_ack_is_busy_not_forwarded() {
    let n = name("noack");
    let server = Server::bind(&n).unwrap();
    thread::scope(|s| {
        s.spawn(|| {
            // A daemon with no free slot: reads the line like the real one, then drops it.
            let mut conn = server.accept().unwrap();
            let _ = conn.recv::<Request>();
        });
        let err = forward_open_settings(&n).unwrap_err();
        assert!(matches!(err, Error::Busy), "{err:?}");
    });
}

#[test]
fn recv_timeout_keeps_the_connection_and_a_half_line() {
    let n = name("halfline");
    let server = Server::bind(&n).unwrap();
    thread::scope(|s| {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        s.spawn(move || {
            let mut conn = server.accept().unwrap();
            let _ = conn.recv::<Request>();
            // Raw bytes: half of a Push line, a pause, the rest.
            let line = oxi_core::ipc::encode(&Push::Raise);
            let (a, b) = line.split_at(line.len() / 2);
            conn.send(&Raw(a.into())).unwrap();
            wait.recv().unwrap();
            conn.send(&Raw(b.into())).unwrap();
        });
        let mut client = Client::connect(&n).unwrap();
        client.send(&hello()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        for _ in 0..2 {
            let err = client.recv::<Push>().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        }
        go.send(()).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(client.recv::<Push>().unwrap(), Some(Push::Raise));
    });
}

#[cfg(unix)]
#[test]
fn stale_socket_is_replaced() {
    let n = name("stale");
    let _ = std::fs::remove_file(&n);
    // std leaves the socket file behind on drop, as a crashed daemon would.
    drop(std::os::unix::net::UnixListener::bind(&n).unwrap());
    let server = Server::bind(&n).unwrap();
    assert!(hello_round_trip(&server, &n).is_some());
}

#[cfg(windows)]
mod windows {
    use std::io::Read;
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use std::sync::mpsc;
    use std::time::Instant;

    use interprocess::local_socket::{GenericFilePath, ListenerOptions, Stream, prelude::*};
    use oxi_platform::os::pipe::{owner_only_sddl, user_sid};
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Security::{
        GetTokenInformation, RevertToSelf, SecurityIdentification, TOKEN_QUERY,
        TokenImpersonationLevel,
    };
    use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

    use super::*;

    /// A pipe every Windows box has, owned by Administrators: stands in for a squatter.
    const FOREIGN: &str = r"\\.\pipe\lsass";

    /// Runs `f` on a thread and fails the test if it has not returned within `secs`.
    fn within<R: Send + 'static>(secs: u64, f: impl FnOnce() -> R + Send + 'static) -> R {
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(f()));
        rx.recv_timeout(Duration::from_secs(secs))
            .expect("call did not return in time")
    }

    #[test]
    fn sddl_admits_only_the_given_sid() {
        let sddl = owner_only_sddl("S-1-5-21-1-2-3-1001");
        assert_eq!(
            sddl,
            "O:S-1-5-21-1-2-3-1001D:P(A;;GA;;;S-1-5-21-1-2-3-1001)S:(ML;;NWNR;;;ME)"
        );
        assert!(!sddl.contains("WD") && !sddl.contains("Everyone"));
    }

    #[test]
    fn default_name_holds_the_user_sid() {
        let sid = user_sid().unwrap();
        assert!(sid.starts_with("S-1-5-"), "{sid}");
        assert_eq!(
            oxi_platform::ipc::default_name().unwrap(),
            oxi_core::ipc::pipe_name(&sid)
        );
    }

    #[test]
    fn pipe_of_another_account_is_foreign_not_running() {
        assert!(matches!(Server::bind(FOREIGN), Err(Error::ForeignServer)));
        assert!(matches!(
            Client::connect(FOREIGN),
            Err(Error::ForeignServer)
        ));
        assert!(matches!(
            forward_open_settings(FOREIGN),
            Err(Error::ForeignServer)
        ));
    }

    #[test]
    fn busy_pipe_connect_times_out() {
        let n = name("busy");
        let _server = Server::bind(&n).unwrap();
        // Takes the only waiting instance; nobody accepts it.
        let _hog = Client::connect(&n).unwrap();
        let err = within(10, move || Client::connect(&n).map(drop).unwrap_err());
        assert!(matches!(err, Error::Busy), "{err:?}");
    }

    #[test]
    fn forward_gives_up_when_daemon_keeps_conn() {
        let n = name("fwdhang");
        let server = Server::bind(&n).unwrap();
        let n2 = n.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let t = Instant::now();
            tx.send((forward_open_settings(&n2), t.elapsed()))
        });
        // Accepts and neither reads nor closes.
        let _conn = server.accept().unwrap();
        let (got, took) = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(got, Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut));
        assert!(took < Duration::from_millis(3500), "{took:?}");
    }

    #[test]
    fn read_timeout_ends_a_silent_recv() {
        let n = name("silent");
        let server = Server::bind(&n).unwrap();
        thread::scope(|s| {
            s.spawn(|| {
                let mut c = server.accept().unwrap();
                // Hold the connection until the client side is done.
                let _ = c.recv::<Request>();
            });
            let mut client = Client::connect(&n).unwrap();
            client
                .set_read_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let t = Instant::now();
            let err = client.recv::<Push>().unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::TimedOut);
            assert!(t.elapsed() < Duration::from_secs(5));
            drop(client);
        });
    }

    #[test]
    fn client_allows_only_identification() {
        let n = name("sqos");
        let listener = ListenerOptions::new()
            .name(n.as_str().to_fs_name::<GenericFilePath>().unwrap())
            .create_sync()
            .unwrap();
        let level = thread::scope(|s| {
            s.spawn(|| {
                let mut c = Client::connect(&n).unwrap();
                c.send(&hello()).unwrap();
                let _ = c.recv::<Push>();
            });
            let mut stream = listener.accept().unwrap();
            // A pipe server can impersonate only after reading from the client.
            let mut b = [0u8; 1];
            stream.read_exact(&mut b).unwrap();
            let Stream::NamedPipe(pipe) = &stream;
            let h = pipe.inner().as_handle().as_raw_handle();
            // SAFETY: `h` is a connected server pipe handle that has been read from.
            assert_ne!(unsafe { ImpersonateNamedPipeClient(h) }, 0);
            let mut token = std::ptr::null_mut();
            // SAFETY: valid out pointer; this thread impersonates the client now.
            let opened =
                unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &raw mut token) };
            let mut level = -1i32;
            let mut len = 0u32;
            if opened != 0 {
                // SAFETY: `level` has room for a SECURITY_IMPERSONATION_LEVEL (i32).
                unsafe {
                    GetTokenInformation(
                        token,
                        TokenImpersonationLevel,
                        (&raw mut level).cast(),
                        4,
                        &raw mut len,
                    )
                };
                // SAFETY: opened above, closed once.
                unsafe { CloseHandle(token) };
            }
            // SAFETY: ends the impersonation started above.
            unsafe { RevertToSelf() };
            level
        });
        assert_eq!(level, SecurityIdentification);
    }

    /// A pipe on `n` created by this process with the descriptor `sddl`.
    fn pipe_with(n: &str, sddl: &str) -> interprocess::local_socket::Listener {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        ListenerOptions::new()
            .name(n.to_fs_name::<GenericFilePath>().unwrap())
            .security_descriptor(oxi_platform::os::pipe::security_descriptor(sddl).unwrap())
            .create_sync()
            .unwrap()
    }

    #[test]
    fn zero_read_timeout_still_returns() {
        let n = name("zero");
        let server = Server::bind(&n).unwrap();
        let n2 = n.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut c = Client::connect(&n2).unwrap();
            c.set_read_timeout(Some(Duration::ZERO)).unwrap();
            // Many tries: the cancel can land before the read is pending.
            for _ in 0..200 {
                tx.send(c.recv::<Push>().map(drop).unwrap_err().kind())
                    .unwrap();
            }
        });
        let _conn = server.accept().unwrap();
        for _ in 0..200 {
            let got = rx.recv_timeout(Duration::from_secs(5));
            assert_eq!(got, Ok(io::ErrorKind::TimedOut));
        }
    }

    #[test]
    fn forward_deadline_is_total_not_per_read() {
        let n = name("chatty");
        let server = Server::bind(&n).unwrap();
        let n2 = n.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(forward_open_settings(&n2)));
        let mut conn = server.accept().unwrap();
        // Read the request, as a real daemon does: interprocess holds a dropped, unread
        // client handle open until its peer reads, and both ends live in this process.
        assert_eq!(conn.recv::<Request>().unwrap(), Some(Request::OpenSettings));
        // A daemon that pushes every 500 ms and never closes; stops when the peer goes.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10) && conn.send(&Push::Raise).is_ok() {
            if let Ok(got) = rx.recv_timeout(Duration::from_millis(500)) {
                assert!(matches!(got, Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut));
                assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
                return;
            }
        }
        panic!("forward_open_settings did not give up");
    }

    #[test]
    fn pipe_that_denies_us_is_foreign() {
        let n = name("deny");
        let _squatter = pipe_with(&n, "D:P");
        assert!(matches!(Server::bind(&n), Err(Error::ForeignServer)));
    }

    #[test]
    fn name_held_busy_is_busy_not_foreign() {
        let n = name("held");
        let _squatter = pipe_with(&n, &format!("D:P(A;;GA;;;{})", user_sid().unwrap()));
        let _hog = Client::connect(&n);
        let got = within(10, move || Server::bind(&n).map(drop));
        assert!(matches!(got, Err(Error::Busy)), "{got:?}");
    }

    #[test]
    fn busy_pipe_that_vanishes_is_not_found() {
        let n = name("vanish");
        let server = Server::bind(&n).unwrap();
        let hog = Client::connect(&n).unwrap();
        let (tx, rx) = mpsc::channel();
        let n2 = n.clone();
        thread::spawn(move || tx.send(Client::connect(&n2).map(drop)));
        // Best effort: let the client reach WaitNamedPipeW; either order must give NotFound.
        thread::sleep(Duration::from_millis(300));
        drop(hog);
        drop(server);
        let got = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            matches!(got, Err(Error::Io(ref e)) if e.kind() == io::ErrorKind::NotFound),
            "{got:?}"
        );
    }

    #[test]
    fn low_integrity_pipe_of_ours_untrusted() {
        type Call = fn(&str) -> Result<(), Error>;
        let sid = user_sid().unwrap();
        let low = format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NW;;;LW)");
        // A fresh squatter per call: each probe takes and drops its only instance.
        let cases: [(&str, Call); 3] = [
            ("lowc", |n| Client::connect(n).map(drop)),
            ("lowb", |n| Server::bind(n).map(drop)),
            ("lowf", forward_open_settings),
        ];
        for (tag, call) in cases {
            let n = name(tag);
            let _squatter = pipe_with(&n, &low);
            assert!(matches!(call(&n), Err(Error::Untrusted)), "{tag}");
        }
    }

    #[test]
    fn medium_labelled_pipe_of_ours_is_trusted() {
        let n = name("medium");
        let sid = user_sid().unwrap();
        let _daemon = pipe_with(&n, &owner_only_sddl(&sid));
        assert!(Client::connect(&n).is_ok());
    }

    #[test]
    fn write_timeout_ends_a_send_to_a_stalled_peer() {
        let n = name("stallw");
        let server = Server::bind(&n).unwrap();
        let n2 = n.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let mut c = Client::connect(&n2).unwrap();
            c.set_write_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            // Far more than the pipe buffer, so the write blocks on a peer that never reads.
            let big = Request::DownloadModel {
                id: "a".repeat(4 * MAX_LINE),
            };
            tx.send(c.send(&big).map_err(|e| e.kind())).unwrap();
        });
        let _conn = server.accept().unwrap();
        let got = rx.recv_timeout(Duration::from_secs(10));
        assert_eq!(got, Ok(Err(io::ErrorKind::TimedOut)));
    }

    /// A client whose big send timed out against a peer that never reads.
    fn timed_out_sender(server: &Server, n: &str) -> (Client, Client) {
        let n2 = n.to_owned();
        let client = thread::spawn(move || {
            let mut c = Client::connect(&n2).unwrap();
            c.set_write_timeout(Some(Duration::from_millis(200)))
                .unwrap();
            let big = Request::DownloadModel {
                id: "a".repeat(4 * MAX_LINE),
            };
            assert_eq!(c.send(&big).unwrap_err().kind(), io::ErrorKind::TimedOut);
            c
        });
        let conn = server.accept().unwrap();
        (client.join().unwrap(), conn)
    }

    #[test]
    fn send_after_timed_out_send_is_refused() {
        let n = name("poison");
        let server = Server::bind(&n).unwrap();
        let (mut client, _conn) = timed_out_sender(&server, &n);
        let err = client.send(&Request::Status).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(
            client.recv::<Push>().unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn dropping_a_timed_out_sender_does_not_stall_later_closes() {
        let n = name("linger");
        let server = Server::bind(&n).unwrap();
        // The stalled peer stays alive and unread for the whole test.
        let (client, _stalled) = timed_out_sender(&server, &n);
        drop(client);
        // Later connections in this process must still close.
        let got = within(10, move || {
            let mut conns = Vec::new();
            for _ in 0..4 {
                let n2 = n.clone();
                let t = thread::spawn(move || {
                    let mut c = Client::connect(&n2).unwrap();
                    c.send(&Request::Status).unwrap();
                });
                let mut conn = server.accept().unwrap();
                t.join().unwrap();
                assert_eq!(conn.recv::<Request>().unwrap(), Some(Request::Status));
                assert_eq!(conn.recv::<Request>().unwrap(), None);
                conns.push(conn);
            }
            conns.len()
        });
        assert_eq!(got, 4);
    }

    #[test]
    fn forward_deadline_covers_a_trickled_line() {
        use std::io::Write;
        let n = name("trickle");
        let listener = pipe_with(&n, &owner_only_sddl(&user_sid().unwrap()));
        let n2 = n.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let t = Instant::now();
            tx.send((forward_open_settings(&n2), t.elapsed()))
        });
        let mut stream = listener.accept().unwrap();
        // Read the request first, as a real daemon does: an unread dirty handle pins
        // interprocess's linger thread (see forward_deadline_is_total_not_per_read).
        let mut b = [0u8; 1];
        while b[0] != b'\n' {
            stream.read_exact(&mut b).unwrap();
        }
        // One byte of a never-ending line every 100 ms: each read returns, no line completes.
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(10) {
            if let Ok((got, took)) = rx.try_recv() {
                assert!(matches!(got, Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut));
                assert!(took < Duration::from_millis(3500), "{took:?}");
                return;
            }
            if stream.write_all(b" ").is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        panic!("forward_open_settings outlived its deadline");
    }
}
