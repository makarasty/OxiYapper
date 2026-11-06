//! Local socket: single instance, round trip, oversized line, owner-only pipe.

use std::io;
use std::thread;

use oxi_core::ipc::{MAX_LINE, PROTO, Push, Request};
use oxi_platform::ipc::{BindError, Client, Server, forward_open_settings};

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
    assert!(matches!(Server::bind(&n), Err(BindError::AlreadyRunning)));
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
    use oxi_platform::os::pipe::{owner_only_sddl, user_sid};

    #[test]
    fn sddl_admits_only_the_given_sid() {
        let sddl = owner_only_sddl("S-1-5-21-1-2-3-1001");
        assert_eq!(sddl, "D:P(A;;GA;;;S-1-5-21-1-2-3-1001)");
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
}
