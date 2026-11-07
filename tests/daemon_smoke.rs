//! Daemon smoke test: a portable copy of the exe starts, a second launch forwards
//! `OpenSettings` and exits 0, the first answers `Status`, `Shutdown` ends it with 0, and the
//! log exists and quotes no user text.
//!
//! Live (a real hook, tray icon and pipe), so it runs only with `OXI_DAEMON_TEST=1`. The copy
//! is portable (its data stays in a temp dir), its pipe carries a test suffix so it never meets
//! a real daemon, and its hotkey is Ctrl+Shift+F13, never the default Ctrl+Win. The test holds
//! the settings connection itself, so the second launch raises it instead of opening a window.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::thread;
use std::time::{Duration, Instant};

use oxi_core::config::ConfigFile;
use oxi_core::ipc::{PROTO, Push, Request};
use oxi_core::machine::Phase;
use oxi_core::paths::{Layout, PORTABLE_MARKER, portable_layout};
use oxi_platform::ipc::{Client, PIPE_SUFFIX_ENV};

const CHORD: [&str; 3] = ["Ctrl", "Shift", "F13"];
/// Sits in `dictionary.txt`: a log line that quotes user text would show it.
const CANARY: &str = "smokecanaryword";
const UP: Duration = Duration::from_secs(30);
const SECOND_EXIT: Duration = Duration::from_secs(2);
const READ: Duration = Duration::from_secs(5);
const SHUTDOWN: Duration = Duration::from_secs(15);

/// The temp install and the processes this test started; Drop kills only those.
struct Install {
    dir: PathBuf,
    exe: PathBuf,
    suffix: String,
    children: Vec<Child>,
}

impl Install {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("oxi-smoke-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("OxiYapper.exe");
        fs::copy(env!("CARGO_BIN_EXE_OxiYapper"), &exe).unwrap();
        fs::write(dir.join(PORTABLE_MARKER), "").unwrap();
        let layout = portable_layout(&dir);
        fs::create_dir_all(&layout.config_dir).unwrap();
        let config = format!("[hotkey]\nchord = {CHORD:?}\n");
        let parsed = ConfigFile::parse(&config).unwrap();
        assert_eq!(parsed.config.hotkey.chord, CHORD, "never the default chord");
        fs::write(layout.config_file(), config).unwrap();
        fs::write(layout.dictionary_file(), format!("{CANARY}\n")).unwrap();
        Self {
            dir,
            exe,
            suffix: format!("smoke-{}", std::process::id()),
            children: Vec::new(),
        }
    }

    fn layout(&self) -> Layout {
        portable_layout(&self.dir)
    }

    fn pipe(&self) -> String {
        let sid = oxi_platform::os::pipe::user_sid().unwrap();
        format!("{}-{}", oxi_core::ipc::pipe_name(&sid), self.suffix)
    }

    /// Starts the daemon copy; returns its index in `children`.
    fn launch(&mut self) -> usize {
        let child = Command::new(&self.exe)
            .env(PIPE_SUFFIX_ENV, &self.suffix)
            .spawn()
            .unwrap();
        self.children.push(child);
        self.children.len() - 1
    }

    /// The exit status of child `i` once it ends within `limit`.
    fn exit_within(&mut self, i: usize, limit: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.children[i].try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Install {
    fn drop(&mut self) {
        for child in &mut self.children {
            if matches!(child.try_wait(), Ok(None)) {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Connects once the daemon serves its pipe.
fn connect(pipe: &str) -> Client {
    let deadline = Instant::now() + UP;
    loop {
        match Client::connect(pipe) {
            Ok(conn) => return conn,
            Err(e) if Instant::now() >= deadline => panic!("daemon not up within {UP:?}: {e}"),
            Err(_) => thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// The next push that `pick` accepts; others are skipped.
fn next<T>(conn: &mut Client, mut pick: impl FnMut(Push) -> Option<T>) -> T {
    loop {
        let push = conn
            .recv::<Push>()
            .unwrap()
            .expect("daemon closed the connection");
        if let Some(found) = pick(push) {
            return found;
        }
    }
}

fn logs(layout: &Layout) -> Vec<(PathBuf, String)> {
    fs::read_dir(&layout.logs_dir)
        .map(|dir| {
            dir.map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|x| x == "log"))
                .map(|p| {
                    let text = fs::read_to_string(&p).unwrap();
                    (p, text)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn file_names(logs: &[(PathBuf, String)]) -> Vec<&Path> {
    logs.iter().map(|(p, _)| p.as_path()).collect()
}

#[test]
fn second_launch_forwards_status_answers_shutdown_exits_cleanly() {
    if std::env::var_os("OXI_DAEMON_TEST").is_none_or(|v| v != "1") {
        eprintln!("skipped: set OXI_DAEMON_TEST=1 to run the live daemon");
        return;
    }
    let mut install = Install::new();
    let pipe = install.pipe();
    let first = install.launch();
    let mut conn = connect(&pipe);
    assert!(
        install.exit_within(first, Duration::ZERO).is_none(),
        "the first instance is running"
    );
    conn.set_read_timeout(Some(READ)).unwrap();
    conn.send(&Request::Hello {
        proto: PROTO,
        version: env!("CARGO_PKG_VERSION").into(),
    })
    .unwrap();
    next(&mut conn, |p| {
        matches!(p, Push::HelloAck { .. }).then_some(())
    });

    let second = install.launch();
    let status = install.exit_within(second, SECOND_EXIT);
    assert!(
        status.is_some_and(|s| s.success()),
        "second launch: {status:?}"
    );
    next(&mut conn, |p| (p == Push::Raise).then_some(()));

    conn.send(&Request::Status).unwrap();
    let (phase, backend) = next(&mut conn, |p| match p {
        Push::Status { phase, backend, .. } => Some((phase, backend)),
        _ => None,
    });
    assert_eq!(phase, Phase::Idle);
    assert!(!backend.is_empty(), "Status has no backend");

    conn.send(&Request::Shutdown).unwrap();
    let status = install.exit_within(first, SHUTDOWN);
    assert!(status.is_some_and(|s| s.success()), "shutdown: {status:?}");

    let logs = logs(&install.layout());
    assert!(!logs.is_empty(), "no log file");
    let all: String = logs.iter().map(|(_, text)| text.as_str()).collect();
    assert!(all.contains("daemon started"), "{:?}", file_names(&logs));
    assert!(all.contains("daemon stopped"), "{:?}", file_names(&logs));
    assert!(!all.contains(CANARY), "the log quotes user text");
}
