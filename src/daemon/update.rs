//! Update check and self-update (ARCHITECTURE section 12, docs/RELEASING.md "Assets and tags").
//!
//! One thread checks a minute after the start, then once a day, and on the System page's "Check
//! now", while `[system] update_check` is on. A newer release (`oxi_core::update`) with
//! `auto_update` on is downloaded next to the exe as `<exe>.new`: SHA256 from the release's
//! `SHA256SUMS`, then one `--version` run to see that it starts. The swap waits until no dictation
//! runs ([`Inbox::UpdateReady`], run by the coordinator): `<exe>` is renamed `<stem>.old.exe`
//! (Windows lets a running exe be renamed), `.new` takes its name, the daemon quits and a new one
//! starts from the same path ([`relaunch`]); the next start deletes the `.old`. A folder that cannot
//! be written, a release without the files, or any failure leaves the running exe in place and
//! offers the release page instead. Files beside the exe (the portable marker, `data\`, DLLs) are
//! never touched.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use oxi_core::config::{ConfigFile, System};
use oxi_core::ipc::Push;
use oxi_core::paths::part_path;
use oxi_core::update::{self, Release, Status, Version};

use super::cmd::Inbox;
use super::ipc::{Pushes, SystemHost};
use super::lock;
use crate::net::{self, DownloadError};

/// The first argument of a daemon started by [`relaunch`]: wait for the old process to end, then
/// say "Updated".
pub const AFTER_UPDATE_ARG: &str = "--after-update";
/// With [`AFTER_UPDATE_ARG`]: a settings window was open, open it again.
pub const OPEN_SETTINGS_ARG: &str = "--open-settings";

const FIRST_CHECK: Duration = Duration::from_secs(60);
/// With the test hook set, the end-to-end check should not wait a minute.
const FIRST_CHECK_FAKE: Duration = Duration::from_secs(5);
const EVERY: Duration = Duration::from_hours(24);
const MAX_LIST: u64 = 1 << 20;
const MAX_SUMS: u64 = 64 << 10;
const MAX_EXE: u64 = 512 << 20;
/// `--version` of a good exe answers in well under a second.
const PROBE_LIMIT: Duration = Duration::from_secs(20);
/// How long a relaunched daemon waits for the old one to end (its quit grace is 2 s).
const PARENT_LIMIT: Duration = Duration::from_secs(30);

/// What the update thread hears, through the io worker.
pub enum UpdateCmd {
    /// "Check now".
    Check,
    /// A settings window connected: it gets the current status.
    Subscribed,
}

/// The network and the start check, so the tests run without either.
pub struct Net {
    pub text: Box<TextFn>,
    pub file: Box<FileFn>,
    pub probe: Box<ProbeFn>,
}

type TextFn = dyn Fn(&str, u64) -> Result<String, DownloadError> + Send;
type ProbeFn = dyn Fn(&Path) -> anyhow::Result<()> + Send;
type FileFn =
    dyn Fn(&str, &Path, u64, &str, &mut dyn FnMut(u64, u64)) -> Result<(), DownloadError> + Send;

impl Net {
    fn real() -> Self {
        Self {
            text: Box::new(net::get_text),
            file: Box::new(net::download_file),
            probe: Box::new(probe),
        }
    }
}

/// The status, shared by the thread and the swap, and pushed to the window on every change.
#[derive(Clone)]
struct Reporter {
    status: Arc<Mutex<Status>>,
    pushes: Pushes,
}

impl Reporter {
    fn set(&self, status: Status) {
        lock(&self.status).clone_from(&status);
        self.pushes.send(Push::Update { status });
    }

    fn get(&self) -> Status {
        lock(&self.status).clone()
    }
}

