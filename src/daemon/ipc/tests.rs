use std::sync::atomic::AtomicU64;

use oxi_core::config::OverlayPosition;
use oxi_core::device::DeviceChoice;
use oxi_core::ipc::{Attention, DeleteResult, MAX_LINE, SoundWarning, SoundWarningKind};
use oxi_core::machine::{Phase, Sound};

use super::test_bus::OnBus;
use super::*;

const WAIT: Duration = Duration::from_secs(3);

fn hid(id: &str) -> HistoryId {
    HistoryId::parse(id).unwrap()
}

#[derive(Debug, PartialEq)]
enum Call {
    Capture(bool, bool),
    Open,
    Allow(u32),
}

struct Fake {
    foreground: AtomicBool,
    calls: Sender<Call>,
    /// What the hook would be doing now: keys swallowed.
    capturing: AtomicBool,
    /// Milliseconds `set_capture(true, ..)` takes, as when its thread is preempted.
    on_delay: AtomicU64,
    /// The threads `probe` ran on.
    probed_on: Mutex<Vec<Option<String>>>,
    /// The words each `history_set_text` got.
    set_words: Mutex<Vec<Vec<oxi_core::history::Word>>>,
    /// History is on, so a file run goes past its start.
    file_on: AtomicBool,
}

impl Host for Fake {
    fn set_capture(&self, on: bool, swallow: bool) {
        if on {
            thread::sleep(Duration::from_millis(self.on_delay.load(Ordering::Relaxed)));
        }
        self.capturing.store(on, Ordering::SeqCst);
        let _ = self.calls.send(Call::Capture(on, swallow));
    }
    fn settings_foreground(&self, _client: u32) -> bool {
        self.foreground.load(Ordering::Relaxed)
    }
    fn allow_foreground(&self, client: u32) -> io::Result<()> {
        let _ = self.calls.send(Call::Allow(client));
        Ok(())
    }
    fn devices(&self) -> Push {
        Push::Devices {
            mics: vec!["mic".into()],
            gpus: vec![],
        }
    }
    fn stats(&self) -> Push {
        Push::Stats(oxi_core::stats::Summary {
            words: 7,
            heat: vec![0, 4],
            ..Default::default()
        })
    }
    fn open_settings(&self) -> io::Result<()> {
        let _ = self.calls.send(Call::Open);
        Ok(())
    }
    fn probe(&self, _model: &str, _device: &str) -> Probe {
        lock(&self.probed_on).push(thread::current().name().map(String::from));
        Probe {
            model: Some(ModelStatus::Bad),
            mic_missing: true,
        }
    }
    fn history_list(&self, older_than: Option<&str>) -> Push {
        let item = |id: &str| HistoryItem {
            id: hid(id),
            status: oxi_core::history::Status::Done,
            duration_ms: 900,
            model: "turbo".into(),
            language: "auto".into(),
            text: Spoken("words".into()),
            held: Vec::new(),
        };
        let (items, more) = history_page(
            [item("2026-11-10/120000-000"), item("2026-11-09/120000-000")],
            older_than,
        );
        Push::History { items, more }
    }
    fn history_search(
        &self,
        _cache: &mut history::SearchCache,
        seq: u32,
        query: &str,
        _filter: &oxi_core::history::Filter,
        _older_than: Option<&str>,
    ) -> Push {
        let found = query == "words";
        let items = if found {
            let Push::History { items, .. } = self.history_list(None) else {
                unreachable!()
            };
            items
        } else {
            Vec::new()
        };
        Push::HistoryFound {
            seq,
            items,
            more: false,
        }
    }
    fn file_set(&self) -> Option<SettingsSet> {
        self.file_on
            .load(Ordering::SeqCst)
            .then(SettingsSet::defaults)
    }
    fn decode_file(
        &self,
        path: &std::path::Path,
        _: &AtomicBool,
    ) -> Result<Vec<f32>, oxi_platform::decode::DecodeError> {
        oxi_platform::decode::decode_file(path, oxi_core::audio::SAMPLE_RATE)
    }
    fn history_save_file(&self, _: &[f32], _: &str, _: Language) -> io::Result<HistoryItem> {
        Err(io::ErrorKind::Unsupported.into())
    }
    fn history_audio(&self, id: &HistoryId) -> io::Result<Vec<f32>> {
        if id == GOOD_ID || id == RAW_ID {
            Ok(vec![0.25; 3])
        } else {
            Err(io::ErrorKind::NotFound.into())
        }
    }
    fn history_processed(&self, id: &HistoryId) -> bool {
        id != RAW_ID
    }
    fn history_set_text(
        &self,
        id: &HistoryId,
        _text: &str,
        _set: &SettingsSet,
        _language: Language,
        words: Vec<oxi_core::history::Word>,
    ) -> io::Result<()> {
        lock(&self.set_words).push(words);
        self.history_audio(id).map(drop)
    }
    fn history_words(
        &self,
        id: &HistoryId,
    ) -> io::Result<(
        Vec<oxi_core::history::Word>,
        Option<oxi_core::history::Timings>,
    )> {
        self.history_audio(id).map(|_| {
            let timed = id == GOOD_ID;
            let word = oxi_core::history::Word {
                w: "w".into(),
                p: 0.9,
                t0: timed.then_some(5),
                t1: timed.then_some(15),
            };
            let how = oxi_core::history::Timings {
                by: oxi_core::history::TimingBy::Tokens,
                preset: String::new(),
                model: String::new(),
            };
            (vec![word], timed.then_some(how))
        })
    }
    fn history_edit(&self, id: &HistoryId, _text: &str) -> io::Result<()> {
        self.history_audio(id).map(drop)
    }
    fn history_restore(&self, id: &HistoryId) -> io::Result<()> {
        self.history_audio(id).map(drop)
    }
    fn history_delete(&self, id: &HistoryId) -> io::Result<()> {
        self.history_audio(id).map(drop)
    }
}

const GOOD_ID: &str = "2026-11-10/101500-123";
/// A recovered recording: raw microphone audio.
const RAW_ID: &str = "2026-11-10/101600-456";

/// A one-process child (`ping` starts no grandchild), so nothing is left behind.
fn sleeper() -> std::process::Child {
    std::process::Command::new("ping")
        .args(["-n", "30", "127.0.0.1"])
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn end_child_leaves_no_process_behind() {
    let mut child = sleeper();
    end_child(&mut child); // no window: ended at once
    assert!(child.try_wait().unwrap().is_some());
}

#[test]
fn end_child_lets_a_window_close_itself() {
    // A Forms window exits with 0 on WM_CLOSE; a killed process exits with 1.
    let script = "Add-Type -AssemblyName System.Windows.Forms; \
                      [Windows.Forms.Application]::Run((New-Object Windows.Forms.Form))";
    let mut child = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .spawn()
        .unwrap();
    let up = Instant::now() + Duration::from_secs(30);
    while !has_window(child.id()) {
        assert!(Instant::now() < up, "the test window never opened");
        thread::sleep(Duration::from_millis(100));
    }
    end_child(&mut child);
    assert_eq!(child.try_wait().unwrap().and_then(|s| s.code()), Some(0));
}

/// Does `pid` own a window yet? Asked of Windows through another process, since
/// `post_close` would close it.
fn has_window(pid: u32) -> bool {
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-Command",
            &format!("(Get-Process -Id {pid}).MainWindowHandle -ne 0"),
        ])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim() == "True"
}

