//! IPC server thread (ARCHITECTURE section 5).
//!
//! One thread accepts connections; each connection gets one thread of its own, so a client
//! that stops reading stalls only itself. The first message decides a connection's role:
//! `OpenSettings` is acted on and the connection closed (a second daemon launch waits for that
//! close); `Hello` makes it *the* settings connection; anything else closes it. A new `Hello`
//! replaces the old settings connection.
//!
//! Nothing outside the connection thread writes to a socket. Other threads (io progress, hook
//! keys, the watcher) hand a [`Push`] to [`Pushes::send`], which only queues it and never
//! blocks; the connection thread writes the queue with a write timeout. A full queue or a
//! failed write ends that connection. The coordinator thread is never asked to wait for a client.
//!
//! Dictated text never reaches this module; log lines carry kinds and counts only.

use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context;
use oxi_core::history::HistoryId;
use oxi_core::hotkey::Key;
use oxi_core::ipc::{
    FileFailure, FileResult, HeldText, HistoryItem, HistoryOp, Observed, PROTO, Push, Request,
    Spoken, TestFailure, TestOutcome, Words, WordsFailure, WordsResult, attention,
};
use oxi_core::machine::ModelFailure;
use oxi_core::models::{self, ModelStatus};
use oxi_core::tuning::{SettingsSet, append_pcm};
use oxi_core::types::Language;
use oxi_platform::ipc::{Conn, Error, Server, default_name, forward_open_settings};

use super::cmd::{Inbox, IoCmd, IpcAsk, StatusReport, TestJob};
use super::history::{self, Context as HistoryContext};
use super::lock;
use super::whisper::NO_BACKEND;

/// Concurrent connections; more are closed unanswered.
const MAX_CONNS: usize = 8;
/// Pushes waiting for one connection's thread.
const QUEUE: usize = 64;
/// How long a connection thread waits for a message before it looks at its queue again, so
/// this is also the delay of a push.
const POLL: Duration = Duration::from_millis(50);
/// A connection that sends no `Hello` within this long is closed.
const FIRST_MESSAGE: Duration = Duration::from_secs(5);
/// A write the client does not read within this long ends the connection.
const WRITE_LIMIT: Duration = Duration::from_secs(2);
/// How long a `Status` waits for the coordinator before it is dropped.
const ASK_LIMIT: Duration = Duration::from_secs(2);
/// `Status` asks waiting for the coordinator, per connection; more are ignored.
const MAX_ASKS: usize = 4;
/// How often capture checks that the settings window still has the focus, on a thread of its
/// own: a stalled send or a slow ask on the connection must not delay turning capture off.
const FOCUS_POLL: Duration = Duration::from_millis(20);

/// Longest `History` page: items, and bytes of text and names (the line limit is 1 MiB).
const HISTORY_PAGE_ITEMS: usize = 200;
const HISTORY_PAGE_BYTES: usize = 512 * 1024;

/// What the daemon checks on disk and on the system before each `Attention` push.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Probe {
    /// The chosen registry model's files; `None` for a custom file or an unknown id.
    pub model: Option<ModelStatus>,
    pub mic_missing: bool,
}

/// What the server needs from the OS and the rest of the daemon; fake in the tests.
pub trait Host: Send + Sync + 'static {
    /// Hotkey capture on or off; `swallow` while the settings window is foreground.
    fn set_capture(&self, on: bool, swallow: bool);
    /// Is the foreground window one of process `client`, the settings window that holds the
    /// connection (never one of this daemon's own windows, such as the tray menu).
    fn settings_foreground(&self, client: u32) -> bool;
    /// Lets the windows of process `client` take the foreground, before a `Raise`.
    fn allow_foreground(&self, client: u32) -> io::Result<()>;
    /// The `Devices` push.
    fn devices(&self) -> Push;
    /// The `Stats` push.
    fn stats(&self) -> Push;
    /// Start a settings window.
    fn open_settings(&self) -> io::Result<()>;
    /// What the `Attention` push needs: the files of model `model` and whether input device
    /// `device` (empty is the system default) is there.
    fn probe(&self, model: &str, device: &str) -> Probe;
    /// One `History` push: the recordings older than `older_than`, newest first.
    fn history_list(&self, older_than: Option<&str>) -> Push;
    /// One `HistoryFound` push (carrying `seq`): the recordings that pass `filter` and hold every
    /// word of `query`, older than `older_than`, newest first. `cache` is the connection's: what
    /// an earlier query read is not read again.
    fn history_search(
        &self,
        cache: &mut history::SearchCache,
        seq: u32,
        query: &str,
        filter: &oxi_core::history::Filter,
        older_than: Option<&str>,
    ) -> Push;
    /// What a file run transcribes with (the configured model and audio steps); `None` when
    /// History saving is off, so the result would have nowhere to go.
    fn file_set(&self) -> Option<SettingsSet>;
    /// Decodes a file for a file run to 16 kHz mono; `stop` set ends it with `Stopped`. The system
    /// host decodes in a child process: a file that crashes the decoder must not end the daemon.
    fn decode_file(
        &self,
        path: &std::path::Path,
        stop: &AtomicBool,
    ) -> Result<Vec<f32>, oxi_platform::decode::DecodeError>;
    /// Saves a transcribed file (`samples`: its 16 kHz mono copy) as a History record.
    fn history_save_file(
        &self,
        samples: &[f32],
        text: &str,
        language: Language,
    ) -> io::Result<HistoryItem>;
    /// The audio of recording `id`, 16 kHz mono, for a new transcript.
    fn history_audio(&self, id: &HistoryId) -> io::Result<Vec<f32>>;
    /// Did the app already process the audio of recording `id` (gain, denoise, trim, normalise)?
    /// `false` for a recovered raw one (`interrupted`, `failed`) and for an unknown id.
    fn history_processed(&self, id: &HistoryId) -> bool;
    /// Stores `text` as the transcript of recording `id`, made by `set` in `language`; `words`
    /// are the run's words (kept only when the record has no timed ones).
    fn history_set_text(
        &self,
        id: &HistoryId,
        text: &str,
        set: &SettingsSet,
        language: Language,
        words: Vec<oxi_core::history::Word>,
    ) -> io::Result<()>;
    /// The stored words of recording `id` and how their times were made (`None`: they have none).
    fn history_words(
        &self,
        id: &HistoryId,
    ) -> io::Result<(
        Vec<oxi_core::history::Word>,
        Option<oxi_core::history::Timings>,
    )>;
    /// Replaces the transcript of recording `id` with the user's edit; the run stays.
    fn history_edit(&self, id: &HistoryId, text: &str) -> io::Result<()>;
    /// Swaps the transcript of recording `id` with its other version.
    fn history_restore(&self, id: &HistoryId) -> io::Result<()>;
    fn history_delete(&self, id: &HistoryId) -> io::Result<()>;
    /// Does this host take the daemon's announcements ([`announce`])? Only the real one does, so
    /// a thread under test never reaches another test's window.
    fn announces(&self) -> bool {
        false
    }
}

/// Who hears what the daemon announces without being asked (a saved recording, a stats write,
/// a file the watcher saw, a status change): the settings connection of the real daemon.
static BUS: Mutex<Option<Pushes>> = Mutex::new(None);

/// Is a settings window connected? Cheap: a worker asks before it builds a push.
pub fn connected() -> bool {
    lock(&BUS).as_ref().is_some_and(Pushes::connected)
}

/// Queues `push` for the settings window, if one is connected and a bus was set.
pub fn announce(push: Push) {
    announce_with(|| push);
}