/// Starts the update thread. Never joined: like the io worker, it ends with the process.
pub fn spawn(
    rx: Receiver<UpdateCmd>,
    inbox: Sender<Inbox>,
    pushes: Pushes,
    host: Arc<SystemHost>,
    config: PathBuf,
    updated: bool,
) -> io::Result<JoinHandle<()>> {
    let exe = std::env::current_exe()?;
    let fake = std::env::var(update::FAKE_VERSION_ENV).ok();
    let current = fake
        .as_deref()
        .and_then(Version::parse)
        .or_else(|| Version::parse(oxi_core::VERSION));
    // A test daemon (its own pipe) never goes online; a debug build never replaces itself unless
    // the test hook asks for it.
    let online = std::env::var_os(oxi_platform::ipc::PIPE_SUFFIX_ENV).is_none();
    let mut checker = Updater {
        net: Net::real(),
        exe,
        config,
        report: Reporter {
            status: Arc::new(Mutex::new(if updated {
                Status::Updated
            } else {
                Status::Unknown
            })),
            pushes,
        },
        current,
        may_install: !cfg!(debug_assertions) || fake.is_some(),
        updated,
    };
    if let Some(v) = &fake {
        tracing::info!(fake = %v, "update check takes a fake running version");
    }
    let first = if fake.is_some() {
        FIRST_CHECK_FAKE
    } else {
        FIRST_CHECK
    };
    thread::Builder::new().name("update".into()).spawn(move || {
        let mut next = Instant::now() + first;
        loop {
            let manual = match rx.recv_timeout(next.saturating_duration_since(Instant::now())) {
                Ok(UpdateCmd::Subscribed) => {
                    checker.report.set(checker.report.get());
                    continue;
                }
                Ok(UpdateCmd::Check) => true,
                Err(RecvTimeoutError::Timeout) => {
                    next = Instant::now() + EVERY;
                    false
                }
                Err(RecvTimeoutError::Disconnected) => return,
            };
            if !online {
                continue;
            }
            if let Some(staged) = checker.check(manual) {
                let apply = apply(
                    staged,
                    checker.report.clone(),
                    Arc::clone(&host),
                    inbox.clone(),
                );
                if inbox.send(Inbox::UpdateReady(Box::new(apply))).is_err() {
                    return;
                }
            }
        }
    })
}

/// A downloaded, checked exe waiting for its swap.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Staged {
    exe: PathBuf,
    new: PathBuf,
    version: String,
    page: String,
}

struct Updater {
    net: Net,
    /// The running exe's path.
    exe: PathBuf,
    config: PathBuf,
    report: Reporter,
    /// `None` when the running version does not parse: no check then.
    current: Option<Version>,
    may_install: bool,
    /// This start finished an update: "Up to date" reads "Updated to" until the next one.
    updated: bool,
}

impl Updater {
    /// `[system]` of `config.toml` as it is now; the defaults when it cannot be read.
    fn system(&self) -> System {
        fs::read_to_string(&self.config)
            .ok()
            .and_then(|text| ConfigFile::parse(&text).ok())
            .map(|file| file.config.system)
            .unwrap_or_default()
    }

    /// One check; `Some` when a newer exe is staged and waits for the swap.
    fn check(&mut self, manual: bool) -> Option<Staged> {
        if matches!(
            self.report.get(),
            Status::Ready { .. } | Status::Downloading { .. }
        ) {
            return None;
        }
        let system = self.system();
        if !system.update_check && !manual {
            self.report.set(Status::Off);
            return None;
        }
        let current = self.current.clone()?;
        self.report.set(Status::Checking);
        let found = (self.net.text)(update::RELEASES_URL, MAX_LIST).and_then(|text| {
            update::parse_releases(&text).map_err(|e| DownloadError::Network(io::Error::other(e)))
        });
        let releases = match found {
            Ok(releases) => releases,
            Err(e) => {
                // Offline or rate-limited: quiet, the next try is tomorrow (item 53).
                tracing::info!("update check failed: {e}");
                self.report.set(Status::Failed);
                return None;
            }
        };
        let Some(release) = update::newest(releases, &current) else {
            self.report.set(if self.updated {
                Status::Updated
            } else {
                Status::UpToDate
            });
            return None;
        };
        let version = release.version.to_string();
        tracing::info!(%version, running = %current, "update available");
        if !(system.auto_update && self.may_install) {
            self.offer(&release);
            return None;
        }
        match self.stage(&release) {
            Ok(new) => {
                self.report.set(Status::Ready {
                    version: version.clone(),
                });
                Some(Staged {
                    exe: self.exe.clone(),
                    new,
                    version,
                    page: release.page(),
                })
            }
            Err(e) => {
                tracing::warn!(%version, "update not downloaded: {e:#}");
                self.offer(&release);
                None
            }
        }
    }

    fn offer(&self, release: &Release) {
        self.report.set(Status::Available {
            version: release.version.to_string(),
            page: release.page(),
        });
    }