#[test]
fn settings_proc_opens_once_and_never_after_closing() {
    let settings = SettingsProc::default();
    let started = std::cell::Cell::new(0);
    let start = || {
        started.set(started.get() + 1);
        Ok(sleeper())
    };
    settings.open(start).unwrap();
    settings.open(start).unwrap(); // still running: not started twice
    assert_eq!(started.get(), 1);
    let mut child = settings.stop_opening().unwrap();
    end_child(&mut child);
    settings.open(start).unwrap();
    assert_eq!(started.get(), 1, "opened after the daemon went down");
}

/// A line that is no `Request`, written raw.
struct Raw(String);

impl oxi_platform::ipc::Message for Raw {
    fn encode(&self) -> String {
        self.0.clone()
    }
    fn decode(_: &mut oxi_core::ipc::Decoder) -> Option<Result<Self, oxi_core::ipc::DecodeError>> {
        None
    }
}

struct Rig {
    name: String,
    host: Arc<Fake>,
    pushes: Pushes,
    inbox: Receiver<Inbox>,
    io: Receiver<IoCmd>,
    calls: Receiver<Call>,
    keys: Sender<(Key, bool)>,
}

fn rig(tag: &str) -> Rig {
    // The daemon is Windows-only: the name is always a pipe.
    let name = oxi_core::ipc::pipe_name(&format!("test-{}-ipc-{tag}", std::process::id()));
    let Ok(Start::Serve(server)) = bind_at(&name) else {
        panic!("bind failed");
    };
    let (inbox_tx, inbox) = mpsc::channel();
    let (io_tx, io) = mpsc::channel();
    let (calls_tx, calls) = mpsc::channel();
    let (keys, keys_rx) = mpsc::channel();
    let host = Arc::new(Fake {
        foreground: AtomicBool::new(true),
        calls: calls_tx,
        capturing: AtomicBool::new(false),
        on_delay: AtomicU64::new(0),
        probed_on: Mutex::new(Vec::new()),
        set_words: Mutex::new(Vec::new()),
        file_on: AtomicBool::new(false),
    });
    let pushes = Pushes::default();
    spawn(
        server,
        inbox_tx,
        io_tx,
        host.clone(),
        pushes.clone(),
        keys_rx,
    )
    .unwrap();
    Rig {
        name,
        host,
        pushes,
        inbox,
        io,
        calls,
        keys,
    }
}

fn connect(rig: &Rig) -> Conn {
    let mut conn = Conn::connect(&rig.name).unwrap();
    conn.set_read_timeout(Some(WAIT)).unwrap();
    conn
}

/// A connection that said `Hello` and got its ack.
fn settings(rig: &Rig) -> Conn {
    let mut conn = connect(rig);
    conn.send(&Request::Hello {
        proto: PROTO,
        version: "test".into(),
    })
    .unwrap();
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::HelloAck { proto: PROTO, .. })
    ));
    conn
}

/// Review c04 #7: a status the coordinator posts is probed and sent by the connection's
/// thread; the poster only leaves it (no device enumeration on the coordinator).
#[test]
fn a_posted_status_is_probed_and_sent_by_the_connection_thread() {
    let rig = rig("posted");
    let mut conn = settings(&rig);
    let report = StatusReport {
        phase: Phase::Idle,
        model: "tiny".into(),
        device: "mic".into(),
        gpu: DeviceChoice::Auto,
        backend: "cpu".into(),
        model_failure: None,
        warnings: Vec::new(),
        sounds: Vec::new(),
    };
    rig.pushes.post_status(report.clone());
    rig.pushes.post_status(report); // a newer one replaces the unsent one
    assert!(
        lock(&rig.host.probed_on).is_empty(),
        "the poster never probes"
    );
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::Status {
            phase: Phase::Idle,
            ..
        })
    ));
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::Attention { .. })
    ));
    let me = thread::current().name().map(String::from);
    let probed = lock(&rig.host.probed_on).clone();
    assert_eq!(probed.len(), 1, "sent once: {probed:?}");
    assert_ne!(probed[0], me);
}

#[test]
fn status_and_devices_are_answered() {
    let rig = rig("status");
    let mut conn = settings(&rig);
    let report = StatusReport {
        phase: Phase::Recording,
        model: "tiny".into(),
        device: "mic".into(),
        gpu: DeviceChoice::Named("Vulkan0".into()),
        backend: "cpu".into(),
        model_failure: None,
        warnings: vec!["w".into()],
        sounds: Vec::new(),
    };
    conn.send(&Request::Status).unwrap();
    let Ok(Inbox::Ipc(IpcAsk::Status(reply))) = rig.inbox.recv_timeout(WAIT) else {
        panic!("no Status ask");
    };
    reply.send(report).unwrap();
    assert_eq!(
        conn.recv::<Push>().unwrap(),
        Some(Push::Status {
            phase: Phase::Recording,
            model: "tiny".into(),
            device: DeviceChoice::Named("Vulkan0".into()),
            backend: "cpu".into(),
            loaded: true,
            warnings: vec!["w".into()],
        })
    );
    // The fake host reports a damaged model and no microphone; the engine line is a plain
    // warning, so it adds nothing.
    assert_eq!(
        conn.recv::<Push>().unwrap(),
        Some(Push::Attention {
            items: vec![Attention::ModelDamaged, Attention::NoMicrophone]
        })
    );
    conn.send(&Request::ListDevices).unwrap();
    assert_eq!(conn.recv::<Push>().unwrap(), Some(rig.host.devices()));
    conn.send(&Request::Stats).unwrap();
    assert_eq!(conn.recv::<Push>().unwrap(), Some(rig.host.stats()));
}

#[test]
fn history_requests_reach_the_host_and_answer() {
    let rig = rig("history");
    let mut conn = settings(&rig);
    conn.send(&Request::HistoryList { older_than: None })
        .unwrap();
    let Some(Push::History { items, more }) = conn.recv::<Push>().unwrap() else {
        panic!("no History");
    };
    assert_eq!((items.len(), more), (2, false));
    conn.send(&Request::HistoryList {
        older_than: Some(hid("2026-11-10/120000-000")),
    })
    .unwrap();
    let Some(Push::History { items, .. }) = conn.recv::<Push>().unwrap() else {
        panic!("no History");
    };
    assert_eq!(items.len(), 1);
    for (id, ok) in [(GOOD_ID, true), ("2026-11-10/000000-000", false)] {
        conn.send(&Request::HistorySetText {
            id: hid(id),
            text: Spoken("new".into()),
            set: SettingsSet::defaults(),
            language: Language::RU,
            words: Words::default(),
        })
        .unwrap();
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::HistoryResult {
                id: hid(id),
                op: HistoryOp::SetText,
                ok
            })
        );
        conn.send(&Request::HistoryEdit {
            id: hid(id),
            text: Spoken("edited".into()),
        })
        .unwrap();
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::HistoryResult {
                id: hid(id),
                op: HistoryOp::Edit,
                ok
            })
        );
        conn.send(&Request::HistoryRestore { id: hid(id) }).unwrap();
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::HistoryResult {
                id: hid(id),
                op: HistoryOp::Restore,
                ok
            })
        );
        conn.send(&Request::HistoryDelete { id: hid(id) }).unwrap();
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::HistoryResult {
                id: hid(id),
                op: HistoryOp::Delete,
                ok
            })
        );
    }
}