/// [`announce`] with the push built only when a window is there to read it.
pub fn announce_with(make: impl FnOnce() -> Push) {
    let bus = lock(&BUS).clone();
    if let Some(pushes) = bus
        && pushes.connected()
    {
        pushes.send(make());
    }
}

/// `Status` and `Attention` for the window, after the daemon's own state changed. Only leaves
/// the report: the connection's thread probes the disk and the microphones and sends both, so
/// the caller (the coordinator) never waits on a device enumeration.
pub fn announce_status(report: &StatusReport) {
    if let Some(pushes) = lock(&BUS).as_ref() {
        pushes.post_status(report.clone());
    }
}

/// Queue to the settings connection, shared by every thread that pushes. Cheap to clone.
///
/// shortcut: the record a word-times run is for ([`Pushes::with_words_for`]) and the switch that
/// stops that run ride here, not on `TestJob`. Upgrade: move them to `TestJob` beside `origin`.
#[derive(Clone, Default)]
pub struct Pushes(Arc<Mutex<Slot>>, Option<(HistoryId, Arc<AtomicBool>)>);

/// Where the audio of a recognition test came from.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Origin {
    /// The Test page's microphone clip: raw, and silence stays silence.
    #[default]
    Mic,
    /// A History recording the app already processed (gain, denoise, trim, normalise) when it
    /// saved it.
    History,
    /// A recovered History recording: raw microphone audio.
    HistoryRaw,
}

impl Origin {
    /// Is the audio already processed, so the worker must not process it again?
    #[must_use]
    pub fn is_stored_audio(self) -> bool {
        self == Self::History
    }

    /// Does the audio come from History, processed or not?
    #[must_use]
    pub fn is_from_history(self) -> bool {
        self != Self::Mic
    }
}

#[derive(Default)]
struct Slot {
    next_id: u64,
    current: Option<(u64, u32, SyncSender<Push>)>,
    /// A status for the settings connection's thread to send with its `Attention`.
    status: Option<StatusReport>,
}

impl Pushes {
    /// This queue for the word-times run of recording `id`: the whisper worker makes the times
    /// of its audio and stores them in that record.
    #[must_use]
    pub fn with_words_for(&self, id: &HistoryId) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        Self(Arc::clone(&self.0), Some((id.clone(), stop)))
    }

    /// The recording a word-times run replying here is for.
    pub fn words_for(&self) -> Option<&HistoryId> {
        self.1.as_ref().map(|(id, _)| id)
    }

    /// The switch that stops the word-times run replying here: set, the run ends as soon as the
    /// worker looks (before it starts, or at whisper's next cancel check) and answers `Busy`.
    pub fn words_stop(&self) -> Option<&Arc<AtomicBool>> {
        self.1.as_ref().map(|(_, stop)| stop)
    }

    /// Was the word-times run replying here stopped?
    pub fn words_stopped(&self) -> bool {
        self.words_stop().is_some_and(|s| s.load(Ordering::Acquire))
    }

    /// Queues `push` for the settings connection. `false` when there is none, or when its
    /// queue is full (it is then disconnected, so a stuck window cannot grow memory).
    pub fn send(&self, push: Push) -> bool {
        let mut slot = lock(&self.0);
        let Some((_, _, tx)) = &slot.current else {
            return false;
        };
        match tx.try_send(push) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                tracing::warn!("settings window not reading: connection dropped");
                slot.current = None;
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                slot.current = None;
                false
            }
        }
    }

    /// A new settings connection; the old one, if any, ends.
    pub(super) fn claim(&self, client: u32) -> (u64, Receiver<Push>) {
        let (tx, rx) = mpsc::sync_channel(QUEUE);
        let mut slot = lock(&self.0);
        slot.next_id += 1;
        let id = slot.next_id;
        slot.current = Some((id, client, tx));
        slot.status = None;
        (id, rx)
    }

    /// Leaves `report` for the settings connection's thread, which sends it with its
    /// `Attention`; a newer one replaces one not sent yet. Nothing without a connection.
    pub fn post_status(&self, report: StatusReport) {
        let mut slot = lock(&self.0);
        if slot.current.is_some() {
            slot.status = Some(report);
        }
    }

    /// The status posted for connection `id`, while it is the settings connection.
    fn take_status(&self, id: u64) -> Option<StatusReport> {
        let mut slot = lock(&self.0);
        if slot.current.as_ref().is_some_and(|(cur, _, _)| *cur == id) {
            slot.status.take()
        } else {
            None
        }
    }

    /// Is a settings connection up?
    pub fn connected(&self) -> bool {
        lock(&self.0).current.is_some()
    }

    /// Process id of the settings client, if one is connected.
    fn client(&self) -> Option<u32> {
        lock(&self.0).current.as_ref().map(|&(_, client, _)| client)
    }

    fn release(&self, id: u64) {
        let mut slot = lock(&self.0);
        if slot.current.as_ref().is_some_and(|(cur, _, _)| *cur == id) {
            slot.current = None;
        }
    }
}

/// Outcome of [`bind`].
pub enum Start {
    /// This process is the daemon: serve on it.
    Serve(Server),
    /// Another daemon runs; it was asked to open settings and this process should exit.
    Forwarded,
}

/// Binds this user's socket. An error here is a refusal to start: its text says why and
/// goes to the log and to stderr; the process must not exit silently.
pub fn bind() -> anyhow::Result<Start> {
    bind_at(&default_name().context("no IPC socket name")?)
}

fn bind_at(name: &str) -> anyhow::Result<Start> {
    match Server::bind(name) {
        Ok(server) => Ok(Start::Serve(server)),
        Err(Error::AlreadyRunning) => {
            // This launch has the user's input; without passing it on, the settings window the
            // daemon raises or starts stays behind the app in front.
            if let Err(e) = oxi_platform::os::foreground::allow_any() {
                tracing::debug!(kind = ?e.kind(), "foreground not passed on");
            }
            forward_open_settings(name).map_err(refusal)?;
            Ok(Start::Forwarded)
        }
        Err(e) => Err(refusal(e)),
    }
}

/// The plain message for an error of binding or forwarding.
fn refusal(e: Error) -> anyhow::Error {
    let msg = match e {
        Error::AlreadyRunning => "another OxiYapper is already running".to_owned(),
        Error::Busy => format!(
            "{}; end it in Task Manager and start again",
            oxi_core::strings::DAEMON_BUSY
        ),
        Error::ForeignServer => {
            "cannot start: the OxiYapper IPC pipe is held by another account or program".to_owned()
        }
        Error::Untrusted => {
            "cannot start: the OxiYapper IPC pipe is held by an untrusted (low-integrity) program"
                .to_owned()
        }
        Error::Io(e) if e.kind() == io::ErrorKind::TimedOut => {
            "OxiYapper is running but did not answer in time".to_owned()
        }
        Error::Io(e) => format!("IPC failed: {e}"),
    };
    tracing::error!("{msg}");
    anyhow::anyhow!(msg)
}

/// Starts the accept thread and the thread that turns captured keys into `Key` pushes.
/// Neither ends before the process does.
pub fn spawn(
    server: Server,
    inbox: Sender<Inbox>,
    io: Sender<IoCmd>,
    host: Arc<dyn Host>,
    pushes: Pushes,
    keys: Receiver<(Key, bool)>,
) -> io::Result<JoinHandle<()>> {
    if host.announces() {
        *lock(&BUS) = Some(pushes.clone());
    }
    let key_pushes = pushes.clone();
    thread::Builder::new()
        .name("ipc-keys".into())
        .spawn(move || {
            for (code, down) in keys {
                key_pushes.send(Push::Key { code, down });
            }
        })?;
    let ctx = Arc::new(Ctx {
        inbox,
        io,
        host,
        pushes,
    });
    thread::Builder::new()
        .name("ipc".into())
        .spawn(move || accept_loop(&server, &ctx))
}