    /// Downloads and checks the release's exe as `<exe>.new`.
    fn stage(&self, release: &Release) -> anyhow::Result<PathBuf> {
        let name = update::exe_name(&release.version);
        let exe = release.exe.as_ref().context("the release has no exe")?;
        let sums = release
            .sums
            .as_ref()
            .context("the release has no SHA256SUMS")?;
        anyhow::ensure!(exe.size <= MAX_EXE, "the exe is {} bytes", exe.size);
        let sums = (self.net.text)(&sums.url, MAX_SUMS)?;
        let sha = update::checksum(&sums, &name).context("the exe is not in SHA256SUMS")?;
        let new = staged(&self.exe);
        let version = release.version.to_string();
        let mut shown = None;
        // A folder that cannot be written fails here, at the `.part`: the page is offered then.
        (self.net.file)(&exe.url, &new, exe.size, &sha, &mut |done, total| {
            let percent = u8::try_from(done.saturating_mul(100) / total.max(1)).unwrap_or(100);
            if shown != Some(percent) {
                shown = Some(percent);
                self.report.set(Status::Downloading {
                    version: version.clone(),
                    percent,
                });
            }
        })?;
        if let Err(e) = (self.net.probe)(&new) {
            let _ = fs::remove_file(&new);
            return Err(e.context("the new exe does not start"));
        }
        Ok(new)
    }
}

/// `<exe>.new`: the downloaded exe until the swap.
fn staged(exe: &Path) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(".new");
    exe.with_file_name(name)
}

/// `<stem>.old.exe`: the replaced exe, deleted at the next start.
fn retired(exe: &Path) -> PathBuf {
    let mut name = exe.file_stem().unwrap_or_default().to_os_string();
    name.push(".old.exe");
    exe.with_file_name(name)
}

/// Puts `new` where `exe` is; `exe` becomes `old`. On any failure `exe` is what it was.
fn swap(exe: &Path, new: &Path, old: &Path) -> io::Result<()> {
    match fs::remove_file(old) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    fs::rename(exe, old)?;
    if let Err(e) = fs::rename(new, exe) {
        if let Err(back) = fs::rename(old, exe) {
            tracing::error!("the replaced exe could not be put back: {back}");
        }
        return Err(e);
    }
    Ok(())
}

/// Undoes [`swap`]: the new exe goes back to `new`, `old` to `exe`. On a failure of the second
/// step the new exe is put back, so a working exe stays at `exe`.
fn unswap(exe: &Path, new: &Path, old: &Path) -> io::Result<()> {
    fs::rename(exe, new)?;
    fs::rename(old, exe).inspect_err(|_| {
        let _ = fs::rename(new, exe);
    })
}

/// Runs `exe --version` once: it must start, answer with the app name and exit 0.
fn probe(exe: &Path) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = Command::new(exe)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()?;
    let mut out = child.stdout.take().context("no stdout")?;
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = io::Read::read_to_string(&mut out, &mut text);
        let _ = tx.send(text);
    });
    // The pipe closes when the child exits, so the answer is the wait.
    let Ok(text) = rx.recv_timeout(PROBE_LIMIT) else {
        let _ = child.kill();
        anyhow::bail!("no answer in {PROBE_LIMIT:?}");
    };
    let status = child.wait()?;
    anyhow::ensure!(
        status.success() && text.starts_with(oxi_core::APP_NAME),
        "answered {status} {:?}",
        text.trim()
    );
    Ok(())
}

/// What the quit leaves to [`relaunch`].
struct Relaunch {
    exe: PathBuf,
    new: PathBuf,
    old: PathBuf,
    settings: bool,
}

static RELAUNCH: Mutex<Option<Relaunch>> = Mutex::new(None);

/// The swap, run by the coordinator once no dictation runs: on success it closes the settings
/// window and quits the daemon; [`relaunch`] starts the new exe after the quit.
fn apply(
    staged: Staged,
    report: Reporter,
    host: Arc<SystemHost>,
    inbox: Sender<Inbox>,
) -> impl FnOnce() + Send {
    move || {
        let Staged {
            exe,
            new,
            version,
            page,
        } = staged;
        let old = retired(&exe);
        if let Err(e) = swap(&exe, &new, &old) {
            tracing::warn!(%version, "update not installed: {e}");
            let _ = fs::remove_file(&new);
            report.set(Status::Available { version, page });
            return;
        }
        tracing::info!(%version, "update installed, restarting");
        *lock(&RELAUNCH) = Some(Relaunch {
            exe,
            new,
            old,
            settings: report.pushes.connected(),
        });
        host.close_settings();
        let _ = inbox.send(Inbox::Shutdown);
    }
}

