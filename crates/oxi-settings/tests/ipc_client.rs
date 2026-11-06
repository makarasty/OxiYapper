//! The settings IPC client against a fake daemon (an `oxi_platform::ipc::Server` on a test
//! pipe). The fake reads every request like the real daemon: an unread connection stalls closes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::ipc::{PROTO, Push, Request};
use oxi_platform::ipc::{Conn, Server};
use oxi_settings::ipc::{Action, Cmd, Event, Ipc, Link, Notice, Options, notice, spawn_worker};

/// A name no other test or running daemon uses.
fn name(tag: &str) -> String {
    let id = format!("OxiYapper-test-{}-{tag}", std::process::id());
    if cfg!(windows) {
        format!(r"\\.\pipe\{id}")
    } else {
        std::env::temp_dir().join(id).to_string_lossy().into_owned()
    }
}

fn options(name: &str, grace: Duration, spawn: impl Fn() + Send + 'static) -> Options {
    Options {
        name: name.into(),
        spawn: Box::new(move || {
            spawn();
            Ok(())
        }),
        grace,
    }
}

/// Starts the worker; its events arrive on the receiver.
fn start(options: Options) -> (Sender<Cmd>, Receiver<Event>) {
    let (events, rx) = mpsc::channel();
    let tx = spawn_worker(options, move |e| {
        let _ = events.send(e);
    })
    .unwrap();
    (tx, rx)
}

/// The first event from now on that `want` accepts.
fn until(rx: &Receiver<Event>, want: impl Fn(&Event) -> bool) -> Event {
    let end = Instant::now() + Duration::from_secs(4);
    loop {
        let e = rx
            .recv_timeout(end.saturating_duration_since(Instant::now()))
            .expect("the expected event did not arrive");
        if want(&e) {
            return e;
        }
    }
}

fn link(l: Link) -> impl Fn(&Event) -> bool {
    move |e| *e == Event::Link(l)
}

/// Reads `Hello` and answers with `proto`.
fn ack(conn: &mut Conn, proto: u32) {
    let Some(Request::Hello { proto: p, .. }) = conn.recv().unwrap() else {
        panic!("expected Hello");
    };
    assert_eq!(p, PROTO);
    conn.send(&Push::HelloAck {
        proto,
        version: "0.0.0-test".into(),
    })
    .unwrap();
}

/// Reads until the client closes.
fn drain(conn: &mut Conn) {
    while let Ok(Some(_)) = conn.recv::<Request>() {}
}

#[test]
fn hello_then_pushes_and_requests() {
    let n = name("online");
    let server = Server::bind(&n).unwrap();
    let daemon = thread::spawn(move || {
        let mut conn = server.accept().unwrap();
        ack(&mut conn, PROTO);
        conn.send(&Push::Raise).unwrap();
        let seen = conn.recv::<Request>().unwrap();
        drain(&mut conn);
        seen
    });
    let (tx, events) = start(options(&n, Duration::ZERO, || {}));
    until(&events, link(Link::Online));
    let raise = until(&events, |e| matches!(e, Event::Push(_)));
    assert_eq!(raise, Event::Push(Push::Raise));
    assert_eq!(notice(&raise), None);
    tx.send(Cmd::Send(Request::Status)).unwrap();
    drop(tx);
    assert_eq!(daemon.join().unwrap(), Some(Request::Status));
}

#[test]
fn mismatch_shows_the_update_banner_and_restart_shuts_the_daemon_down() {
    let n = name("mismatch");
    let server = Server::bind(&n).unwrap();
    let daemon = thread::spawn(move || {
        let mut conn = server.accept().unwrap();
        ack(&mut conn, PROTO + 1);
        // The window must not send anything else to a daemon of another version.
        let first = conn.recv::<Request>().unwrap();
        (server, first)
    });
    let spawned = Arc::new(AtomicUsize::new(0));
    let count = spawned.clone();
    let (tx, events) = start(options(&n, Duration::from_millis(200), move || {
        count.fetch_add(1, Ordering::SeqCst);
    }));
    let shown = until(&events, |e| matches!(e, Event::Link(Link::Mismatch { .. })));
    assert_eq!(
        notice(&shown),
        Some(Notice {
            text: "Restart OxiYapper to finish the update".into(),
            action: Some(Action::Restart),
        })
    );
    tx.send(Cmd::Send(Request::Status)).unwrap();
    tx.send(Cmd::Restart).unwrap();
    let (_server, first) = daemon.join().unwrap();
    assert_eq!(first, Some(Request::Shutdown));
    until(&events, link(Link::Connecting));
    assert_eq!(spawned.load(Ordering::SeqCst), 1);
}