fn accept_loop(server: &Server, ctx: &Arc<Ctx>) {
    let live = Arc::new(AtomicUsize::new(0));
    loop {
        let conn = match server.accept() {
            Ok(conn) => conn,
            Err(e) => {
                tracing::warn!(kind = ?e.kind(), "ipc accept failed");
                thread::sleep(POLL); // an error that repeats must not spin
                continue;
            }
        };
        if live.fetch_add(1, Ordering::AcqRel) >= MAX_CONNS {
            live.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!("ipc connection refused: {MAX_CONNS} are open");
            continue; // dropping `conn` closes it
        }
        let (ctx, slot) = (ctx.clone(), Live(live.clone()));
        let spawned = thread::Builder::new()
            .name("ipc-conn".into())
            .spawn(move || {
                let _slot = slot;
                ctx.connection(conn);
            });
        if let Err(e) = spawned {
            tracing::warn!(kind = ?e.kind(), "ipc connection thread not started");
        }
    }
}

/// Counts a connection while it lives.
struct Live(Arc<AtomicUsize>);

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Ctx {
    inbox: Sender<Inbox>,
    io: Sender<IoCmd>,
    host: Arc<dyn Host>,
    pushes: Pushes,
}

/// What one connection has become.
#[derive(Default)]
struct Session {
    /// Set by `Hello`: this is the settings connection.
    settings: Option<(u64, Receiver<Push>)>,
    /// Process id of the settings client; 0 when unknown (capture is then refused).
    client: u32,
    /// Hotkey capture this connection switched on.
    capture: Option<FocusGuard>,
    /// `Status` asks the coordinator has not answered yet, oldest first.
    asks: VecDeque<(Receiver<StatusReport>, Instant)>,
    /// The recognition test's recording: in memory only, dropped with this connection.
    test_audio: Option<Vec<f32>>,
    /// The `id` of the run waiting for its `TestResult`; each run holds a copy of the recording.
    test_running: Option<u32>,
    /// A `TestTranscribe` was sent, so the worker may hold a model for it.
    tested: bool,
    /// The word-times run in hand and its stop switch: any other run, a cancel, or the window
    /// closing stops it (arch-plan 8.3), so it never holds the slot against them.
    words: Option<(u32, Arc<AtomicBool>)>,
    /// The file run in hand and its stop switch.
    file: Option<(u32, Arc<AtomicBool>)>,
    /// What this window's searches have read.
    search: history::SearchCache,
}

impl Session {
    /// `HistoryWordsCancel`: stops run `run` when it is the one in hand; always `true` (the
    /// connection goes on).
    fn cancel_words(&mut self, run: u32) -> bool {
        if self.words.as_ref().is_some_and(|(r, _)| *r == run) {
            self.stop_words();
        }
        true
    }

    /// Stops the word-times run in hand, if any, and frees the slot it held; its `Busy` answer
    /// comes later and frees nothing.
    fn stop_words(&mut self) {
        if let Some((run, stop)) = self.words.take() {
            stop.store(true, Ordering::Release);
            if self.test_running == Some(run) {
                self.test_running = None;
            }
        }
    }
}

/// Turns capture off the moment the settings window loses the foreground.
struct FocusGuard {
    stop: Arc<AtomicBool>,
    /// The guard turned capture off itself.
    lost: Arc<AtomicBool>,
}

impl FocusGuard {
    fn start(host: &Arc<dyn Host>, client: u32) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let lost = Arc::new(AtomicBool::new(false));
        let (host, flags) = (host.clone(), (stop.clone(), lost.clone()));
        thread::Builder::new()
            .name("ipc-focus".into())
            .spawn(move || {
                let (stop, lost) = flags;
                loop {
                    thread::sleep(FOCUS_POLL);
                    if stop.load(Ordering::Acquire) {
                        return;
                    }
                    if !host.settings_foreground(client) {
                        host.set_capture(false, false);
                        lost.store(true, Ordering::Release);
                        return;
                    }
                }
            })?;
        Ok(Self { stop, lost })
    }

    fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }
}

impl Ctx {
    fn connection(&self, mut conn: Conn) {
        let mut session = Session::default();
        if conn.set_write_timeout(Some(WRITE_LIMIT)).is_ok()
            && conn.set_read_timeout(Some(POLL)).is_ok()
        {
            self.serve(&mut conn, &mut session);
        }
        self.end_capture(&mut session);
        session.stop_words();
        // The run's pushes would reach the next window, whose run ids start over.
        if let Some((_, stop)) = session.file.take() {
            stop.store(true, Ordering::Release);
        }
        if session.tested {
            let _ = self.inbox.send(Inbox::Ipc(IpcAsk::TestDone));
        }
        if let Some((id, _)) = session.settings {
            self.pushes.release(id);
        }
    }