/// After the daemon's quit: starts the swapped-in exe. If it cannot start, the old exe is put
/// back and started instead. The new process waits for this one to end before it takes the pipe.
pub fn relaunch() {
    let Some(r) = lock(&RELAUNCH).take() else {
        return;
    };
    if start(&r.exe, r.settings).is_ok() {
        return;
    }
    tracing::error!("the updated exe did not start, putting the old one back");
    match unswap(&r.exe, &r.new, &r.old) {
        // shortcut: the old exe says "Updated" too, rare (the new one passed its probe); add an
        // argument if it is ever seen.
        Ok(()) => drop(start(&r.exe, r.settings)),
        Err(e) => tracing::error!("the old exe could not be put back: {e}"),
    }
}

fn start(exe: &Path, settings: bool) -> io::Result<()> {
    let mut cmd = Command::new(exe);
    cmd.arg(AFTER_UPDATE_ARG)
        .stdin(Stdio::piped())
        // The test hook would make the new daemon update itself again, in a loop.
        .env_remove(update::FAKE_VERSION_ENV);
    if settings {
        cmd.arg(OPEN_SETTINGS_ARG);
    }
    let child = cmd
        .spawn()
        .inspect_err(|e| tracing::error!("new daemon not started: {e}"))?;
    // Its stdin closes when this process ends: that is what the new daemon waits for.
    std::mem::forget(child);
    Ok(())
}

/// In a daemon started by [`relaunch`]: returns once the old daemon ended (its end closes our
/// stdin), or after [`PARENT_LIMIT`].
pub fn wait_for_parent() {
    let (tx, rx) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("wait-parent".into())
        .spawn(move || {
            let _ = io::copy(&mut io::stdin(), &mut io::sink());
            let _ = tx.send(());
        });
    if reader.is_ok() {
        let _ = rx.recv_timeout(PARENT_LIMIT);
    }
}