#[test]
fn a_retranscribe_runs_the_saved_audio_one_at_a_time() {
    let rig = rig("retranscribe");
    let mut conn = settings(&rig);
    let ask = |run, id: &str| Request::HistoryRetranscribe {
        run,
        id: hid(id),
        set: SettingsSet::defaults(),
        language: Language::RU,
    };
    conn.send(&ask(1, "2026-11-10/000000-000")).unwrap();
    assert_eq!(
        conn.recv::<Push>().unwrap(),
        Some(Push::TestResult {
            id: 1,
            outcome: TestOutcome::Failed {
                why: TestFailure::NoAudio
            }
        })
    );
    conn.send(&ask(2, GOOD_ID)).unwrap();
    let Ok(Inbox::Ipc(IpcAsk::Test(job))) = rig.inbox.recv_timeout(WAIT) else {
        panic!("no test job");
    };
    assert_eq!((job.id, job.samples), (2, vec![0.25; 3]));
    assert!(
        job.origin.is_stored_audio(),
        "a History recording is already processed"
    );
    // Its answer is pending: the next one waits.
    conn.send(&ask(3, GOOD_ID)).unwrap();
    assert_eq!(
        conn.recv::<Push>().unwrap(),
        Some(Push::TestResult {
            id: 3,
            outcome: TestOutcome::Failed {
                why: TestFailure::Busy
            }
        })
    );
}

#[test]
fn stored_word_times_are_answered_at_once_and_the_rest_goes_to_the_worker() {
    let rig = rig("words");
    let mut conn = settings(&rig);
    let ask = |run, id: &str| Request::HistoryWords { run, id: hid(id) };
    conn.send(&ask(1, "2026-11-10/000000-000")).unwrap();
    assert_eq!(
        conn.recv::<Push>().unwrap(),
        Some(Push::HistoryWords {
            run: 1,
            id: hid("2026-11-10/000000-000"),
            result: WordsResult::Unavailable {
                why: WordsFailure::NoAudio
            }
        })
    );
    // GOOD_ID has timed words in the fake: no worker run.
    conn.send(&ask(2, GOOD_ID)).unwrap();
    let Some(Push::HistoryWords {
        run: 2,
        result: WordsResult::Ready { words, timed: true },
        ..
    }) = conn.recv::<Push>().unwrap()
    else {
        panic!("no Ready");
    };
    assert_eq!(words.0[0].t0, Some(5));
    assert!(rig.inbox.recv_timeout(Duration::from_millis(50)).is_err());
    // RAW_ID has words without times: the worker gets a run for that record.
    conn.send(&ask(3, RAW_ID)).unwrap();
    let job = |rig: &Rig| match rig.inbox.recv_timeout(WAIT) {
        Ok(Inbox::Ipc(IpcAsk::Test(job))) => job,
        _ => panic!("no job"),
    };
    let three = job(&rig);
    assert_eq!((three.id, three.reply.words_for()), (3, Some(&hid(RAW_ID))));
    assert!(!three.reply.words_stopped());
    // The next ask (the pick moved on) stops the run in hand instead of being Busy.
    conn.send(&ask(4, RAW_ID)).unwrap();
    let four = job(&rig);
    assert_eq!(four.id, 4);
    assert!(three.reply.words_stopped() && !four.reply.words_stopped());
    // The stopped run's late answer and a cancel of it leave the new run alone.
    three.reply.send(Push::HistoryWords {
        run: 3,
        id: hid(RAW_ID),
        result: WordsResult::Busy,
    });
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::HistoryWords { run: 3, .. })
    ));
    conn.send(&Request::HistoryWordsCancel { run: 3 }).unwrap();
    conn.send(&Request::Stats).unwrap();
    assert!(matches!(conn.recv::<Push>().unwrap(), Some(Push::Stats(_))));
    assert!(!four.reply.words_stopped());
    // Transcribe again goes first: the words run stops, the run starts.
    conn.send(&Request::HistoryRetranscribe {
        run: 6,
        id: hid(GOOD_ID),
        set: SettingsSet::defaults(),
        language: Language::RU,
    })
    .unwrap();
    assert_eq!(job(&rig).id, 6);
    assert!(four.reply.words_stopped());
}

#[test]
fn a_words_run_stops_on_its_cancel_and_when_the_window_closes() {
    let rig = rig("words-stop");
    let mut conn = settings(&rig);
    let job = |rig: &Rig| match rig.inbox.recv_timeout(WAIT) {
        Ok(Inbox::Ipc(IpcAsk::Test(job))) => job,
        _ => panic!("no job"),
    };
    conn.send(&Request::HistoryWords {
        run: 1,
        id: hid(RAW_ID),
    })
    .unwrap();
    let one = job(&rig);
    conn.send(&Request::HistoryWordsCancel { run: 1 }).unwrap();
    conn.send(&Request::Stats).unwrap();
    assert!(matches!(conn.recv::<Push>().unwrap(), Some(Push::Stats(_))));
    assert!(one.reply.words_stopped());
    // The slot is free at once: a Test page run is not Busy.
    conn.send(&Request::TestTranscribe {
        id: 7,
        set: SettingsSet::defaults(),
        language: Language::RU,
    })
    .unwrap();
    assert_eq!(job(&rig).id, 7);

    let rig = self::rig("words-close");
    let mut conn = settings(&rig);
    conn.send(&Request::HistoryWords {
        run: 2,
        id: hid(RAW_ID),
    })
    .unwrap();
    let two = job(&rig);
    drop(conn);
    let until = Instant::now() + WAIT;
    while !two.reply.words_stopped() && Instant::now() < until {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(two.reply.words_stopped(), "the closed window's run goes on");
}

#[test]
fn the_words_of_a_new_run_reach_the_record_with_its_text() {
    let rig = rig("set-words");
    let mut conn = settings(&rig);
    let word = oxi_core::history::Word {
        w: "new".into(),
        p: 0.3,
        t0: None,
        t1: None,
    };
    conn.send(&Request::HistorySetText {
        id: hid(GOOD_ID),
        text: Spoken("new".into()),
        set: SettingsSet::defaults(),
        language: Language::RU,
        words: Words(vec![word.clone()]),
    })
    .unwrap();
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::HistoryResult { ok: true, .. })
    ));
    assert_eq!(*lock(&rig.host.set_words), [vec![word]]);
}

#[test]
fn a_recovered_raw_recording_still_gets_processed_on_retranscribe() {
    let rig = rig("retranscribe-raw");
    let mut conn = settings(&rig);
    conn.send(&Request::HistoryRetranscribe {
        run: 1,
        id: hid(RAW_ID),
        set: SettingsSet::defaults(),
        language: Language::RU,
    })
    .unwrap();
    let Ok(Inbox::Ipc(IpcAsk::Test(job))) = rig.inbox.recv_timeout(WAIT) else {
        panic!("no test job");
    };
    assert!(!job.origin.is_stored_audio(), "raw audio must be processed");
}

/// A wav header of `rate` Hz mono 16 bit and two samples of silence.
fn tiny_wav(rate: u32) -> Vec<u8> {
    let mut b = b"RIFF".to_vec();
    b.extend(40_u32.to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16_u32.to_le_bytes());
    b.extend([1, 0, 1, 0]);
    b.extend(rate.to_le_bytes());
    b.extend((rate * 2).to_le_bytes());
    b.extend([2, 0, 16, 0]);
    b.extend(b"data");
    b.extend(4_u32.to_le_bytes());
    b.extend([0; 4]);
    b
}

/// An entry in a fresh owned folder: a wav at `rate` and a meta with `status`.
fn entry(
    tag: &str,
    rate: u32,
    status: oxi_core::history::Status,
) -> (std::path::PathBuf, &'static str) {
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-ipc-{tag}-{}", std::process::id()));
    std::fs::write(dir.join(oxi_core::history::MARKER), b"x").unwrap();
    let id = "2026-11-10/101600-456";
    std::fs::create_dir_all(dir.join("2026-11-10")).unwrap();
    std::fs::write(dir.join(id).with_extension("wav"), tiny_wav(rate)).unwrap();
    let meta = oxi_core::history::Meta {
        version: oxi_core::history::Meta::VERSION,
        started: String::new(),
        duration_ms: 0,
        status,
        model: String::new(),
        language: String::new(),
        settings: None,
        previous: None,
        words: Vec::new(),
        timings: None,
        raw: false,
        held: Vec::new(),
        delivered: None,
    };
    std::fs::write(dir.join(id).with_extension("json"), meta.to_json().unwrap()).unwrap();
    (dir, id)
}