    fn serve(&self, conn: &mut Conn, session: &mut Session) {
        let opened = Instant::now();
        loop {
            if !flush(conn, session) {
                return;
            }
            if session
                .capture
                .as_ref()
                .is_some_and(|g| g.lost.load(Ordering::Acquire))
            {
                session.capture = None;
            }
            if !answer_status(conn, session, &*self.host) {
                return;
            }
            if let Some((id, _)) = session.settings
                && let Some(report) = self.pushes.take_status(id)
                && !send_status(conn, report, &*self.host)
            {
                return;
            }
            match conn.recv::<Request>() {
                // A client that connects and closes (the probe of `Server::bind`) reads as `None`.
                Ok(None) => return,
                Ok(Some(req)) => {
                    if !self.handle(req, conn, session) {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                    if session.settings.is_none() && opened.elapsed() > FIRST_MESSAGE {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!(kind = ?e.kind(), "ipc connection ended");
                    return;
                }
            }
        }
    }

    /// Acts on `req`; `false` closes the connection.
    #[allow(clippy::too_many_lines, reason = "one arm per request")]
    fn handle(&self, req: Request, conn: &mut Conn, session: &mut Session) -> bool {
        let first = session.settings.is_none();
        match req {
            Request::OpenSettings => self.open_settings(conn, first),
            Request::Hello { .. } => {
                session.client = conn.peer_pid().unwrap_or(0);
                if session.settings.is_none() {
                    session.settings = Some(self.pushes.claim(session.client));
                    // A window that opens during a download shows it (no second DownloadModel).
                    let _ = self.io.send(IoCmd::Subscribed);
                }
                conn.send(&hello_ack()).is_ok()
            }
            // Only the settings connection may ask for anything else.
            _ if first => false,
            Request::Status => self.status(session),
            Request::ListDevices => conn.send(&self.host.devices()).is_ok(),
            Request::Stats => conn.send(&self.host.stats()).is_ok(),
            Request::DownloadModel { id } => self.io.send(IoCmd::Download { id }).is_ok(),
            Request::CancelDownload { id } => self.io.send(IoCmd::CancelDownload { id }).is_ok(),
            Request::CaptureHotkey { on } => {
                self.capture(on, session);
                true
            }
            Request::PreviewOverlay { position } => self
                .inbox
                .send(Inbox::Ipc(IpcAsk::PreviewOverlay(position)))
                .is_ok(),
            Request::PreviewSound {
                choice,
                volume_percent,
            } => self
                .inbox
                .send(Inbox::Ipc(IpcAsk::PreviewSound {
                    choice,
                    volume_percent,
                }))
                .is_ok(),
            Request::TestAudio { reset, pcm } => {
                if !append_pcm(&mut session.test_audio, reset, &pcm) {
                    tracing::warn!("recognition test audio refused");
                }
                true
            }
            Request::TestTranscribe { id, set, language } => {
                session.stop_words();
                if session.test_running.is_some() {
                    return conn.send(&test_failed(id, TestFailure::Busy)).is_ok();
                }
                let samples = session.test_audio.clone().unwrap_or_default();
                self.start_test(session, id, set, language, samples, Origin::Mic)
            }
            Request::HistoryList { older_than } => conn
                .send(&self.host.history_list(older_than.as_deref()))
                .is_ok(),
            Request::HistorySearch {
                seq,
                query: Spoken(query),
                day,
                min_ms,
                max_ms,
                older_than,
            } => {
                let filter = oxi_core::history::Filter {
                    day: day.as_deref().and_then(oxi_core::history::day_dir),
                    min_ms,
                    max_ms,
                };
                let found = self.host.history_search(
                    &mut session.search,
                    seq,
                    &query,
                    &filter,
                    older_than.as_deref(),
                );
                conn.send(&found).is_ok()
            }
            Request::TranscribeFile {
                run,
                path,
                language,
            } => self.transcribe_file(conn, session, run, path, language),
            Request::TranscribeFileCancel { run } => {
                if let Some((_, stop)) = session.file.as_ref().filter(|(r, _)| *r == run) {
                    stop.store(true, Ordering::Release);
                }
                true
            }
            Request::HistoryRetranscribe {
                run,
                id,
                set,
                language,
            } => self.retranscribe(conn, session, run, &id, set, language),
            Request::HistorySetText {
                id,
                text: Spoken(text),
                set,
                language,
                words: Words(words),
            } => {
                let done = self
                    .host
                    .history_set_text(&id, &text, &set, language, words);
                history_result(conn, id, HistoryOp::SetText, done)
            }
            Request::HistoryWords { run, id } => self.words(conn, session, run, &id),
            Request::HistoryWordsCancel { run } => session.cancel_words(run),
            Request::HistoryEdit {
                id,
                text: Spoken(text),
            } => {
                let done = self.host.history_edit(&id, &text);
                history_result(conn, id, HistoryOp::Edit, done)
            }
            Request::HistoryRestore { id } => {
                let done = self.host.history_restore(&id);
                history_result(conn, id, HistoryOp::Restore, done)
            }
            Request::HistoryDelete { id } => {
                let done = self.host.history_delete(&id);
                history_result(conn, id, HistoryOp::Delete, done)
            }
            // The coordinator refuses it while a job runs; the whisper worker unloads and deletes,
            // and the answer comes back as a push.
            Request::DeleteModel { id } => self
                .inbox
                .send(Inbox::Ipc(IpcAsk::DeleteModel {
                    id,
                    reply: self.pushes.clone(),
                }))
                .is_ok(),
            Request::Shutdown => {
                let _ = self.inbox.send(Inbox::Shutdown);
                false
            }
        }
    }

    /// Raises the settings window, or starts it. `first`: a second launch forwarded its click
    /// and waits for the ack; the connection closes after it.
    fn open_settings(&self, conn: &mut Conn, first: bool) -> bool {
        // Windows only lets a window take the foreground when the process allows it.
        if let Some(client) = self.pushes.client()
            && let Err(e) = self.host.allow_foreground(client)
        {
            tracing::debug!(kind = ?e.kind(), "foreground not allowed for settings");
        }
        if !self.pushes.send(Push::Raise)
            && let Err(e) = self.host.open_settings()
        {
            tracing::warn!(kind = ?e.kind(), "settings window not started");
        }
        if first {
            // The second launch waits for this ack and the close after it; a close
            // without the ack (no free slot) tells it nothing was acted on.
            let _ = conn.send(&hello_ack());
        }
        !first
    }

    /// Runs a stored recording through the test pipeline.
    fn retranscribe(
        &self,
        conn: &mut Conn,
        session: &mut Session,
        run: u32,
        id: &HistoryId,
        set: SettingsSet,
        language: Language,
    ) -> bool {
        session.stop_words();
        if session.test_running.is_some() {
            return conn.send(&test_failed(run, TestFailure::Busy)).is_ok();
        }
        let Ok(samples) = self.host.history_audio(id) else {
            return conn.send(&test_failed(run, TestFailure::NoAudio)).is_ok();
        };
        let origin = if self.host.history_processed(id) {
            Origin::History
        } else {
            Origin::HistoryRaw
        };
        self.start_test(session, run, set, language, samples, origin)
    }

    /// The words of a recording for the player: the stored ones at once when they carry times,
    /// else a word-times run in the slot of a test run (its answer is a push from the worker).
    fn words(&self, conn: &mut Conn, session: &mut Session, run: u32, id: &HistoryId) -> bool {
        let answer = |conn: &mut Conn, result| {
            conn.send(&Push::HistoryWords {
                run,
                id: id.to_owned(),
                result,
            })
            .is_ok()
        };
        let no_audio = WordsResult::Unavailable {
            why: WordsFailure::NoAudio,
        };
        session.stop_words();
        if session.test_running.is_some() {
            return answer(conn, WordsResult::Busy);
        }
        match self.host.history_words(id) {
            Ok((words, Some(_))) if words.iter().any(|w| w.t0.is_some()) => {
                let result = WordsResult::Ready {
                    words: Words(words),
                    timed: true,
                };
                return answer(conn, result);
            }
            Ok(_) => {}
            Err(_) => return answer(conn, no_audio),
        }
        let Ok(samples) = self.host.history_audio(id) else {
            return answer(conn, no_audio);
        };
        let reply = self.pushes.with_words_for(id);
        session.test_running = Some(run);
        session.tested = true;
        session.words = reply.words_stop().map(|stop| (run, Arc::clone(stop)));
        // The configured model and language run it: `set` and `language` are not read.
        let job = TestJob {
            id: run,
            set: SettingsSet::defaults(),
            language: Language::AUTO,
            samples,
            origin: Origin::History,
            reply,
        };
        self.inbox.send(Inbox::Ipc(IpcAsk::Test(job))).is_ok()
    }

    /// A file run: a thread of its own decodes (in a child process), cuts and runs the pieces one by one in the test
    /// slot (this connection holds it until `FileResult`), then saves the record.
    fn transcribe_file(
        &self,
        conn: &mut Conn,
        session: &mut Session,
        run: u32,
        path: String,
        language: Language,
    ) -> bool {
        session.stop_words();
        if session.test_running.is_some() {
            return conn.send(&file_failed(run, FileFailure::Busy)).is_ok();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (host, inbox, pushes) = (
            Arc::clone(&self.host),
            self.inbox.clone(),
            self.pushes.clone(),
        );
        let thread_stop = Arc::clone(&stop);
        let spawned = thread::Builder::new()
            .name("file-run".into())
            .spawn(move || {
                let push = |p: Push| pushes.send(p);
                let chunk =
                    |set: &SettingsSet,
                     language,
                     samples: &[f32],
                     on: &mut dyn FnMut(u8, Option<u32>) -> bool| {
                        run_chunk(&inbox, run, set, language, samples, on, &thread_stop)
                    };
                let save =
                    |samples: &[f32], text: &str| host.history_save_file(samples, text, language);
                let decode = |path: &std::path::Path| host.decode_file(path, &thread_stop);
                let env = FileEnv {
                    set: host.file_set(),
                    decode: &decode,
                    push: &push,
                    chunk: &chunk,
                    save: &save,
                    cancel: &thread_stop,
                    max: CHUNK_MAX,
                    busy_wait: BUSY_WAIT,
                };
                let result = file_run(&env, run, std::path::Path::new(&path), language);
                pushes.send(Push::FileResult { run, result });
            });
        if spawned.is_err() {
            return conn.send(&file_failed(run, FileFailure::Failed)).is_ok();
        }
        session.test_running = Some(run);
        session.tested = true;
        session.file = Some((run, stop));
        true
    }

    /// Hands a recognition test run to the coordinator; its result comes back as a push. For
    /// `Origin::History` the worker does not process the samples again (a recovered raw one and
    /// a microphone clip still go through the processing).
    fn start_test(
        &self,
        session: &mut Session,
        id: u32,
        set: SettingsSet,
        language: Language,
        samples: Vec<f32>,
        origin: Origin,
    ) -> bool {
        session.test_running = Some(id);
        session.tested = true;
        let job = TestJob {
            id,
            set,
            language,
            samples,
            origin,
            reply: self.pushes.clone(),
        };
        self.inbox.send(Inbox::Ipc(IpcAsk::Test(job))).is_ok()
    }

    /// Asks the coordinator; the answer is sent by [`answer_status`], so a busy
    /// coordinator never holds up this connection's loop.
    fn status(&self, session: &mut Session) -> bool {
        if session.asks.len() >= MAX_ASKS {
            return true;
        }
        let (tx, rx) = mpsc::channel();
        session.asks.push_back((rx, Instant::now()));
        self.inbox.send(Inbox::Ipc(IpcAsk::Status(tx))).is_ok()
    }

    /// Capture starts only while the settings window is foreground and ends the moment it is not.
    fn capture(&self, on: bool, session: &mut Session) {
        if !on {
            self.end_capture(session);
        } else if session.capture.is_none()
            && session.client != 0
            && self.host.settings_foreground(session.client)
        {
            // On first, guard second: a guard that ran before the `on` could turn capture off and
            // then be overtaken by it, leaving every key swallowed with nobody watching.
            self.host.set_capture(true, true);
            match FocusGuard::start(&self.host, session.client) {
                Ok(guard) => session.capture = Some(guard),
                Err(e) => {
                    tracing::warn!(kind = ?e.kind(), "capture not started");
                    self.host.set_capture(false, false);
                }
            }
        }
    }

    fn end_capture(&self, session: &mut Session) {
        if let Some(guard) = session.capture.take() {
            guard.stop();
            // Always off, even when the guard already did it: off is idempotent, a key
            // swallowed for ever is not.
            self.host.set_capture(false, false);
        }
    }
}

/// Sends the answers that came in; drops asks the coordinator left unanswered too long.
fn answer_status(conn: &mut Conn, session: &mut Session, host: &dyn Host) -> bool {
    while let Some((rx, asked)) = session.asks.front() {
        match rx.try_recv() {
            Ok(report) => {
                session.asks.pop_front();
                if !send_status(conn, report, host) {
                    return false;
                }
            }
            Err(TryRecvError::Empty) if asked.elapsed() < ASK_LIMIT => break,
            Err(_) => {
                tracing::warn!("coordinator did not answer Status");
                session.asks.pop_front();
            }
        }
    }
    true
}

/// Sends `report` and the `Attention` the disk and the system make of it; `false` when the
/// connection is over.
fn send_status(conn: &mut Conn, report: StatusReport, host: &dyn Host) -> bool {
    let probe = host.probe(&report.model, &report.device);
    let attention = attention_push(&report, probe);
    conn.send(&status_push(report)).is_ok() && conn.send(&attention).is_ok()
}

/// Writes what other threads queued; `false` when the connection is over.
fn flush(conn: &mut Conn, session: &mut Session) -> bool {
    let Some((_, queue)) = &session.settings else {
        return true;
    };
    loop {
        match queue.try_recv() {
            Ok(push) => {
                if matches!(push, Push::TestResult { id, .. } | Push::HistoryWords { run: id, .. } if session.test_running == Some(id))
                {
                    session.test_running = None;
                }
                if matches!(&push, Push::TestResult { id, .. } | Push::HistoryWords { run: id, .. } if session.words.as_ref().is_some_and(|(r, _)| r == id))
                {
                    session.words = None;
                }
                if let Push::FileResult { run, .. } = &push
                    && session.file.as_ref().is_some_and(|(r, _)| r == run)
                {
                    session.file = None;
                    if session.test_running == Some(*run) {
                        session.test_running = None;
                    }
                }
                if conn.send(&push).is_err() {
                    return false;
                }
            }
            Err(TryRecvError::Empty) => return true,
            // Replaced by a newer window, or dropped for not reading.
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

fn test_failed(id: u32, why: TestFailure) -> Push {
    Push::TestResult {
        id,
        outcome: TestOutcome::Failed { why },
    }
}

fn file_failed(run: u32, why: FileFailure) -> Push {
    Push::FileResult {
        run,
        result: FileResult::Failed { why },
    }
}

/// The longest piece of a file given to whisper at once (arch-plan 9.6).
const CHUNK_MAX: oxi_core::types::Millis = oxi_core::types::Millis(300_000);
/// How long a piece waits when a dictation holds the worker (the dictation goes first).
const BUSY_WAIT: Duration = Duration::from_millis(500);

/// How one piece of a file ended.
enum Chunk {
    Text(String),
    /// A dictation is running: try the piece again.
    Busy,
    /// The run was cancelled or its window is gone.
    Stopped,
    Failed(FileFailure),
}

/// What [`file_run`] needs from outside, so a test runs it without a worker or a host.
struct FileEnv<'a> {
    /// `None`: History is off.
    set: Option<SettingsSet>,
    /// Decodes the file to 16 kHz mono; the daemon does it in a child process.
    decode: &'a dyn Fn(&std::path::Path) -> Result<Vec<f32>, oxi_platform::decode::DecodeError>,
    /// Queues a push for the window; `false` when nobody listens.
    push: &'a dyn Fn(Push) -> bool,
    /// Runs one piece; calls the closure with (percent, estimated ms) as it goes and stops when
    /// it answers `false`.
    #[allow(
        clippy::type_complexity,
        reason = "one callback, named by its parameters"
    )]
    chunk: &'a dyn Fn(
        &SettingsSet,
        Language,
        &[f32],
        &mut dyn FnMut(u8, Option<u32>) -> bool,
    ) -> Chunk,
    save: &'a dyn Fn(&[f32], &str) -> io::Result<HistoryItem>,
    cancel: &'a AtomicBool,
    max: oxi_core::types::Millis,
    busy_wait: Duration,
}

/// Decodes the file at `path`, cuts it at pauses, transcribes the pieces one by one and saves the
/// text as a record (announced with `HistoryAdded` before the `Done`). Progress is the share of
/// the audio done: finished pieces plus the percent whisper reports inside the one running.
fn file_run(env: &FileEnv<'_>, run: u32, path: &std::path::Path, language: Language) -> FileResult {
    use oxi_core::audio::{SAMPLE_RATE, voiced_runs};
    use oxi_core::config::VadSensitivity;
    use oxi_core::types::Millis;
    use oxi_platform::decode::DecodeError;

    let failed = |why| FileResult::Failed { why };
    let Some(set) = &env.set else {
        return failed(FileFailure::HistoryOff);
    };
    // Decoded straight to 16 kHz: the file's own rate is never held whole.
    let samples = match (env.decode)(path) {
        Ok(samples) => samples,
        Err(DecodeError::Stopped) => return FileResult::Cancelled,
        Err(DecodeError::Unsupported { what }) => return failed(FileFailure::Unsupported { what }),
        Err(DecodeError::Corrupt) => return failed(FileFailure::Corrupt),
        Err(DecodeError::TooLong) => return failed(FileFailure::TooLong),
        Err(DecodeError::Io(e)) => {
            tracing::warn!(kind = ?e.kind(), "file not opened");
            return failed(FileFailure::Unreadable);
        }
    };
    let per_ms = u64::from(SAMPLE_RATE) / 1000;
    let total = samples.len() as u64 / per_ms;
    let voiced: Vec<_> = voiced_runs(&samples, VadSensitivity::Normal)
        .into_iter()
        .map(|r| Millis(r.start as u64 / per_ms)..Millis(r.end as u64 / per_ms))
        .collect();
    let mut texts = Vec::new();
    for (i, cut) in oxi_core::history::chunks(&voiced, Millis(total), env.max)
        .iter()
        .enumerate()
    {
        let piece_ms = (cut.end.0 - cut.start.0).max(1);
        let at = |ms: u64| usize::try_from(ms * per_ms).unwrap_or(usize::MAX);
        let piece = &samples[at(cut.start.0)..at(cut.end.0).min(samples.len())];
        loop {
            if env.cancel.load(Ordering::Acquire) {
                return FileResult::Cancelled;
            }
            let mut progress = |percent: u8, est_ms: Option<u32>| {
                let done = cut.start.0 + piece_ms * u64::from(percent.min(100)) / 100;
                // The estimate is for the piece: the first one scales it to the whole file.
                let est_ms = est_ms.filter(|_| i == 0).map(|e| {
                    u32::try_from(u64::from(e) * total.max(1) / piece_ms).unwrap_or(u32::MAX)
                });
                (env.push)(Push::TestProgress {
                    id: run,
                    percent: u8::try_from(done * 100 / total.max(1))
                        .unwrap_or(100)
                        .min(100),
                    est_ms,
                })
            };
            match (env.chunk)(set, language, piece, &mut progress) {
                Chunk::Text(text) => {
                    texts.push(text);
                    break;
                }
                Chunk::Busy => thread::sleep(env.busy_wait),
                Chunk::Stopped => return FileResult::Cancelled,
                Chunk::Failed(why) => return failed(why),
            }
        }
    }
    let text = texts
        .iter()
        .map(|t| t.trim())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return failed(FileFailure::NothingHeard);
    }
    match (env.save)(&samples, &text) {
        Ok(item) => {
            let id = item.id.clone();
            (env.push)(Push::HistoryAdded { item });
            FileResult::Done { id }
        }
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), "file result not saved");
            failed(FileFailure::Failed)
        }
    }
}