#[test]
fn config_error_push_names_the_line() {
    let n = name("config-error");
    let server = Server::bind(&n).unwrap();
    let daemon = thread::spawn(move || {
        let mut conn = server.accept().unwrap();
        ack(&mut conn, PROTO);
        conn.send(&Push::ConfigError {
            file: "config.toml".into(),
            line: 7,
            message: "expected `=`".into(),
        })
        .unwrap();
        drain(&mut conn);
    });
    let (tx, events) = start(options(&n, Duration::ZERO, || {}));
    let shown = until(&events, |e| matches!(e, Event::Push(_)));
    assert_eq!(
        notice(&shown),
        Some(Notice {
            text: "config.toml, line 7: expected `=`".into(),
            action: None,
        })
    );
    drop(tx);
    daemon.join().unwrap();
}

#[test]
fn dropped_connection_shows_not_running_and_reconnects() {
    let n = name("dropped");
    let server = Server::bind(&n).unwrap();
    let daemon = thread::spawn(move || {
        let mut first = server.accept().unwrap();
        ack(&mut first, PROTO);
        drop(first);
        let mut second = server.accept().unwrap();
        ack(&mut second, PROTO);
        drain(&mut second);
    });
    let (tx, events) = start(options(&n, Duration::ZERO, || {}));
    until(&events, link(Link::Online));
    let off = until(&events, link(Link::Offline));
    assert_eq!(notice(&off).unwrap().action, Some(Action::Start));
    until(&events, link(Link::Online));
    drop(tx);
    daemon.join().unwrap();
}

#[test]
fn no_daemon_is_started_once_then_offline_and_start_brings_it_up() {
    let n = name("absent");
    let spawned = Arc::new(AtomicUsize::new(0));
    let (count, name) = (spawned.clone(), n.clone());
    let hold = Arc::new(Mutex::new(None));
    let slot = hold.clone();
    let (tx, events) = start(options(&n, Duration::from_millis(300), move || {
        // The second start is the one that works: it binds the pipe like a real daemon.
        if count.fetch_add(1, Ordering::SeqCst) == 1 {
            let server = Server::bind(&name).unwrap();
            let daemon = thread::spawn(move || {
                let mut conn = server.accept().unwrap();
                ack(&mut conn, PROTO);
                drain(&mut conn);
            });
            *slot.lock().unwrap() = Some(daemon);
        }
    }));
    until(&events, link(Link::Connecting));
    let off = until(&events, link(Link::Offline));
    assert_eq!(spawned.load(Ordering::SeqCst), 1);
    assert_eq!(notice(&off).unwrap().action, Some(Action::Start));
    tx.send(Cmd::Start).unwrap();
    until(&events, link(Link::Online));
    assert_eq!(spawned.load(Ordering::SeqCst), 2);
    drop(tx);
    let daemon = hold.lock().unwrap().take().unwrap();
    daemon.join().unwrap();
}

#[test]
fn fake_ipc_records_requests_and_delivers_pushes() {
    let (ipc, sent) = Ipc::fake();
    let got = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = got.clone();
    ipc.subscribe(move |e| sink.borrow_mut().push(e.clone()));
    ipc.send(Request::ListDevices);
    assert_eq!(sent.try_recv(), Ok(Cmd::Send(Request::ListDevices)));
    ipc.deliver(&Event::Push(Push::Raise));
    ipc.deliver(&Event::Link(Link::Offline));
    assert_eq!(ipc.link(), Link::Offline);
    assert_eq!(
        *got.borrow(),
        [Event::Push(Push::Raise), Event::Link(Link::Offline)]
    );
}

#[test]
fn pushes_reach_subscribers_on_the_ui_thread() {
    i_slint_backend_testing::init_integration_test_with_system_time();
    let n = name("ui-thread");
    let server = Server::bind(&n).unwrap();
    let daemon = thread::spawn(move || {
        let mut conn = server.accept().unwrap();
        ack(&mut conn, PROTO);
        conn.send(&Push::Raise).unwrap();
        drain(&mut conn);
    });
    let ipc = Ipc::connect(options(&n, Duration::ZERO, || {})).unwrap();
    let ui = thread::current().id();
    let raised = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = raised.clone();
    ipc.subscribe(move |e| {
        assert_eq!(thread::current().id(), ui);
        if *e == Event::Push(Push::Raise) {
            flag.set(true);
            slint::quit_event_loop().unwrap();
        }
    });
    // A hung daemon must not hang the test.
    slint::Timer::single_shot(Duration::from_secs(4), || {
        slint::quit_event_loop().unwrap();
    });
    slint::run_event_loop().unwrap();
    assert!(raised.get());
    assert_eq!(ipc.link(), Link::Online);
    drop(ipc);
    daemon.join().unwrap();
}
