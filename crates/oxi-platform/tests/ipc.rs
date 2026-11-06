//! Local socket: single instance, round trip, oversized line, owner-only pipe.

use std::io;
use std::thread;

use oxi_core::ipc::{MAX_LINE, PROTO, Push, Request};
use oxi_platform::ipc::{Client, Error, Server, forward_open_settings};

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
        let daemon = s.spawn(|| server.accept().unwrap().recv::<Request>().unwrap());
        // Returns only after the daemon side has read the line and closed.
        forward_open_settings(&n).unwrap();
        daemon.join().unwrap()
    });
    assert_eq!(got, Some(Request::OpenSettings));
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
    use std::time::{Duration, Instant};

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
            "O:S-1-5-21-1-2-3-1001D:P(A;;GA;;;S-1-5-21-1-2-3-1001)"
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
        assert!(matches!(err, Error::Io(e) if e.kind() == io::ErrorKind::TimedOut));
    }

    #[test]
    fn forward_gives_up_when_daemon_keeps_conn() {
        let n = name("fwdhang");
        let server = Server::bind(&n).unwrap();
        let n2 = n.clone();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || tx.send(forward_open_settings(&n2)));
        // Accepts and neither reads nor closes.
        let _conn = server.accept().unwrap();
        let got = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(matches!(got, Err(Error::Io(e)) if e.kind() == io::ErrorKind::TimedOut));
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
}