/// One piece through the worker, in the test slot: its progress goes to `on`, its text comes
/// back. The worker cannot be stopped inside a piece, so a cancel abandons the wait and the
/// worker's answer goes nowhere.
fn run_chunk(
    inbox: &Sender<Inbox>,
    run: u32,
    set: &SettingsSet,
    language: Language,
    samples: &[f32],
    on: &mut dyn FnMut(u8, Option<u32>) -> bool,
    cancel: &AtomicBool,
) -> Chunk {
    let own = Pushes::default();
    let (_, replies) = own.claim(0);
    let job = TestJob {
        id: run,
        set: set.clone(),
        language,
        samples: samples.to_vec(),
        origin: Origin::Mic,
        reply: own,
    };
    if inbox.send(Inbox::Ipc(IpcAsk::Test(job))).is_err() {
        return Chunk::Failed(FileFailure::Failed);
    }
    loop {
        // Before every message, not only after a timeout: progress faster than POLL must not
        // delay a stop.
        if cancel.load(Ordering::Acquire) {
            return Chunk::Stopped;
        }
        match replies.recv_timeout(POLL) {
            Ok(Push::TestProgress {
                percent, est_ms, ..
            }) => {
                if !on(percent, est_ms) {
                    return Chunk::Stopped;
                }
            }
            Ok(Push::TestResult { outcome, .. }) => {
                return match outcome {
                    TestOutcome::Heard { text, .. } => Chunk::Text(text),
                    TestOutcome::Failed {
                        why: TestFailure::Busy,
                    } => Chunk::Busy,
                    TestOutcome::Failed {
                        why: TestFailure::ModelMissing | TestFailure::ModelBad,
                    } => Chunk::Failed(FileFailure::NoModel),
                    TestOutcome::Failed { .. } => Chunk::Failed(FileFailure::Failed),
                };
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Chunk::Failed(FileFailure::Failed);
            }
        }
    }
}