fn retranscribed(dir: &std::path::Path, id: &str) {
    history::set_text(dir, &hid(id), "text", &HistoryContext::default()).unwrap();
}

#[test]
fn a_raw_device_rate_recording_stays_raw_after_a_retranscribe_wrote_done() {
    let (dir, id) = entry("raw48", 48_000, oxi_core::history::Status::Interrupted);
    assert!(!entry_processed(&dir, &hid(id)));
    retranscribed(&dir, id); // status is now done
    assert!(
        !entry_processed(&dir, &hid(id)),
        "second retranscribe: still raw"
    );
}

#[test]
fn a_recovered_16k_recording_stays_raw_after_its_status_is_rewritten() {
    let (dir, id) = entry("raw16", 16_000, oxi_core::history::Status::Failed);
    assert!(
        !entry_processed(&dir, &hid(id)),
        "a Bluetooth clip, as recovered"
    );
    // The recovered record carries the marker; a retranscribe or an edit keep it.
    mark_raw(&dir, id);
    retranscribed(&dir, id);
    assert!(!entry_processed(&dir, &hid(id)), "after set_text");
    history::edit_text(&dir, &hid(id), "edited").unwrap();
    assert!(!entry_processed(&dir, &hid(id)), "after edit_text");
    // An older record with no marker: nothing on disk tells it from a processed one.
    let (dir, id) = entry("raw16-old", 16_000, oxi_core::history::Status::Failed);
    retranscribed(&dir, id);
    assert!(entry_processed(&dir, &hid(id)));
    let (dir, id) = entry("done16", 16_000, oxi_core::history::Status::Done);
    assert!(entry_processed(&dir, &hid(id)));
}

/// What `recover_part` writes: the marker on top of the meta the entry has.
fn mark_raw(dir: &std::path::Path, id: &str) {
    let json = dir.join(id).with_extension("json");
    let meta =
        oxi_core::history::Meta::from_json(&std::fs::read_to_string(&json).unwrap()).unwrap();
    let meta = oxi_core::history::Meta { raw: true, ..meta };
    std::fs::write(json, meta.to_json().unwrap()).unwrap();
}

#[test]
fn only_done_and_cancelled_audio_counts_as_processed() {
    use oxi_core::history::Status::{Cancelled, Done, Failed, Interrupted};
    let meta = |status: oxi_core::history::Status| {
        oxi_core::history::Meta {
            version: oxi_core::history::Meta::VERSION,
            started: String::new(),
            duration_ms: 0,
            status,
            model: String::new(),
            language: String::new(),
            settings: None,
            previous: None,
            words: Vec::new(),
            timings: None,
            raw: false,
            held: Vec::new(),
            delivered: None,
        }
        .to_json()
        .unwrap()
    };
    let rate = Some(16_000);
    assert!(audio_is_processed(Some(&meta(Done)), false, rate));
    assert!(audio_is_processed(Some(&meta(Cancelled)), false, rate));
    assert!(!audio_is_processed(Some(&meta(Interrupted)), false, rate));
    assert!(!audio_is_processed(Some(&meta(Failed)), true, rate));
    assert!(audio_is_processed(None, true, rate));
    assert!(!audio_is_processed(None, false, rate));
    assert!(!audio_is_processed(Some("not json"), false, rate));
    assert!(!audio_is_processed(Some(&meta(Done)), true, Some(48_000)));
    assert!(!audio_is_processed(Some(&meta(Done)), true, None));
}

#[test]
fn a_model_delete_goes_to_the_coordinator_and_its_answer_comes_back_as_a_push() {
    let rig = rig("delete-model");
    let mut conn = settings(&rig);
    for result in [DeleteResult::Done { freed: 9 }, DeleteResult::InUse] {
        conn.send(&Request::DeleteModel { id: "turbo".into() })
            .unwrap();
        let Ok(Inbox::Ipc(IpcAsk::DeleteModel { id, reply })) = rig.inbox.recv_timeout(WAIT) else {
            panic!("no DeleteModel ask");
        };
        assert_eq!(id, "turbo");
        reply.send(Push::ModelDeleted {
            id,
            result: result.clone(),
        });
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::ModelDeleted {
                id: "turbo".into(),
                result
            })
        );
    }
}

#[test]
fn a_failed_load_reaches_the_attention_even_when_the_file_looks_right() {
    use ModelFailure::{Bad, OutOfMemory};
    use ModelStatus::{Bad as Wrong, Missing, NeedsHash, Partial, Ready};
    // (what the files say, what the last load said, what the chip sees)
    for (files, load, want) in [
        (Some(Ready), Some(Bad), Some(Wrong)),
        (Some(NeedsHash), Some(Bad), Some(Wrong)),
        (None, Some(Bad), Some(Wrong)),
        (Some(Missing), Some(Bad), Some(Missing)),
        (Some(Partial), Some(Bad), Some(Partial)),
        (Some(Ready), None, Some(Ready)),
        (Some(Ready), Some(OutOfMemory), Some(Ready)),
    ] {
        assert_eq!(seen_model(files, load), want, "{files:?} {load:?}");
    }
    let mut report = StatusReport {
        phase: Phase::Idle,
        model: "tiny".into(),
        device: String::new(),
        gpu: DeviceChoice::Auto,
        backend: "none".into(),
        model_failure: Some(OutOfMemory),
        warnings: Vec::new(),
        sounds: Vec::new(),
    };
    let probe = Probe {
        model: Some(Ready),
        mic_missing: false,
    };
    assert_eq!(
        attention_push(&report, probe),
        Push::Attention {
            items: vec![Attention::OutOfMemory { cpu: true }]
        }
    );
    // Status.loaded follows the worker's "no device" word.
    assert!(matches!(
        status_push(report.clone()),
        Push::Status { loaded: false, .. }
    ));
    report.backend = "Vulkan0".into();
    assert!(matches!(
        status_push(report.clone()),
        Push::Status { loaded: true, .. }
    ));
    report.backend = "none".into();
    // The sound worker's kind decides, not its line's wording.
    report.model_failure = None;
    report.sounds = vec![SoundWarning {
        kind: SoundWarningKind::Unused(Sound::Stop),
        line: "any wording".into(),
    }];
    assert_eq!(
        attention_push(&report, probe),
        Push::Attention {
            items: vec![Attention::SoundFailed {
                line: "any wording".into(),
                stop: true
            }]
        }
    );
    report.sounds[0].kind = SoundWarningKind::SlowOpen;
    assert_eq!(
        attention_push(&report, probe),
        Push::Attention { items: Vec::new() }
    );
}