/// At the start: the `.old` of the last update and a `.new` never swapped in are deleted (a
/// `.new` is downloaded again by the next check).
pub fn remove_leftovers() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let new = staged(&exe);
    for path in [retired(&exe), part_path(&new), new] {
        match fs::remove_file(&path) {
            Ok(()) => tracing::info!(file = ?path.file_name(), "update leftover removed"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(file = ?path.file_name(), "update leftover not removed: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::scratch::scratch_dir;

    fn dir(name: &str) -> PathBuf {
        scratch_dir(&format!("oxi-update-{}-{name}", std::process::id()))
    }

    #[test]
    fn names_beside_the_exe() {
        let exe = Path::new(r"C:\apps\OxiYapper\OxiYapper.exe");
        assert_eq!(
            staged(exe),
            Path::new(r"C:\apps\OxiYapper\OxiYapper.exe.new")
        );
        assert_eq!(
            retired(exe),
            Path::new(r"C:\apps\OxiYapper\OxiYapper.old.exe")
        );
        let single = Path::new(r"C:\d\OxiYapper-0.1.0-pre.12-windows-x64.exe");
        assert_eq!(
            retired(single),
            Path::new(r"C:\d\OxiYapper-0.1.0-pre.12-windows-x64.old.exe")
        );
    }

    /// The exe folder of the portable zip, with KB-sized fakes.
    fn folder(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let d = dir(name);
        let exe = d.join("OxiYapper.exe");
        fs::write(&exe, b"old exe").unwrap();
        fs::write(d.join("vcruntime140.dll"), b"dll").unwrap();
        fs::write(d.join("portable"), b"").unwrap();
        (d.clone(), exe.clone(), staged(&exe), retired(&exe))
    }

    #[test]
    fn swap_puts_the_new_exe_in_place_and_keeps_the_rest() {
        let (d, exe, new, old) = folder("swap");
        fs::write(&new, b"new exe").unwrap();
        fs::write(&old, b"stale old").unwrap();
        swap(&exe, &new, &old).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new exe");
        assert_eq!(fs::read(&old).unwrap(), b"old exe");
        assert!(!new.exists());
        assert_eq!(fs::read(d.join("vcruntime140.dll")).unwrap(), b"dll");
        assert!(d.join("portable").exists());

        unswap(&exe, &new, &old).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"old exe");
        assert_eq!(fs::read(&new).unwrap(), b"new exe");
        assert!(!old.exists());
    }

    #[test]
    fn a_failed_swap_leaves_the_running_exe() {
        let (_d, exe, new, old) = folder("rollback");
        // No `.new`: the second rename fails, the first is undone.
        assert!(swap(&exe, &new, &old).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old exe");
        assert!(!old.exists());
        // An `.old` that cannot be removed (a folder here) stops it before anything moves.
        fs::create_dir(&old).unwrap();
        fs::write(old.join("x"), b"x").unwrap();
        fs::write(&new, b"new exe").unwrap();
        assert!(swap(&exe, &new, &old).is_err());
        assert_eq!(fs::read(&exe).unwrap(), b"old exe");
        assert_eq!(fs::read(&new).unwrap(), b"new exe");
    }

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    fn releases(tag: &str) -> String {
        let base = format!("https://github.com/makarasty/OxiYapper/releases/download/v{tag}");
        format!(
            r#"[{{"tag_name": "v{tag}", "draft": false, "prerelease": true, "assets": [
                {{"name": "OxiYapper-{tag}-windows-x64.exe", "size": 8, "browser_download_url": "{base}/OxiYapper-{tag}-windows-x64.exe"}},
                {{"name": "SHA256SUMS", "size": 100, "browser_download_url": "{base}/SHA256SUMS"}}
            ]}}]"#
        )
    }

    struct Rig {
        updater: Updater,
        pushes: Receiver<Push>,
        dir: PathBuf,
    }

    /// An updater for `exe` in a temp folder whose network serves `list`, `SHA256SUMS` with `sums`
    /// and an exe through `file`.
    fn rig(name: &str, current: &str, list: String, sums: String, file: Box<FileFn>) -> Rig {
        let (dir, exe, _, _) = folder(name);
        let pushes = Pushes::default();
        let (_, rx) = pushes.claim(0);
        let updater = Updater {
            net: Net {
                text: Box::new(move |url, _| {
                    Ok(if url.ends_with("SHA256SUMS") {
                        sums.clone()
                    } else {
                        list.clone()
                    })
                }),
                file,
                probe: Box::new(|_| Ok(())),
            },
            exe,
            config: dir.join("config.toml"),
            report: Reporter {
                status: Arc::default(),
                pushes,
            },
            current: Version::parse(current),
            may_install: true,
            updated: false,
        };
        Rig {
            updater,
            pushes: rx,
            dir,
        }
    }

    fn good_file() -> Box<FileFn> {
        Box::new(|_, dest, size, sha, progress| {
            assert_eq!(sha, SHA);
            progress(size / 2, size);
            progress(size, size);
            fs::write(dest, b"new exe").unwrap();
            Ok(())
        })
    }

    fn sums(tag: &str) -> String {
        format!("{SHA}  OxiYapper-{tag}-windows-x64.exe\n")
    }

    fn statuses(rx: &Receiver<Push>) -> Vec<Status> {
        rx.try_iter()
            .filter_map(|p| match p {
                Push::Update { status } => Some(status),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_newer_release_is_downloaded_checked_and_staged() {
        let mut r = rig(
            "stage",
            "0.1.0-pre.12",
            releases("0.1.0-pre.13"),
            sums("0.1.0-pre.13"),
            good_file(),
        );
        let staged = r.updater.check(false).expect("staged");
        assert_eq!(staged.version, "0.1.0-pre.13");
        assert_eq!(fs::read(&staged.new).unwrap(), b"new exe");
        assert_eq!(
            fs::read(&staged.exe).unwrap(),
            b"old exe",
            "nothing swapped yet"
        );
        let seen = statuses(&r.pushes);
        assert_eq!(seen.first(), Some(&Status::Checking));
        assert!(seen.contains(&Status::Downloading {
            version: "0.1.0-pre.13".into(),
            percent: 50
        }));
        assert_eq!(
            seen.last(),
            Some(&Status::Ready {
                version: "0.1.0-pre.13".into()
            })
        );
        // Waiting for the swap: a second check does nothing.
        assert!(r.updater.check(true).is_none());
    }

    #[test]
    fn the_same_or_an_older_release_is_up_to_date() {
        let mut r = rig(
            "same",
            "0.1.0-pre.13",
            releases("0.1.0-pre.13"),
            String::new(),
            good_file(),
        );
        assert!(r.updater.check(false).is_none());
        assert_eq!(statuses(&r.pushes).last(), Some(&Status::UpToDate));
        r.updater.updated = true;
        assert!(r.updater.check(false).is_none());
        assert_eq!(statuses(&r.pushes).last(), Some(&Status::Updated));
    }

    #[test]
    fn an_exe_missing_from_the_sums_is_offered_not_downloaded() {
        let file: Box<FileFn> = Box::new(|_, _, _, _, _| panic!("no download without a checksum"));
        let mut r = rig(
            "nosum",
            "0.1.0-pre.12",
            releases("0.1.0-pre.13"),
            sums("0.1.0-pre.99"),
            file,
        );
        assert!(r.updater.check(false).is_none());
        assert_eq!(
            statuses(&r.pushes).last(),
            Some(&Status::Available {
                version: "0.1.0-pre.13".into(),
                page: "https://github.com/makarasty/OxiYapper/releases/tag/v0.1.0-pre.13".into()
            })
        );
        assert!(!staged(&r.updater.exe).exists());
    }

    #[test]
    fn a_checksum_mismatch_or_an_unwritable_folder_leaves_the_exe_and_offers_the_page() {
        for (name, err) in [
            (
                "mismatch",
                (|| DownloadError::Hash {
                    expected: SHA.into(),
                    actual: "b".repeat(64),
                }) as fn() -> DownloadError,
            ),
            ("readonly", || {
                DownloadError::Io(io::ErrorKind::PermissionDenied.into())
            }),
        ] {
            let file: Box<FileFn> = Box::new(move |_, _, _, _, _| Err(err()));
            let mut r = rig(
                name,
                "0.1.0-pre.12",
                releases("0.1.0-pre.13"),
                sums("0.1.0-pre.13"),
                file,
            );
            assert!(r.updater.check(false).is_none(), "{name}");
            assert!(
                matches!(statuses(&r.pushes).last(), Some(Status::Available { .. })),
                "{name}"
            );
            assert_eq!(fs::read(&r.updater.exe).unwrap(), b"old exe");
        }
    }

    #[test]
    fn an_exe_that_does_not_start_is_deleted_and_offered() {
        let mut r = rig(
            "probe",
            "0.1.0-pre.12",
            releases("0.1.0-pre.13"),
            sums("0.1.0-pre.13"),
            good_file(),
        );
        r.updater.net.probe = Box::new(|_| anyhow::bail!("exit 1"));
        assert!(r.updater.check(false).is_none());
        assert!(!staged(&r.updater.exe).exists());
        assert!(matches!(
            statuses(&r.pushes).last(),
            Some(Status::Available { .. })
        ));
    }

    #[test]
    fn automatic_install_off_or_checks_off_follow_the_config() {
        let mut r = rig(
            "config",
            "0.1.0-pre.12",
            releases("0.1.0-pre.13"),
            sums("0.1.0-pre.13"),
            good_file(),
        );
        fs::write(r.dir.join("config.toml"), "[system]\nauto_update = false\n").unwrap();
        assert!(r.updater.check(false).is_none());
        assert!(matches!(
            statuses(&r.pushes).last(),
            Some(Status::Available { .. })
        ));
        fs::write(
            r.dir.join("config.toml"),
            "[system]\nupdate_check = false\n",
        )
        .unwrap();
        assert!(r.updater.check(false).is_none());
        assert_eq!(statuses(&r.pushes), [Status::Off]);
        // "Check now" still checks.
        assert!(r.updater.check(true).is_some());
    }

    #[test]
    fn no_answer_is_a_quiet_failure() {
        let mut r = rig(
            "offline",
            "0.1.0",
            String::new(),
            String::new(),
            good_file(),
        );
        r.updater.net.text = Box::new(|_, _| Err(DownloadError::Http(403)));
        assert!(r.updater.check(false).is_none());
        assert_eq!(statuses(&r.pushes), [Status::Checking, Status::Failed]);
    }

    #[test]
    fn the_probe_refuses_a_file_that_is_no_program() {
        let d = dir("probe-real");
        let fake = d.join("fake.exe");
        fs::write(&fake, b"not a program").unwrap();
        assert!(probe(&fake).is_err());
    }
}