/// Answers a history edit; a failure is logged by its kind only, never with the text.
fn history_result(conn: &mut Conn, id: HistoryId, op: HistoryOp, done: io::Result<()>) -> bool {
    let ok = match done {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(kind = ?e.kind(), ?op, "history entry not changed");
            false
        }
    };
    conn.send(&Push::HistoryResult { id, op, ok }).is_ok()
}

fn hello_ack() -> Push {
    Push::HelloAck {
        proto: PROTO,
        version: crate::VERSION.into(),
    }
}

/// Is the wav of an entry the app's processed 16 kHz audio? A wav that is not at 16 kHz is raw
/// device audio. A meta file with `raw` set says so for good (a recovered recording stays raw
/// after a retranscribe or an edit wrote `done`, also when the microphone records at 16 kHz
/// itself, a Bluetooth headset). An older record has no marker: at 16 kHz only `done` and
/// `cancelled` count, a recovered one (`interrupted`, `failed`) is raw (docs/HISTORY-FORMAT.md);
/// such a record rewritten to `done` by a build from before the marker reads as processed.
/// No readable meta: as `history::list` reads it, `done` when there is a text, else `failed`.
fn audio_is_processed(meta_json: Option<&str>, has_text: bool, wav_rate: Option<u32>) -> bool {
    if wav_rate != Some(oxi_core::audio::SAMPLE_RATE) {
        return false;
    }
    let meta = meta_json.and_then(|j| oxi_core::history::Meta::from_json(j).ok());
    match meta {
        Some(m) if m.raw => false,
        Some(m) => matches!(
            m.status,
            oxi_core::history::Status::Done | oxi_core::history::Status::Cancelled
        ),
        None => has_text,
    }
}

/// Is the wav of entry `id` in `dir` the app's processed audio (see [`audio_is_processed`])?
fn entry_processed(dir: &std::path::Path, id: &HistoryId) -> bool {
    let base = dir.join(id.as_str());
    let json = std::fs::read_to_string(base.with_extension("json")).ok();
    let has_text = std::fs::read_to_string(base.with_extension("txt")).is_ok_and(|t| !t.is_empty());
    audio_is_processed(
        json.as_deref(),
        has_text,
        wav_rate(&base.with_extension("wav")),
    )
}