#[test]
fn a_history_page_is_capped_and_continues_after_its_last_id() {
    let item = |n: usize| HistoryItem {
        id: {
            let s = 86_399 - n;
            hid(&format!(
                "2026-11-10/{:02}{:02}{:02}-000",
                s / 3600,
                s / 60 % 60,
                s % 60
            ))
        },
        status: oxi_core::history::Status::Done,
        duration_ms: 1,
        model: String::new(),
        language: String::new(),
        text: Spoken("t".into()),
        held: Vec::new(),
    };
    let all: Vec<_> = (0..HISTORY_PAGE_ITEMS + 5).map(item).collect();
    let (first, more) = history_page(all.clone(), None);
    assert_eq!((first.len(), more), (HISTORY_PAGE_ITEMS, true));
    let (rest, more) = history_page(all, first.last().map(|i| i.id.as_str()));
    assert_eq!((rest.len(), more), (5, false));
    // One huge text is still sent alone, and the next item waits for the next page.
    let huge = HistoryItem {
        text: Spoken("x".repeat(HISTORY_PAGE_BYTES + 1)),
        ..item(0)
    };
    let (page, more) = history_page([huge, item(1)], None);
    assert_eq!((page.len(), more), (1, true));
    assert_eq!(history_page([], None), (Vec::new(), false));
}

#[test]
fn the_microphone_is_missing_when_the_chosen_one_or_any_is_not_listed() {
    let mics = vec!["USB Mic".to_owned()];
    assert!(!mic_missing("", &mics));
    assert!(mic_missing("", &[]));
    assert!(!mic_missing("USB Mic", &mics));
    assert!(mic_missing("Other", &mics));
}

#[test]
fn second_launch_opens_settings_and_is_closed() {
    let rig = rig("second");
    // `bind_at` on a served name forwards and returns once the daemon closed the connection.
    assert!(matches!(bind_at(&rig.name), Ok(Start::Forwarded)));
    assert_eq!(rig.calls.recv_timeout(WAIT).unwrap(), Call::Open);
}

#[test]
fn open_settings_raises_a_connected_window() {
    let rig = rig("raise");
    let mut window = settings(&rig);
    forward_open_settings(&rig.name).unwrap();
    assert_eq!(window.recv::<Push>().unwrap(), Some(Push::Raise));
    // The window may take the focus: allowed for the client's own pid (here this process).
    assert_eq!(rig.calls.try_recv(), Ok(Call::Allow(std::process::id())));
    assert!(rig.calls.try_recv().is_err(), "no second window");
}

#[test]
fn preview_sound_goes_to_the_coordinator() {
    let rig = rig("preview-sound");
    let mut conn = settings(&rig);
    conn.send(&Request::PreviewSound {
        choice: "pop".into(),
        volume_percent: 40,
    })
    .unwrap();
    assert!(matches!(
        rig.inbox.recv_timeout(WAIT),
        Ok(Inbox::Ipc(IpcAsk::PreviewSound { choice, volume_percent: 40 })) if choice == "pop"
    ));
}

#[test]
fn test_audio_stays_with_the_connection_and_goes_out_with_each_run() {
    let rig = rig("test-audio");
    let mut conn = settings(&rig);
    for (reset, samples) in [(true, &[0.9f32][..]), (true, &[0.1, 0.2]), (false, &[0.3])] {
        conn.send(&Request::TestAudio {
            reset,
            pcm: oxi_core::tuning::encode_pcm(samples),
        })
        .unwrap();
    }
    conn.send(&Request::TestTranscribe {
        id: 3,
        set: SettingsSet::defaults(),
        language: Language::EN,
    })
    .unwrap();
    assert!(matches!(
        rig.inbox.recv_timeout(WAIT),
        Ok(Inbox::Ipc(IpcAsk::Test(job)))
            if job.id == 3 && job.samples == [0.1, 0.2, 0.3] && !job.origin.is_stored_audio()
    ));
    drop(conn);
    assert!(matches!(
        rig.inbox.recv_timeout(WAIT),
        Ok(Inbox::Ipc(IpcAsk::TestDone))
    ));
}

#[test]
fn one_test_run_per_connection_the_rest_are_busy() {
    let rig = rig("test-one");
    let mut conn = settings(&rig);
    let run = |id| Request::TestTranscribe {
        id,
        set: SettingsSet::defaults(),
        language: Language::EN,
    };
    conn.send(&run(1)).unwrap();
    let Ok(Inbox::Ipc(IpcAsk::Test(job))) = rig.inbox.recv_timeout(WAIT) else {
        panic!("no first run");
    };
    conn.send(&run(2)).unwrap();
    let busy = Push::TestResult {
        id: 2,
        outcome: TestOutcome::Failed {
            why: TestFailure::Busy,
        },
    };
    assert_eq!(conn.recv::<Push>().unwrap(), Some(busy));
    assert!(matches!(rig.inbox.try_recv(), Err(TryRecvError::Empty)));

    // The answer frees the slot before the client can see it.
    let done = Push::TestResult {
        id: 1,
        outcome: TestOutcome::Failed {
            why: TestFailure::NoAudio,
        },
    };
    assert!(job.reply.send(done.clone()));
    assert_eq!(conn.recv::<Push>().unwrap(), Some(done));
    conn.send(&run(3)).unwrap();
    assert!(matches!(
        rig.inbox.recv_timeout(WAIT),
        Ok(Inbox::Ipc(IpcAsk::Test(job))) if job.id == 3
    ));
}

#[test]
fn empty_and_silent_connections_are_survived() {
    let rig = rig("empty");
    drop(Conn::connect(&rig.name).unwrap());
    let _idle = connect(&rig); // sends nothing
    let mut conn = settings(&rig);
    conn.send(&Request::PreviewOverlay {
        position: OverlayPosition::TopLeft,
    })
    .unwrap();
    assert!(matches!(
        rig.inbox.recv_timeout(WAIT),
        Ok(Inbox::Ipc(IpcAsk::PreviewOverlay(OverlayPosition::TopLeft)))
    ));
}

#[test]
fn only_hello_or_open_settings_may_start_a_connection() {
    let rig = rig("stranger");
    let mut conn = connect(&rig);
    conn.send(&Request::Shutdown).unwrap();
    assert!(conn.recv::<Push>().unwrap().is_none());
    assert!(matches!(rig.inbox.try_recv(), Err(TryRecvError::Empty)));
}

#[test]
fn oversized_line_closes_only_that_connection() {
    let rig = rig("oversize");
    let mut good = settings(&rig);
    let mut bad = connect(&rig);
    let mut line = "x".repeat(MAX_LINE + 2);
    line.push('\n');
    bad.send(&Raw(line)).unwrap();
    assert!(bad.recv::<Push>().map_or(true, |m| m.is_none()));
    good.send(&Request::ListDevices).unwrap();
    assert!(matches!(
        good.recv::<Push>().unwrap(),
        Some(Push::Devices { .. })
    ));
}

#[test]
fn requests_reach_their_owners() {
    let rig = rig("route");
    let mut conn = settings(&rig);
    assert_eq!(rig.io.recv_timeout(WAIT).unwrap(), IoCmd::Subscribed);
    // A second Hello on the same connection subscribes nothing new.
    conn.send(&Request::Hello {
        proto: PROTO,
        version: "test".into(),
    })
    .unwrap();
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::HelloAck { .. })
    ));
    conn.send(&Request::DownloadModel { id: "tiny".into() })
        .unwrap();
    conn.send(&Request::CancelDownload { id: "tiny".into() })
        .unwrap();
    assert_eq!(
        rig.io.recv_timeout(WAIT).unwrap(),
        IoCmd::Download { id: "tiny".into() }
    );
    assert_eq!(
        rig.io.recv_timeout(WAIT).unwrap(),
        IoCmd::CancelDownload { id: "tiny".into() }
    );
    conn.send(&Request::Shutdown).unwrap();
    assert!(matches!(rig.inbox.recv_timeout(WAIT), Ok(Inbox::Shutdown)));
    assert!(conn.recv::<Push>().unwrap().is_none());
}

#[test]
fn dropping_the_connection_ends_capture() {
    let rig = rig("drop");
    let mut conn = settings(&rig);
    conn.send(&Request::CaptureHotkey { on: true }).unwrap();
    assert_eq!(
        rig.calls.recv_timeout(WAIT).unwrap(),
        Call::Capture(true, true)
    );
    drop(conn);
    assert_eq!(
        rig.calls.recv_timeout(WAIT).unwrap(),
        Call::Capture(false, false)
    );
}

#[test]
fn a_reconnected_window_can_capture_again() {
    let rig = rig("recapture");
    for _ in 0..2 {
        let mut conn = settings(&rig);
        conn.send(&Request::CaptureHotkey { on: true }).unwrap();
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(true, true)
        );
        drop(conn);
        assert_eq!(
            rig.calls.recv_timeout(WAIT).unwrap(),
            Call::Capture(false, false)
        );
    }
}

#[test]
fn losing_focus_ends_capture_and_blocks_a_start() {
    let rig = rig("focus");
    let mut conn = settings(&rig);
    conn.send(&Request::CaptureHotkey { on: true }).unwrap();
    assert_eq!(
        rig.calls.recv_timeout(WAIT).unwrap(),
        Call::Capture(true, true)
    );
    rig.host.foreground.store(false, Ordering::Relaxed);
    assert_eq!(
        rig.calls.recv_timeout(WAIT).unwrap(),
        Call::Capture(false, false)
    );
    conn.send(&Request::CaptureHotkey { on: true }).unwrap();
    conn.send(&Request::ListDevices).unwrap();
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::Devices { .. })
    ));
    assert!(
        rig.calls.try_recv().is_err(),
        "capture must not start unfocused"
    );
}

#[test]
fn captured_keys_are_pushed() {
    let rig = rig("keys");
    let mut conn = settings(&rig);
    rig.keys.send((Key::F13, true)).unwrap();
    rig.keys.send((Key::F13, false)).unwrap();
    for down in [true, false] {
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::Key {
                code: Key::F13,
                down
            })
        );
    }
}

#[test]
fn connections_are_capped() {
    let rig = rig("cap");
    let idle: Vec<Conn> = (0..MAX_CONNS).map(|_| connect(&rig)).collect();
    let mut extra = connect(&rig);
    // A refused connection is closed without reading it.
    let hello = Request::Hello {
        proto: PROTO,
        version: "test".into(),
    };
    let _ = extra.send(&hello);
    assert!(extra.recv::<Push>().map_or(true, |m| m.is_none()));
    // The second launch of a full daemon must not read that as "settings opened".
    assert!(matches!(forward_open_settings(&rig.name), Err(Error::Busy)));
    drop(idle);
    // Slots free up as the threads see the closes.
    let deadline = Instant::now() + WAIT;
    loop {
        let mut conn = connect(&rig);
        if conn.send(&hello).is_ok()
            && matches!(conn.recv::<Push>(), Ok(Some(Push::HelloAck { .. })))
        {
            break;
        }
        assert!(Instant::now() < deadline, "no slot freed");
    }
}

#[test]
fn a_window_that_stops_reading_never_blocks_the_sender() {
    let rig = rig("stall");
    let _stuck = settings(&rig); // never reads again
    let big = Push::Status {
        phase: Phase::Idle,
        model: "tiny".into(),
        device: DeviceChoice::Auto,
        backend: "cpu".into(),
        loaded: true,
        warnings: vec!["w".repeat(64 * 1024)],
    };
    let start = Instant::now();
    let mut refused = false;
    for _ in 0..4000 {
        refused |= !rig.pushes.send(big.clone());
    }
    assert!(start.elapsed() < Duration::from_secs(1), "send blocked");
    assert!(refused, "the queue never filled");
}

#[test]
fn newer_window_replaces_the_old_one() {
    let rig = rig("replace");
    let mut old = settings(&rig);
    let mut new = settings(&rig);
    assert!(old.recv::<Push>().map_or(true, |m| m.is_none()));
    assert!(rig.pushes.send(Push::Raise));
    assert_eq!(new.recv::<Push>().unwrap(), Some(Push::Raise));
}

#[test]
fn an_unanswered_status_does_not_hold_up_the_focus_check() {
    let rig = rig("slowask");
    let mut conn = settings(&rig);
    conn.send(&Request::CaptureHotkey { on: true }).unwrap();
    assert_eq!(
        rig.calls.recv_timeout(WAIT).unwrap(),
        Call::Capture(true, true)
    );
    // The coordinator never answers.
    conn.send(&Request::Status).unwrap();
    assert!(matches!(
        rig.inbox.recv_timeout(WAIT),
        Ok(Inbox::Ipc(IpcAsk::Status(_)))
    ));
    let lost = Instant::now();
    rig.host.foreground.store(false, Ordering::Relaxed);
    assert_eq!(
        rig.calls.recv_timeout(WAIT).unwrap(),
        Call::Capture(false, false)
    );
    assert!(
        lost.elapsed() < Duration::from_millis(500),
        "{:?}",
        lost.elapsed()
    );
}

/// Waits until the fake hook stops swallowing.
fn capture_ends(rig: &Rig) -> bool {
    let deadline = Instant::now() + WAIT;
    while rig.host.capturing.load(Ordering::SeqCst) {
        if Instant::now() > deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(5));
    }
    true
}

#[test]
fn focus_lost_while_capture_turns_on_still_ends_capture() {
    let rig = rig("slowon");
    // The thread that turns capture on is held up (preempted) for longer than a focus poll.
    rig.host.on_delay.store(150, Ordering::Relaxed);
    let mut conn = settings(&rig);
    conn.send(&Request::CaptureHotkey { on: true }).unwrap();
    thread::sleep(Duration::from_millis(30));
    rig.host.foreground.store(false, Ordering::Relaxed);
    thread::sleep(Duration::from_millis(250));
    assert!(capture_ends(&rig), "keys still swallowed after focus loss");
    drop(conn);
    assert!(capture_ends(&rig), "keys still swallowed after disconnect");
}

#[test]
fn capture_is_off_once_the_connection_is_gone() {
    #[derive(Clone, Copy, Debug)]
    enum Op {
        On,
        Off,
        FocusLost,
        FocusBack,
    }
    use Op::{FocusBack, FocusLost, Off, On};
    let sequences: [&[Op]; 7] = [
        &[On],
        &[On, FocusLost],
        &[On, FocusLost, FocusBack, On],
        &[FocusLost, On, FocusBack, On],
        &[On, Off, On],
        &[On, FocusLost, Off, FocusBack, On, Off],
        &[On, On, FocusLost, FocusBack, FocusLost],
    ];
    for (i, ops) in sequences.into_iter().enumerate() {
        let rig = rig(&format!("inv{i}"));
        rig.host.on_delay.store(25, Ordering::Relaxed);
        let mut conn = settings(&rig);
        for op in ops {
            match op {
                On => conn.send(&Request::CaptureHotkey { on: true }).unwrap(),
                Off => conn.send(&Request::CaptureHotkey { on: false }).unwrap(),
                FocusLost => rig.host.foreground.store(false, Ordering::Relaxed),
                FocusBack => rig.host.foreground.store(true, Ordering::Relaxed),
            }
            thread::sleep(Duration::from_millis(30));
        }
        drop(conn);
        assert!(capture_ends(&rig), "keys swallowed after {ops:?}");
    }
}