/// The sample rate a wav's header names (a killed recording's header is repaired first).
fn wav_rate(wav: &std::path::Path) -> Option<u32> {
    use std::io::Read;
    let mut file = std::fs::File::open(wav).ok()?;
    let mut head = [0; oxi_core::history::WAV_HEADER];
    let got = file.read(&mut head).ok()?;
    oxi_core::history::repaired_header(&head[..got], file.metadata().ok()?.len())
        .map(|h| h.sample_rate)
}

fn status_push(report: StatusReport) -> Push {
    // The worker names no device while nothing is in memory.
    let loaded = report.backend != NO_BACKEND;
    Push::Status {
        phase: report.phase,
        model: report.model,
        device: report.gpu,
        backend: report.backend,
        loaded,
        warnings: report.warnings,
    }
}

/// What the files say, corrected by the last load: a file of the right size that did not load
/// is damaged too. A file that is not there stays what it is.
fn seen_model(files: Option<ModelStatus>, load: Option<ModelFailure>) -> Option<ModelStatus> {
    match (files, load) {
        (Some(ModelStatus::Missing | ModelStatus::Partial), _) => files,
        (_, Some(ModelFailure::Bad)) => Some(ModelStatus::Bad),
        _ => files,
    }
}

/// Everything that needs the user, from the status report and what the disk and the system say.
fn attention_push(report: &StatusReport, probe: Probe) -> Push {
    let seen = Observed {
        model: seen_model(probe.model, report.model_failure),
        mic_missing: probe.mic_missing,
        load_oom_on_cpu: report.model_failure == Some(ModelFailure::OutOfMemory),
        warnings: &report.warnings,
        sounds: &report.sounds,
    };
    Push::Attention {
        items: attention(&seen),
    }
}

/// Is input device `device` (empty: the system default) missing from `mics`.
fn mic_missing(device: &str, mics: &[String]) -> bool {
    if device.is_empty() {
        mics.is_empty()
    } else {
        !mics.iter().any(|m| m == device)
    }
}

fn history_item(r: history::Recording) -> HistoryItem {
    HistoryItem {
        id: r.id,
        status: r.meta.status,
        duration_ms: r.meta.duration_ms,
        model: r.meta.model,
        language: r.meta.language,
        text: Spoken(r.text),
        held: r.meta.held.iter().map(HeldText::from).collect(),
    }
}

/// The page of `all` (newest first) after `older_than`, and whether older items remain.
fn history_page(
    all: impl IntoIterator<Item = HistoryItem>,
    older_than: Option<&str>,
) -> (Vec<HistoryItem>, bool) {
    let mut items = Vec::new();
    let mut bytes = 0;
    // Ids are `YYYY-MM-DD/HHMMSS-mmm`: text order is time order.
    let rest = all
        .into_iter()
        .filter(|i| older_than.is_none_or(|id| i.id.as_str() < id));
    for item in rest {
        bytes += 128 + item.id.len() + item.model.len() + item.language.len() + item.text.0.len();
        if !items.is_empty() && (items.len() >= HISTORY_PAGE_ITEMS || bytes > HISTORY_PAGE_BYTES) {
            return (items, true);
        }
        items.push(item);
    }
    (items, false)
}

/// The foreground window belongs to the settings client, and that client is not this daemon
/// (its tray menu is a foreground window of ours).
fn is_settings(foreground: u32, client: u32, own: u32) -> bool {
    foreground != 0 && foreground == client && foreground != own
}

/// The settings process this daemon started.
#[derive(Default)]
struct SettingsProc {
    /// Kept so a window that is still starting is not started twice.
    child: Mutex<Option<std::process::Child>>,
    /// Set once the daemon is going down: no new window may outlive it.
    closing: AtomicBool,
}

impl SettingsProc {
    /// `start` runs unless a window is still starting or the daemon is going down.
    fn open(&self, start: impl FnOnce() -> io::Result<std::process::Child>) -> io::Result<()> {
        let mut child = lock(&self.child);
        if self.closing.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(running) = child.as_mut()
            && running.try_wait()?.is_none()
        {
            return Ok(()); // still starting; it will connect
        }
        *child = Some(start()?);
        Ok(())
    }

    /// From now on `open` does nothing; returns the running child, if any.
    fn stop_opening(&self) -> Option<std::process::Child> {
        // Under the lock, so an `open` that is running finishes first and its child is taken.
        let mut child = lock(&self.child);
        self.closing.store(true, Ordering::SeqCst);
        child.take()
    }
}

/// How long a window gets to close itself and save before the process is ended.
const CLOSE_WAIT: Duration = Duration::from_millis(1500);

/// Asks `child` to close its windows (`WM_CLOSE`, so the page saves a pending edit), waits up
/// to [`CLOSE_WAIT`], then ends it. A child with no window yet is ended at once.
fn end_child(child: &mut std::process::Child) {
    if close::post_close(child.id()) > 0 {
        let deadline = Instant::now() + CLOSE_WAIT;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// `WM_CLOSE` to the top-level windows of a process.
mod close {
    use core::ffi::c_void;

    type Hwnd = *mut c_void;
    const WM_CLOSE: u32 = 0x0010;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn EnumWindows(f: unsafe extern "system" fn(Hwnd, isize) -> i32, lparam: isize) -> i32;
        fn GetWindowThreadProcessId(hwnd: Hwnd, pid: *mut u32) -> u32;
        fn PostMessageW(hwnd: Hwnd, msg: u32, wparam: usize, lparam: isize) -> i32;
    }

    struct Query {
        pid: u32,
        posted: usize,
    }

    unsafe extern "system" fn visit(hwnd: Hwnd, lparam: isize) -> i32 {
        // SAFETY: `lparam` is the `&mut Query` that `post_close` passed and keeps alive.
        let query = unsafe { &mut *(lparam as *mut Query) };
        let mut pid = 0;
        // SAFETY: `hwnd` comes from EnumWindows; `pid` is a valid out-pointer.
        unsafe { GetWindowThreadProcessId(hwnd, &raw mut pid) };
        // SAFETY: a posted message needs only the handle EnumWindows gave.
        if pid == query.pid && unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) } != 0 {
            query.posted += 1;
        }
        1
    }

    /// How many windows of `pid` were asked to close.
    pub fn post_close(pid: u32) -> usize {
        let mut query = Query { pid, posted: 0 };
        // SAFETY: `visit` only uses `query` during this call, which it outlives.
        unsafe { EnumWindows(visit, (&raw mut query) as isize) };
        query.posted
    }
}

/// The real host: the hook handle, the foreground window, `current_exe settings`, the stats file.
pub struct SystemHost {
    hook: Arc<oxi_platform::os::hook::HookHandle>,
    settings: SettingsProc,
    stats: Option<std::path::PathBuf>,
    layout: Option<oxi_core::paths::Layout>,
}

impl SystemHost {
    pub fn new(hook: Arc<oxi_platform::os::hook::HookHandle>) -> Self {
        Self {
            hook,
            settings: SettingsProc::default(),
            stats: super::stats::path(),
            layout: oxi_platform::dirs::layout().ok(),
        }
    }

    /// The folder history goes to, by the `[history] folder` of `config.toml` as it is now.
    fn history_dir(&self) -> io::Result<std::path::PathBuf> {
        let layout = self.layout.as_ref().ok_or(io::ErrorKind::NotFound)?;
        let cfg = std::fs::read_to_string(layout.config_file())
            .ok()
            .and_then(|text| oxi_core::config::ConfigFile::parse(&text).ok())
            .map(|file| file.config)
            .unwrap_or_default();
        Ok(oxi_core::history::folder(
            &cfg.history,
            &layout.history_dir(),
        ))
    }