#[test]
fn a_late_status_answer_still_arrives() {
    let rig = rig("lateask");
    let mut conn = settings(&rig);
    conn.send(&Request::Status).unwrap();
    let Ok(Inbox::Ipc(IpcAsk::Status(reply))) = rig.inbox.recv_timeout(WAIT) else {
        panic!("no Status ask");
    };
    thread::sleep(Duration::from_millis(300));
    reply
        .send(StatusReport {
            phase: Phase::Idle,
            model: "tiny".into(),
            device: String::new(),
            gpu: DeviceChoice::Auto,
            backend: "none".into(),
            model_failure: None,
            warnings: vec![],
            sounds: vec![],
        })
        .unwrap();
    assert!(matches!(
        conn.recv::<Push>().unwrap(),
        Some(Push::Status { .. })
    ));
}

#[test]
fn only_the_settings_client_counts_as_foreground() {
    // (foreground, client, own, expected)
    let cases = [
        (100, 100, 1, true),
        (1, 100, 1, false),   // the daemon's own window, such as the tray menu
        (1, 1, 1, false),     // a client that is the daemon itself
        (200, 100, 1, false), // another app
        (0, 0, 1, false),     // no foreground window, no known client
    ];
    for (fg, client, own, want) in cases {
        assert_eq!(is_settings(fg, client, own), want, "{fg} {client} {own}");
    }
}

#[test]
fn refusals_say_why() {
    let cases = [
        (Error::Busy, "not answering"),
        (Error::ForeignServer, "another account"),
        (Error::Untrusted, "untrusted"),
        (Error::AlreadyRunning, "already running"),
        (
            Error::Io(io::ErrorKind::TimedOut.into()),
            "did not answer in time",
        ),
    ];
    for (error, text) in cases {
        let msg = refusal(error).to_string();
        assert!(msg.contains(text), "{msg:?} lacks {text:?}");
    }
}
/// The window nobody connected is never read for: the push is not even built.
#[test]
fn nothing_is_built_for_a_window_that_is_not_there() {
    let _only_one = lock(&test_bus::BUS_TEST);
    announce_with(|| panic!("built with no bus"));
    let pushes = Pushes::default();
    *lock(&BUS) = Some(pushes);
    announce_with(|| panic!("built with no connection"));
    *lock(&BUS) = None;
}

#[test]
fn a_stats_write_is_pushed_to_the_window() {
    use oxi_core::stats::Date;
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-ipc-stats-{}", std::process::id()));
    let bus = OnBus::connected();
    let today = || Date::from_ymd(2026, 11, 7).unwrap();
    // Another test's stats thread may write while the bus is set (the stats tests run in
    // parallel): its push is there first, and only this test's word count picks ours.
    announce(Push::Stats(
        oxi_core::stats::Stats::default().summary(today(), 1),
    ));
    let recorder =
        super::super::stats::Recorder::spawn(dir.join(oxi_core::stats::FILE), today).unwrap();
    recorder.add_voiced(
        4099,
        oxi_core::types::Millis(1_200),
        oxi_core::types::Millis(0),
    );
    drop(recorder); // waits for the write
    let summary = bus.next(2000, |p| match p {
        Push::Stats(s) if s.words == 4099 => Some(s),
        _ => None,
    });
    let summary = summary.expect("no Stats push after a write");
    assert_eq!((summary.dictations, summary.words), (1, 4099));
}

#[test]
fn an_edit_of_a_watched_file_is_pushed_to_the_window() {
    use oxi_core::ipc::WatchedFile;
    use oxi_core::paths::Layout;
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-ipc-watch-{}", std::process::id()));
    let layout = Layout {
        config_dir: dir.clone(),
        data_dir: dir.clone(),
        logs_dir: dir,
    };
    let bus = OnBus::connected();
    let (tx, _rx) = mpsc::channel();
    let _watcher = super::super::watch::spawn(&layout, tx, |_| {}).unwrap();
    oxi_platform::fs::write_atomic(&layout.config_file(), b"version = 2\n").unwrap();
    let seen = |want: WatchedFile| {
        bus.next(5000, |p| match p {
            Push::FileChanged { file } if file == want => Some(()),
            _ => None,
        })
    };
    assert!(
        seen(WatchedFile::Config).is_some(),
        "no FileChanged for the config"
    );
    oxi_platform::fs::write_atomic(&layout.dictionary_file(), b"word\n").unwrap();
    assert!(
        seen(WatchedFile::Dictionary).is_some(),
        "no FileChanged for the dictionary"
    );
}

#[test]
fn a_search_is_answered_with_its_own_seq_and_a_file_with_history_off_frees_the_slot() {
    let rig = rig("search-file");
    let mut conn = settings(&rig);
    let ask = |seq, query: &str| Request::HistorySearch {
        seq,
        query: Spoken(query.into()),
        day: None,
        min_ms: None,
        max_ms: None,
        older_than: None,
    };
    conn.send(&ask(3, "words")).unwrap();
    let Some(Push::HistoryFound { seq, items, more }) = conn.recv::<Push>().unwrap() else {
        panic!("no HistoryFound");
    };
    assert_eq!((seq, items.len(), more), (3, 2, false));
    conn.send(&ask(4, "absent")).unwrap();
    let Some(Push::HistoryFound { seq, items, .. }) = conn.recv::<Push>().unwrap() else {
        panic!("no HistoryFound");
    };
    assert_eq!((seq, items.len()), (4, 0));
    // History is off for the fake host: each run ends at once, and the next is not Busy.
    for run in [1, 2] {
        conn.send(&Request::TranscribeFile {
            run,
            path: "none.mp3".into(),
            language: Language::RU,
        })
        .unwrap();
        assert_eq!(
            conn.recv::<Push>().unwrap(),
            Some(Push::FileResult {
                run,
                result: FileResult::Failed {
                    why: FileFailure::HistoryOff
                }
            })
        );
    }
}

#[test]
fn a_file_run_stops_when_its_window_closes() {
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-file-close-{}", std::process::id()));
    let rig = rig("file-close");
    rig.host.file_on.store(true, Ordering::SeqCst);
    let mut conn = settings(&rig);
    conn.send(&Request::TranscribeFile {
        run: 1,
        path: tone_gap_tone(&dir).to_string_lossy().into_owned(),
        language: Language::RU,
    })
    .unwrap();
    let Ok(Inbox::Ipc(IpcAsk::Test(job))) = rig.inbox.recv_timeout(WAIT) else {
        panic!("no piece");
    };
    // The next window claims the slot before the old connection's thread has ended, as when
    // the window is reopened at once: without the run's stop flag, its pushes land here (a
    // released slot alone does not stop them).
    let mut next = settings(&rig);
    next.set_read_timeout(Some(Duration::from_millis(1)))
        .unwrap();
    drop(conn);
    // The run gives up its wait for the piece, so nothing of it reaches the next window.
    let progress = || Push::TestProgress {
        id: 1,
        percent: 10,
        est_ms: None,
    };
    let until = Instant::now() + WAIT;
    // The run looks at its stop flag only between pushes (a wait of POLL), so progress comes
    // slower than that; the new window reads what reaches it, so a full queue cannot end the run.
    while job.reply.send(progress()) && Instant::now() < until {
        thread::sleep(POLL * 2);
        let _ = next.recv::<Push>();
    }
    assert!(
        !job.reply.send(progress()),
        "the closed window's file run goes on"
    );
}

/// A 16 kHz mono wav of tone, a gap, tone: 1 s, 0.4 s, 1.6 s.
fn tone_gap_tone(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("talk.wav");
    let samples: Vec<f32> = (0..48_000_u32)
        .map(|i| {
            let quiet = (16_000..22_400).contains(&i);
            if quiet {
                0.0
            } else {
                (i as f32 * 0.17).sin() * 0.5
            }
        })
        .collect();
    oxi_platform::wav::write_wav(&path, &samples).unwrap();
    path
}

struct FileRig {
    pushed: Mutex<Vec<Push>>,
    pieces: Mutex<Vec<usize>>,
    saved: Mutex<Vec<String>>,
    cancel: AtomicBool,
    busy_once: AtomicBool,
    cancel_after_first: bool,
    /// What the decode gives instead of the file, as a crashed or stopped child does.
    decoded: Mutex<Option<oxi_platform::decode::DecodeError>>,
}

impl FileRig {
    fn new() -> Self {
        Self {
            pushed: Mutex::new(Vec::new()),
            pieces: Mutex::new(Vec::new()),
            saved: Mutex::new(Vec::new()),
            cancel: AtomicBool::new(false),
            busy_once: AtomicBool::new(false),
            cancel_after_first: false,
            decoded: Mutex::new(None),
        }
    }

    fn run(&self, path: &std::path::Path) -> FileResult {
        let push = |p| {
            lock(&self.pushed).push(p);
            true
        };
        let chunk = |_: &SettingsSet,
                     _: Language,
                     samples: &[f32],
                     on: &mut dyn FnMut(u8, Option<u32>) -> bool| {
            if self.busy_once.swap(false, Ordering::SeqCst) {
                return Chunk::Busy;
            }
            let n = {
                let mut pieces = lock(&self.pieces);
                pieces.push(samples.len());
                pieces.len()
            };
            for percent in [0, 50, 100] {
                on(percent, (percent == 0).then_some(1000));
            }
            if self.cancel_after_first {
                self.cancel.store(true, Ordering::SeqCst);
            }
            Chunk::Text(format!("part{n}"))
        };
        let save = |samples: &[f32], text: &str| {
            lock(&self.saved).push(text.to_owned());
            Ok(HistoryItem {
                id: hid("2026-11-10/101500-123"),
                status: oxi_core::history::Status::Done,
                duration_ms: samples.len() as u64 / 16,
                model: String::new(),
                language: String::new(),
                text: Spoken(text.into()),
                held: Vec::new(),
            })
        };
        let decode = |path: &std::path::Path| match lock(&self.decoded).take() {
            Some(e) => Err(e),
            None => oxi_platform::decode::decode_file(path, oxi_core::audio::SAMPLE_RATE),
        };
        let env = FileEnv {
            set: Some(SettingsSet::defaults()),
            decode: &decode,
            push: &push,
            chunk: &chunk,
            save: &save,
            cancel: &self.cancel,
            max: oxi_core::types::Millis(1500),
            busy_wait: Duration::ZERO,
        };
        file_run(&env, 7, path, Language::RU)
    }

    fn percents(&self) -> Vec<u8> {
        lock(&self.pushed)
            .iter()
            .filter_map(|p| match p {
                Push::TestProgress { percent, .. } => Some(*percent),
                _ => None,
            })
            .collect()
    }
}

#[test]
fn a_file_is_cut_run_piece_by_piece_with_real_fractions_and_saved_once() {
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-file-run-{}", std::process::id()));
    let rig = FileRig::new();
    rig.busy_once.store(true, Ordering::SeqCst);
    let done = rig.run(&tone_gap_tone(&dir));
    assert_eq!(
        done,
        FileResult::Done {
            id: hid("2026-11-10/101500-123")
        }
    );
    let pieces = lock(&rig.pieces).clone();
    assert!(pieces.len() >= 2, "{pieces:?}");
    assert!(
        pieces.iter().all(|&n| n <= 1500 * 16),
        "none over the maximum"
    );
    assert_eq!(
        pieces.iter().sum::<usize>(),
        48_000,
        "the pieces cover the audio once"
    );
    // One busy answer was retried, not skipped; the texts are joined in order, saved once.
    let want: Vec<String> = (1..=pieces.len()).map(|n| format!("part{n}")).collect();
    assert_eq!(*lock(&rig.saved), [want.join(" ")]);
    // Fractions rise from 0 to 100 with a step for every piece and every report inside it.
    let percents = rig.percents();
    assert!(percents.windows(2).all(|w| w[0] <= w[1]), "{percents:?}");
    assert_eq!((percents.first(), percents.last()), (Some(&0), Some(&100)));
    assert!(
        percents.iter().any(|p| (10..90).contains(p)),
        "{percents:?}"
    );
    // The record is announced before the end.
    let pushed = lock(&rig.pushed);
    assert!(matches!(pushed.last(), Some(Push::HistoryAdded { .. })));
    // The first estimate is scaled from the piece to the whole file.
    let Some(Push::TestProgress {
        est_ms: Some(est), ..
    }) = pushed.first()
    else {
        panic!("no estimate");
    };
    assert!(*est > 1000, "{est}");
}

#[test]
fn a_cancel_between_pieces_saves_nothing_and_a_bad_file_names_its_reason() {
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-file-bad-{}", std::process::id()));
    let mut rig = FileRig::new();
    rig.cancel_after_first = true;
    assert_eq!(rig.run(&tone_gap_tone(&dir)), FileResult::Cancelled);
    assert_eq!(lock(&rig.pieces).len(), 1);
    assert!(lock(&rig.saved).is_empty());
    let rig = FileRig::new();
    let why = |path: &std::path::Path| match rig.run(path) {
        FileResult::Failed { why } => why,
        other => panic!("{other:?}"),
    };
    assert_eq!(why(&dir.join("none.mp3")), FileFailure::Unreadable);
    let junk = dir.join("junk.mp3");
    std::fs::write(&junk, b"not audio at all, just words").unwrap();
    assert_ne!(why(&junk), FileFailure::Unreadable);
    assert!(lock(&rig.saved).is_empty() && lock(&rig.pieces).is_empty());
}

#[test]
fn a_decoding_child_that_crashed_or_was_stopped_ends_the_run_without_a_piece() {
    use oxi_platform::decode::DecodeError;
    let dir = crate::log::scratch::scratch_dir(&format!("oxi-file-child-{}", std::process::id()));
    let rig = FileRig::new();
    *lock(&rig.decoded) = Some(DecodeError::Corrupt);
    assert_eq!(
        rig.run(&tone_gap_tone(&dir)),
        FileResult::Failed {
            why: FileFailure::Corrupt
        }
    );
    *lock(&rig.decoded) = Some(DecodeError::Stopped);
    assert_eq!(rig.run(&tone_gap_tone(&dir)), FileResult::Cancelled);
    assert!(lock(&rig.saved).is_empty() && lock(&rig.pieces).is_empty());
}

#[test]
fn a_stop_is_seen_while_progress_arrives_faster_than_the_poll() {
    let (inbox_tx, inbox) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let feeder = {
        let cancel = Arc::clone(&cancel);
        thread::spawn(move || {
            let Ok(Inbox::Ipc(IpcAsk::Test(job))) = inbox.recv() else {
                panic!("no job");
            };
            // A message every millisecond, far inside POLL, so no receive ever times out.
            for percent in 0..200u8 {
                job.reply.send(Push::TestProgress {
                    id: job.id,
                    percent: percent % 100,
                    est_ms: None,
                });
                if percent == 5 {
                    cancel.store(true, Ordering::Release);
                }
                thread::sleep(Duration::from_millis(1));
            }
        })
    };
    let started = Instant::now();
    let end = run_chunk(
        &inbox_tx,
        1,
        &SettingsSet::defaults(),
        Language::AUTO,
        &[0.1],
        &mut |_, _| true,
        &cancel,
    );
    assert!(matches!(end, Chunk::Stopped));
    assert!(started.elapsed() < Duration::from_millis(150));
    feeder.join().unwrap();
}