    /// The daemon is going down: a later `OpenSettings` starts nothing. An open settings window
    /// stays (the Stop button's way: it shows the daemon offline and Start).
    pub fn stop_opening_settings(&self) {
        drop(self.settings.stop_opening());
    }

    /// The tray's Quit ends the whole app: no new settings window, and the one this daemon
    /// started closes. The lock is not held while it does.
    pub fn close_settings(&self) {
        if let Some(mut child) = self.settings.stop_opening() {
            end_child(&mut child);
        }
    }
}

impl Host for SystemHost {
    fn set_capture(&self, on: bool, swallow: bool) {
        self.hook.set_capture(on, swallow);
    }

    fn settings_foreground(&self, client: u32) -> bool {
        is_settings(
            oxi_platform::os::pipe::foreground_pid(),
            client,
            std::process::id(),
        )
    }

    fn allow_foreground(&self, client: u32) -> io::Result<()> {
        oxi_platform::os::pipe::allow_foreground(client)
    }

    fn devices(&self) -> Push {
        let mics = oxi_platform::capture::list_input_devices().unwrap_or_else(|e| {
            tracing::warn!("cannot list microphones: {e}");
            Vec::new()
        });
        Push::Devices {
            mics,
            gpus: oxi_whisper::list_devices(),
        }
    }

    fn stats(&self) -> Push {
        Push::Stats(super::stats::summary(
            self.stats.as_deref(),
            oxi_platform::stats_store::today(),
        ))
    }

    fn probe(&self, model: &str, device: &str) -> Probe {
        let mics = oxi_platform::capture::list_input_devices().unwrap_or_default();
        Probe {
            model: self.layout.as_ref().and_then(|layout| {
                models::find(model).map(|entry| models::status(entry, &layout.models_dir()))
            }),
            mic_missing: mic_missing(device, &mics),
        }
    }

    fn history_list(&self, older_than: Option<&str>) -> Push {
        // Lazy: only the entries of this page are read (`history::list_from`).
        let (items, more) = match self.history_dir() {
            Ok(dir) => history_page(
                history::list_from(&dir, older_than).map(history_item),
                older_than,
            ),
            Err(_) => (Vec::new(), false),
        };
        Push::History { items, more }
    }

    fn history_search(
        &self,
        cache: &mut history::SearchCache,
        seq: u32,
        query: &str,
        filter: &oxi_core::history::Filter,
        older_than: Option<&str>,
    ) -> Push {
        let (items, more) = match self.history_dir() {
            Ok(dir) => history_page(
                history::search(&dir, query, filter, older_than, cache).map(history_item),
                older_than,
            ),
            Err(_) => (Vec::new(), false),
        };
        Push::HistoryFound { seq, items, more }
    }

    fn file_set(&self) -> Option<SettingsSet> {
        let layout = self.layout.as_ref()?;
        let cfg = std::fs::read_to_string(layout.config_file())
            .ok()
            .and_then(|text| oxi_core::config::ConfigFile::parse(&text).ok())
            .map(|file| file.config)
            .unwrap_or_default();
        cfg.history.enabled.then_some(SettingsSet {
            normalize: cfg.audio.normalize,
            denoise: cfg.audio.denoise,
            trim: cfg.audio.trim,
            model: cfg.model.id,
        })
    }

    fn decode_file(
        &self,
        path: &std::path::Path,
        stop: &AtomicBool,
    ) -> Result<Vec<f32>, oxi_platform::decode::DecodeError> {
        oxi_platform::decode::decode_in_child(path, oxi_core::audio::SAMPLE_RATE, stop)
    }

    fn history_save_file(
        &self,
        samples: &[f32],
        text: &str,
        language: Language,
    ) -> io::Result<HistoryItem> {
        let layout = self.layout.as_ref().ok_or(io::ErrorKind::NotFound)?;
        let ctx = history::file_context(&layout.config_file(), language.as_str());
        history::save_transcribed(
            &self.history_dir()?,
            samples,
            text,
            &ctx,
            oxi_platform::fs::local_now(),
        )
    }

    fn announces(&self) -> bool {
        true
    }

    fn history_audio(&self, id: &HistoryId) -> io::Result<Vec<f32>> {
        history::audio_16k(&self.history_dir()?, id)
    }

    fn history_processed(&self, id: &HistoryId) -> bool {
        self.history_dir()
            .is_ok_and(|dir| entry_processed(&dir, id))
    }

    fn history_set_text(
        &self,
        id: &HistoryId,
        text: &str,
        set: &SettingsSet,
        language: Language,
        words: Vec<oxi_core::history::Word>,
    ) -> io::Result<()> {
        let ctx = HistoryContext {
            model: set.model.clone(),
            language: language.as_str().to_owned(),
            settings: Some(set.clone()),
        };
        history::set_text_with_words(&self.history_dir()?, id, text, &ctx, words)
    }

    fn history_words(
        &self,
        id: &HistoryId,
    ) -> io::Result<(
        Vec<oxi_core::history::Word>,
        Option<oxi_core::history::Timings>,
    )> {
        history::read_words(&self.history_dir()?, id)
    }

    fn history_edit(&self, id: &HistoryId, text: &str) -> io::Result<()> {
        history::edit_text(&self.history_dir()?, id, text)
    }

    fn history_restore(&self, id: &HistoryId) -> io::Result<()> {
        history::restore(&self.history_dir()?, id)
    }

    fn history_delete(&self, id: &HistoryId) -> io::Result<()> {
        history::delete(&self.history_dir()?, id)
    }

    fn open_settings(&self) -> io::Result<()> {
        self.settings.open(|| {
            let started = std::process::Command::new(std::env::current_exe()?)
                .arg("settings")
                .spawn()?;
            // Its first window comes to the front only with this permission.
            if let Err(e) = self.allow_foreground(started.id()) {
                tracing::debug!(kind = ?e.kind(), "foreground not allowed for settings");
            }
            Ok(started)
        })
    }
}

#[cfg(test)]
pub(super) mod test_bus {
    use std::sync::MutexGuard;

    use super::{BUS, Duration, Instant, Mutex, Push, Pushes, Receiver, lock};

    /// The process bus is one global: a test that sets it holds this lock and leaves it empty.
    pub static BUS_TEST: Mutex<()> = Mutex::new(());

    /// A settings window "connected" on the process bus, for the producers that announce
    /// through it (the stats thread, the file watcher, the coordinator).
    pub struct OnBus {
        rx: Receiver<Push>,
        _only_one: MutexGuard<'static, ()>,
    }

    impl OnBus {
        pub fn connected() -> Self {
            let only_one = lock(&BUS_TEST);
            let pushes = Pushes::default();
            let (_, rx) = pushes.claim(0);
            *lock(&BUS) = Some(pushes);
            Self {
                rx,
                _only_one: only_one,
            }
        }

        /// The next push within `ms` that `want` picks.
        pub fn next<T>(&self, ms: u64, want: impl Fn(Push) -> Option<T>) -> Option<T> {
            let end = Instant::now() + Duration::from_millis(ms);
            while let Some(left) = end.checked_duration_since(Instant::now()) {
                match self.rx.recv_timeout(left) {
                    Ok(push) => {
                        if let Some(found) = want(push) {
                            return Some(found);
                        }
                    }
                    Err(_) => return None,
                }
            }
            None
        }
    }

    impl Drop for OnBus {
        fn drop(&mut self) {
            *lock(&BUS) = None;
        }
    }
}

#[cfg(test)]
mod tests;
