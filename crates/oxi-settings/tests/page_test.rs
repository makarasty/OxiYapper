//! Test page under the testing backend, on a temporary config directory, a fake daemon link and a
//! fake microphone.

mod common;

use std::cell::Cell;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use common::{assert_nothing_moved, config, has, layout, open_linked, spots, with_role};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, mock_elapsed_time};
use oxi_core::config::Audio;
use oxi_core::ipc::{Push, Request, TestFailure, TestOutcome};
use oxi_core::paths::Layout;
use oxi_core::tuning::{self, PCM_CHUNK_SAMPLES, Profile, SettingsSet, word_errors};
use oxi_core::types::Language;
use oxi_platform::capture::CaptureError;
use oxi_settings::ipc::{Cmd, Event, Link};
use oxi_settings::{
    AppWindow, Ctx, Finish, Ipc, Page, TestState, ThemeChoice, Toast, bind_test_page,
};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model};

const MIC_SAMPLES: usize = 16_000;

thread_local! {
    /// How many samples the next fake recording gives.
    static SAMPLES: Cell<usize> = const { Cell::new(MIC_SAMPLES) };
    /// Pulls the plug of the fake recording that is open.
    static PLUG: Cell<Option<Arc<AtomicBool>>> = const { Cell::new(None) };
}

#[expect(clippy::unnecessary_wraps, reason = "has the Mic signature")]
fn fake_mic(_: &Audio, lost: Arc<AtomicBool>) -> Result<Finish, CaptureError> {
    PLUG.set(Some(lost));
    let samples = SAMPLES.get();
    Ok(Box::new(move || Ok(vec![0.1; samples])))
}

fn fake_no_mic(_: &Audio, _: Arc<AtomicBool>) -> Result<Finish, CaptureError> {
    Err(CaptureError::NoDevice)
}

struct Fixture {
    window: AppWindow,
    ipc: Ipc,
    sent: Receiver<Cmd>,
    layout: Layout,
    ctx: Ctx,
}

impl Fixture {
    fn new(name: &str, config: Option<&str>) -> Self {
        Self::with_mic(name, config, fake_mic)
    }

    fn with_mic(name: &str, config: Option<&str>, mic: oxi_settings::Mic) -> Self {
        SAMPLES.set(MIC_SAMPLES);
        let layout = layout(name, config);
        let (ipc, sent) = Ipc::fake();
        let window = open_linked(&layout, Page::Test, ipc.clone());
        let (mut ctx, _) = Ctx::load(layout.clone()).unwrap();
        ctx.ipc = ipc.clone();
        bind_test_page(&window, &ctx, mic);
        // The rebind above replaced what the first bind drew.
        window.global::<TestState>().invoke_shown();
        Self {
            window,
            ipc,
            sent,
            layout,
            ctx,
        }
    }

    fn state(&self) -> TestState<'_> {
        self.window.global::<TestState>()
    }

    fn record_and_stop(&self) {
        self.state().invoke_record();
        self.state().invoke_record();
    }

    fn take_sent(&self) -> Vec<Request> {
        self.sent
            .try_iter()
            .filter_map(|cmd| match cmd {
                Cmd::Send(request) => Some(request),
                Cmd::Start | Cmd::Restart => None,
            })
            .collect()
    }

    /// The one `TestTranscribe` that is out: nothing else was sent since the last look.
    fn one_transcribe(&self) -> (u32, SettingsSet) {
        let mut sent = self.take_sent();
        assert_eq!(sent.len(), 1, "exactly one set at a time");
        match sent.remove(0) {
            Request::TestTranscribe { id, set, .. } => (id, set),
            other => panic!("expected TestTranscribe, got {other:?}"),
        }
    }

    fn answer(&self, id: u32, outcome: TestOutcome) {
        self.ipc
            .deliver(&Event::Push(Push::TestResult { id, outcome }));
    }

    fn errors(&self) -> Vec<String> {
        self.state()
            .get_rows()
            .iter()
            .map(|r| r.errors.to_string())
            .collect()
    }

    fn toast(&self) -> String {
        self.window.global::<Toast>().get_text().to_string()
    }
}

/// A repeating timer fires once per mocked step, so step at its own interval.
fn tick_for(total: Duration) {
    const STEP: Duration = Duration::from_millis(200);
    for _ in 0..total.as_millis() / STEP.as_millis() {
        mock_elapsed_time(STEP);
    }
}

fn heard(text: &str) -> TestOutcome {
    TestOutcome::Heard {
        text: text.to_owned(),
        ms: 1500,
    }
}

fn failed(why: TestFailure) -> TestOutcome {
    TestOutcome::Failed { why }
}

fn first_phrase() -> &'static str {
    tuning::phrases(Language::RU)[0]
}

#[test]
fn starts_with_a_ready_phrase_and_run_off() {
    let f = Fixture::new("empty", None);
    let s = f.state();
    assert_eq!(s.get_phrase_text(), first_phrase());
    assert!(!s.get_own());
    let phrases: Vec<_> = s.get_phrases().iter().map(|p| p.to_string()).collect();
    assert_eq!(phrases.last().unwrap(), "My own phrase");
    assert_eq!(phrases.len(), tuning::phrases(Language::RU).len() + 1);
    assert_eq!(s.get_profiles().row_count(), Profile::ALL.len());
    assert_eq!(s.get_take(), "Nothing recorded yet.");
    assert_eq!(s.get_run_off(), "Record the phrase first");
    s.invoke_run();
    assert!(f.take_sent().is_empty(), "no recording, nothing to send");
}

#[test]
fn a_language_with_no_ready_phrases_asks_for_your_own() {
    let f = Fixture::new("auto", Some("version = 1\n[model]\nlanguage = \"auto\"\n"));
    let s = f.state();
    assert!(s.get_own(), "only \"My own phrase\" is left");
    assert_eq!(s.get_phrases().row_count(), 1);
    f.record_and_stop();
    assert_eq!(s.get_run_off(), "Write the phrase first");
    s.set_own_text("  mine  ".into());
    s.invoke_own_edited("  mine  ".into());
    assert_eq!(s.get_run_off(), "");
}

#[test]
fn record_and_stop_hold_the_audio_and_show_its_length() {
    let f = Fixture::new("record", None);
    let s = f.state();
    s.invoke_record();
    assert!(s.get_recording());
    assert_eq!(s.get_run_off(), "Stop the recording first");
    tick_for(Duration::from_secs(2));
    assert_eq!(s.get_take(), "Recording 0:02");
    s.invoke_record();
    assert!(!s.get_recording());
    assert_eq!(s.get_take(), "Recorded 0:01");
    assert_eq!(s.get_run_off(), "");
}

#[test]
fn no_microphone_says_so_and_records_nothing() {
    let f = Fixture::with_mic("no-mic", None, fake_no_mic);
    f.state().invoke_record();
    assert!(!f.state().get_recording());
    assert_eq!(f.toast(), "No microphone");
}

#[test]
fn leaving_the_page_ends_the_recording_and_keeps_it() {
    let f = Fixture::new("leave", None);
    f.state().invoke_record();
    f.window.set_page(Page::About);
    mock_elapsed_time(Duration::from_millis(300));
    assert!(!f.state().get_recording());
    assert_eq!(f.state().get_take(), "Recorded 0:01");
}

#[test]
fn a_recording_stops_at_the_limit_of_the_engine() {
    let f = Fixture::new("limit", None);
    f.state().invoke_record();
    tick_for(Duration::from_secs(300));
    assert!(!f.state().get_recording());
    assert_eq!(f.toast(), "Recording stopped at 5 minutes.");
}

#[test]
fn another_phrase_drops_the_take_and_says_so() {
    const GONE: &str = "The phrase changed, so the recording is gone. Record it again.";
    let f = Fixture::new("other-phrase", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_phrase_picked(0);
    assert_eq!(
        s.get_take(),
        "Recorded 0:01",
        "the same phrase picked again"
    );

    s.set_phrase(1);
    s.invoke_phrase_picked(1);
    assert_eq!(s.get_take(), "Nothing recorded yet.");
    assert_eq!(s.get_run_off(), "Record the phrase first");
    assert_eq!(f.toast(), GONE);

    s.invoke_record();
    s.set_phrase(0);
    s.invoke_phrase_picked(0);
    assert!(!s.get_recording(), "an open take goes too");
    assert_eq!(s.get_take(), "Nothing recorded yet.");
}

#[test]
fn another_language_drops_the_take() {
    let f = Fixture::new("other-language", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_shown();
    assert_eq!(s.get_take(), "Recorded 0:01", "reopening the page keeps it");

    f.ctx.config.borrow_mut().config.model.language = Language::EN;
    s.invoke_shown();
    assert_eq!(s.get_take(), "Nothing recorded yet.");
    assert_eq!(
        f.toast(),
        "The phrase changed, so the recording is gone. Record it again."
    );
}

#[test]
fn a_lost_microphone_stops_the_recording_and_keeps_it() {
    let f = Fixture::new("unplugged", None);
    let s = f.state();
    s.invoke_record();
    tick_for(Duration::from_secs(2));
    PLUG.take().unwrap().store(true, Ordering::Relaxed);
    tick_for(Duration::from_millis(200));
    assert!(!s.get_recording());
    assert_eq!(s.get_take(), "Recorded 0:01");
    assert_eq!(s.get_run_off(), "");
    assert_eq!(
        f.toast(),
        "Microphone disconnected. What was recorded is kept."
    );
}

#[test]
fn an_own_phrase_without_a_word_cannot_run() {
    let f = Fixture::new(
        "no-words",
        Some("version = 1\n[model]\nlanguage = \"auto\"\n"),
    );
    let s = f.state();
    f.record_and_stop();
    s.set_own_text("...".into());
    s.invoke_own_edited("...".into());
    assert_eq!(s.get_run_off(), "Write the phrase first");
    s.invoke_run();
    assert!(f.take_sent().is_empty(), "a phrase of no words is not sent");
    s.set_own_text("one word...".into());
    s.invoke_own_edited("one word...".into());
    assert_eq!(s.get_run_off(), "", "and the take was kept");
}

fn press(window: &AppWindow, key: Key) {
    let text = slint::SharedString::from(key);
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text });
}

fn ctrl_tab(window: &AppWindow) {
    let ctrl = slint::SharedString::from(Key::Control);
    window
        .window()
        .dispatch_event(WindowEvent::KeyPressed { text: ctrl.clone() });
    press(window, Key::Tab);
    window
        .window()
        .dispatch_event(WindowEvent::KeyReleased { text: ctrl });
}

/// Run sits in the view it replaces: the focus must not go with the button.
#[test]
fn ctrl_tab_works_after_run_from_the_keyboard() {
    let f = Fixture::new("run-keys", None);
    f.record_and_stop();
    // The rail, the view switch, Phrase, Record, Scenario, Run.
    for _ in 0..6 {
        press(&f.window, Key::Tab);
    }
    for space in [
        WindowEvent::KeyPressed { text: " ".into() },
        WindowEvent::KeyReleased { text: " ".into() },
    ] {
        f.window.window().dispatch_event(space);
    }
    assert_eq!(f.state().get_view(), 1, "Space on Run opened the results");
    mock_elapsed_time(Duration::from_millis(1));
    ctrl_tab(&f.window);
    assert_eq!(f.window.get_page(), Page::System);
}

/// An edit of another setting from outside rebinds the pages, and this one must go on.
#[test]
fn an_outside_config_edit_keeps_the_take_and_the_run() {
    let f = Fixture::new("reload", None);
    let s = f.state();
    s.invoke_record();
    fs::write(f.layout.config_file(), "version = 1\n# one\n").unwrap();
    mock_elapsed_time(oxi_settings::FILE_POLL);
    assert!(s.get_recording(), "an open take goes on");
    s.invoke_record();
    assert_eq!(s.get_take(), "Recorded 0:01");

    s.invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
        panic!("first set");
    };
    fs::write(f.layout.config_file(), "version = 1\n# one and two\n").unwrap();
    mock_elapsed_time(oxi_settings::FILE_POLL);
    assert_eq!(s.get_view(), 1, "the results stay open");
    assert_eq!(f.errors()[0], "Running…");
    assert_eq!(s.get_take(), "Recorded 0:01");
    f.answer(*id, heard(first_phrase()));
    assert_eq!(f.errors()[0], "0.0 %");
    assert_eq!(f.one_transcribe().1, Profile::General.sets()[1]);
}

#[test]
fn busy_with_the_last_test_is_not_called_dictating() {
    let f = Fixture::new("busy-self", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id: left, .. }) = sent.last() else {
        panic!("first set");
    };
    // Closing ends the run here, not on the daemon.
    f.window
        .window()
        .dispatch_event(WindowEvent::CloseRequested);
    f.record_and_stop();
    s.invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
        panic!("first set of the new run");
    };
    f.answer(*id, failed(TestFailure::Busy));
    assert_eq!(
        f.toast(),
        "The last test is still finishing. Run the test in a moment."
    );

    // Its answer comes and scores nothing; dictating is what a Busy means from then on.
    f.answer(*left, heard("text"));
    assert!(f.errors().iter().all(|e| e == "Not run"));
    s.invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
        panic!("first set of the third run");
    };
    f.answer(*id, failed(TestFailure::Busy));
    assert_eq!(
        f.toast(),
        "OxiYapper is dictating. Run the test when it is free."
    );
}

#[test]
fn the_flow_sends_one_set_at_a_time_and_apply_writes_the_best() {
    let f = Fixture::new("flow", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_run();
    assert_eq!(s.get_view(), 1, "the table opens");

    let mut requests = f.take_sent();
    let Request::TestAudio { reset, pcm } = requests.remove(0) else {
        panic!("the recording goes first");
    };
    assert!(reset);
    assert_eq!(pcm.len(), MIC_SAMPLES * 8);
    assert_eq!(requests.len(), 1, "one set, the others wait for its answer");
    let Request::TestTranscribe { id, set, language } = requests.remove(0) else {
        panic!("then the first set");
    };
    assert_eq!(language, Language::RU);
    assert_eq!(set, Profile::General.sets()[0]);
    assert_eq!(f.errors()[0], "Running…");
    assert_eq!(f.errors()[1], "Waiting");
    assert_eq!(s.get_run_off(), "The test is running");
    assert_eq!(s.get_record_off(), "The test is running");
    s.invoke_record();
    assert!(!s.get_recording(), "no recording during a run");

    // A word wrong, then every word right: the second set wins.
    let wrong = format!("{first} нет", first = first_phrase());
    f.answer(id, heard(&wrong));
    let percent = word_errors(first_phrase(), &wrong).percent();
    assert_eq!(f.errors()[0], format!("{percent:.1} %"));
    let rows: Vec<_> = s.get_rows().iter().collect();
    assert_eq!(rows[0].time, "1.5 s");
    assert_eq!(rows[0].model, "Whisper large-v3 turbo");
    assert_eq!(rows[0].normalize, "On");

    let (id, set) = f.one_transcribe();
    assert_eq!(set, Profile::General.sets()[1]);
    f.answer(id, heard(first_phrase()));
    let (id, _) = f.one_transcribe();
    f.answer(id, failed(TestFailure::ModelBad));
    let (id, _) = f.one_transcribe();
    f.answer(id, failed(TestFailure::ModelMissing));
    assert_eq!(f.errors()[2], "Model failed");
    assert_eq!(f.errors()[3], "Not downloaded");
    assert!(f.take_sent().is_empty(), "the run is over");

    let rows: Vec<_> = s.get_rows().iter().collect();
    assert!(rows[1].best && !rows[0].best && !rows[2].best);
    assert_eq!(s.get_best_text(), "Lowest error rate: 0.0 %");
    assert_eq!(s.get_apply_off(), "");
    assert_eq!(s.get_run_off(), "");

    s.invoke_apply();
    let want = &Profile::General.sets()[1];
    let saved = config(&f.layout).config;
    assert!(saved.audio.denoise, "the best set is the denoise one");
    assert_eq!(saved.audio.normalize, want.normalize);
    assert_eq!(saved.audio.trim, want.trim);
    assert_eq!(saved.model.id, want.model);
    assert_eq!(f.toast(), "Best set applied.");
}

#[test]
fn a_long_recording_goes_in_chunks_with_one_reset() {
    let f = Fixture::new("chunks", None);
    SAMPLES.set(PCM_CHUNK_SAMPLES * 2 + 1);
    f.record_and_stop();
    f.state().invoke_run();
    let sent = f.take_sent();
    let pieces: Vec<(bool, usize)> = sent
        .iter()
        .filter_map(|r| match r {
            Request::TestAudio { reset, pcm } => Some((*reset, pcm.len() / 8)),
            _ => None,
        })
        .collect();
    assert_eq!(
        pieces,
        [
            (true, PCM_CHUNK_SAMPLES),
            (false, PCM_CHUNK_SAMPLES),
            (false, 1)
        ]
    );
    assert!(matches!(sent[3], Request::TestTranscribe { .. }));
}

#[test]
fn the_scenario_picks_the_sets() {
    let f = Fixture::new("scenario", None);
    let i = Profile::ALL
        .iter()
        .position(|p| *p == Profile::BadMicrophone)
        .unwrap();
    f.state().set_profile(i32::try_from(i).unwrap());
    f.record_and_stop();
    f.state().invoke_run();
    assert_eq!(
        f.state().get_rows().row_count(),
        Profile::BadMicrophone.sets().len()
    );
}

#[test]
fn a_busy_daemon_ends_the_run_and_keeps_the_recording() {
    let f = Fixture::new("busy", None);
    f.record_and_stop();
    f.state().invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
        panic!("first set");
    };
    f.answer(*id, failed(TestFailure::Busy));
    assert!(f.take_sent().is_empty(), "no further set");
    assert!(f.errors().iter().all(|e| e == "Not run"));
    assert_eq!(
        f.toast(),
        "OxiYapper is dictating. Run the test when it is free."
    );
    assert_eq!(f.state().get_run_off(), "", "can run again");
    assert_ne!(f.state().get_apply_off(), "", "nothing to apply");
}

#[test]
fn an_answer_to_another_run_is_ignored() {
    let f = Fixture::new("stale", None);
    f.record_and_stop();
    f.state().invoke_run();
    let _ = f.take_sent();
    f.answer(999, heard("text"));
    assert_eq!(f.errors()[0], "Running…");
    assert_eq!(f.take_sent(), [] as [Request; 0]);
}

#[test]
fn offline_is_a_designed_state() {
    let f = Fixture::new("offline", None);
    f.ipc.deliver(&Event::Link(Link::Offline));
    f.record_and_stop();
    assert_eq!(
        f.state().get_take(),
        "Recorded 0:01",
        "recording needs no daemon"
    );
    assert_eq!(f.state().get_run_off(), "Start OxiYapper to run the test");
    f.state().invoke_run();
    assert_eq!(f.take_sent(), [] as [Request; 0]);
    f.ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(f.state().get_run_off(), "");
}

#[test]
fn losing_the_daemon_ends_the_run() {
    let f = Fixture::new("lost", None);
    f.record_and_stop();
    f.state().invoke_run();
    f.ipc.deliver(&Event::Link(Link::Offline));
    assert!(f.errors().iter().all(|e| e == "Not run"));
    assert_eq!(f.toast(), "OxiYapper stopped answering, so the test ended.");
}

/// The named path of "recordings are gone when the window closes": `PageState::forget`.
#[test]
fn closing_the_window_drops_the_recording() {
    let f = Fixture::new("close", None);
    f.record_and_stop();
    assert_eq!(f.state().get_run_off(), "");
    f.window
        .window()
        .dispatch_event(WindowEvent::CloseRequested);
    assert_eq!(f.state().get_take(), "Nothing recorded yet.");
    assert_eq!(f.state().get_run_off(), "Record the phrase first");
    f.state().invoke_run();
    assert!(f.take_sent().is_empty(), "nothing left to send");
}

#[test]
fn closing_while_recording_discards_it() {
    let f = Fixture::new("close-rec", None);
    f.state().invoke_record();
    f.window
        .window()
        .dispatch_event(WindowEvent::CloseRequested);
    assert!(!f.state().get_recording());
    assert_eq!(f.state().get_take(), "Nothing recorded yet.");
}

fn finish_a_run(f: &Fixture) {
    f.record_and_stop();
    f.state().invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
        panic!("first set");
    };
    f.answer(*id, heard(first_phrase()));
    while let Some(Request::TestTranscribe { id, .. }) = f.take_sent().pop() {
        f.answer(id, heard(first_phrase()));
    }
}

#[test]
fn a_read_only_config_keeps_the_test_but_not_apply() {
    let f = Fixture::new("locked", Some("version = 99\n"));
    finish_a_run(&f);
    assert_eq!(
        with_role(&f.window, "Apply best", AccessibleRole::Button).accessible_enabled(),
        Some(false)
    );
    f.state().set_view(0);
    assert_eq!(f.state().get_run_off(), "", "the test writes nothing");
    assert_eq!(
        with_role(&f.window, "Phrase", AccessibleRole::Combobox).accessible_enabled(),
        Some(true)
    );
}

#[test]
fn states_move_no_row() {
    let f = Fixture::new("no-move", None);
    f.window.window().set_size(LogicalSize::new(1280.0, 860.0));
    let before = spots(&f.window);
    f.state().invoke_record();
    assert_nothing_moved(&before, &spots(&f.window));
    f.state().invoke_record();
    assert_nothing_moved(&before, &spots(&f.window));

    let own = i32::try_from(tuning::phrases(Language::RU).len()).unwrap();
    f.state().set_phrase(own);
    f.state().invoke_phrase_picked(own);
    assert!(f.state().get_own());
    assert_nothing_moved(&before, &spots(&f.window));

    f.state().set_phrase(0);
    f.state().invoke_phrase_picked(0);
    f.state().set_view(1);
    let empty = spots(&f.window);
    f.state().set_view(0);
    finish_a_run(&f);
    assert_nothing_moved(&empty, &spots(&f.window));
}

fn lowest(window: &AppWindow) -> f32 {
    spots(window)
        .iter()
        .map(|(_, _, [_, y, _, h])| y + h)
        .fold(0.0, f32::max)
}

/// The page fits the window at the minimum size in its fullest state: five sets in the table.
#[test]
fn no_scroll_at_the_minimum_or_the_default_size() {
    let f = Fixture::new("no-scroll", None);
    let scenario = Profile::ALL
        .iter()
        .position(|p| *p == Profile::BadMicrophone)
        .unwrap();
    f.state().set_profile(i32::try_from(scenario).unwrap());
    finish_a_run(&f);
    for (w, h) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 800.0)] {
        f.window.window().set_size(LogicalSize::new(w, h));
        for view in [0, 1] {
            f.state().set_view(view);
            assert!(
                lowest(&f.window) <= h,
                "view {view} at {w}x{h}: bottom {}",
                lowest(&f.window)
            );
        }
        // The search skips what is scrolled away, so what must be there is counted too.
        f.state().set_view(0);
        spots(&f.window);
        assert!(has(&f.window, "Run"), "Run at {w}x{h}");
        f.state().set_view(1);
        spots(&f.window);
        let cells = ElementHandle::find_by_accessible_label(&f.window, "0.0 %").count();
        assert_eq!(cells, 5, "all five rows at {w}x{h}");
    }
}

/// Real-renderer shots of the page's states, for a person to read, off-screen and without taking
/// the focus: `OXI_TEST_SHOTS=<dir> cargo test -p oxi-settings --test page_test real_shots --
/// --test-threads=1` (the event loop needs the main thread). `real_window` cannot reach these
/// states: they come from a recording and a run.
#[cfg(windows)]
#[expect(clippy::too_many_lines, reason = "one script of shots")]
#[test]
fn real_shots_of_the_states() {
    use slint::{PhysicalPosition, Timer, TimerMode};
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::rc::Rc;

    const WH_CALLWNDPROC: i32 = 4;
    const WM_SHOWWINDOW: u32 = 0x0018;
    const GWL_EXSTYLE: i32 = -20;
    const WS_EX_NOACTIVATE: i32 = 0x0800_0000;
    /// Timer steps to wait after a state is set, before its snapshot.
    const SETTLE_STEPS: u32 = 4;
    const STEP: Duration = Duration::from_millis(200);

    #[repr(C)]
    struct CallWndProc {
        l: isize,
        w: usize,
        message: u32,
        hwnd: *mut c_void,
    }
    #[link(name = "user32")]
    unsafe extern "system" {
        fn SetWindowsHookExW(
            id: i32,
            proc: unsafe extern "system" fn(i32, usize, isize) -> isize,
            module: *mut c_void,
            thread: u32,
        ) -> *mut c_void;
        fn CallNextHookEx(hook: *mut c_void, code: i32, w: usize, l: isize) -> isize;
        fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
        fn SetWindowLongW(hwnd: *mut c_void, index: i32, value: i32) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThreadId() -> u32;
    }
    /// As in `examples/real_window.rs`: the window must not take the owner's focus.
    unsafe extern "system" fn on_message(code: i32, w: usize, l: isize) -> isize {
        // SAFETY: for HC_ACTION (0) `l` points at the system's live CWPSTRUCT.
        unsafe {
            let msg = &*(l as *const CallWndProc);
            if code == 0 && msg.message == WM_SHOWWINDOW && msg.w != 0 {
                let ex = GetWindowLongW(msg.hwnd, GWL_EXSTYLE);
                SetWindowLongW(msg.hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE);
            }
            CallNextHookEx(std::ptr::null_mut(), code, w, l)
        }
    }

    /// Answers every set with `text`, so the table fills in.
    fn answer_all(ipc: &Ipc, sent: &Receiver<Cmd>, text: &str) {
        let mut ms = 900;
        while let Ok(cmd) = sent.try_recv() {
            if let Cmd::Send(Request::TestTranscribe { id, .. }) = cmd {
                ms += 700;
                ipc.deliver(&Event::Push(Push::TestResult {
                    id,
                    outcome: TestOutcome::Heard {
                        text: text.to_owned(),
                        ms,
                    },
                }));
            }
        }
    }

    let Some(dir) = std::env::var_os("OXI_TEST_SHOTS").map(PathBuf::from) else {
        return;
    };
    fs::create_dir_all(&dir).unwrap();
    // SAFETY: `on_message` matches HOOKPROC and lives for the whole process.
    let hook = unsafe {
        SetWindowsHookExW(
            WH_CALLWNDPROC,
            on_message,
            std::ptr::null_mut(),
            GetCurrentThreadId(),
        )
    };
    assert!(!hook.is_null());

    oxi_settings::use_skia();
    let layout = layout("real-shots", None);
    let (ipc, sent) = Ipc::fake();
    let window = oxi_settings::open_with(layout.clone(), ipc.clone()).unwrap();
    let (mut ctx, _) = Ctx::load(layout).unwrap();
    ctx.ipc = ipc.clone();
    window.set_page(Page::Test);
    window
        .window()
        .set_position(PhysicalPosition::new(-20_000, -20_000));
    let bad_mic = i32::try_from(
        Profile::ALL
            .iter()
            .position(|p| *p == Profile::BadMicrophone)
            .unwrap(),
    )
    .unwrap();
    let long_own = "Long ".repeat(60);
    let (dark, light) = (ThemeChoice::Dark, ThemeChoice::Light);
    let shots: Vec<(&str, f32, f32, ThemeChoice)> = vec![
        ("empty", 720.0, 520.0, dark),
        ("empty", 1000.0, 700.0, dark),
        ("empty", 1280.0, 860.0, dark),
        ("recording", 720.0, 520.0, dark),
        ("recording", 1000.0, 700.0, dark),
        ("recording", 1280.0, 860.0, dark),
        ("results", 720.0, 520.0, dark),
        ("results", 1000.0, 700.0, dark),
        ("results", 1280.0, 860.0, dark),
        ("offline", 720.0, 520.0, dark),
        ("offline", 1000.0, 700.0, dark),
        ("offline", 1280.0, 860.0, dark),
        ("long-phrase", 720.0, 520.0, dark),
        ("empty", 1000.0, 700.0, light),
        ("results", 1000.0, 700.0, light),
    ];

    let timer = Timer::default();
    let next = Cell::new(0_usize);
    let wait = Cell::new(0_u32);
    let shots = Rc::new(RefCell::new(shots));
    let weak = window.as_weak();
    timer.start(TimerMode::Repeated, STEP, move || {
        let Some(window) = weak.upgrade() else { return };
        let i = next.get();
        let shots = shots.borrow();
        let Some(&(name, w, h, theme)) = shots.get(i) else {
            let _ = slint::quit_event_loop();
            return;
        };
        if wait.get() == 0 {
            wait.set(1);
            bind_test_page(&window, &ctx, fake_mic);
            let s = window.global::<TestState>();
            s.invoke_shown();
            ipc.deliver(&Event::Link(Link::Online));
            window.set_daemon_text("".into());
            window.set_daemon_start(false);
            window.set_theme(theme);
            window.window().set_size(LogicalSize::new(w, h));
            match name {
                "recording" => s.invoke_record(),
                "results" => {
                    s.set_profile(bad_mic);
                    s.invoke_record();
                    s.invoke_record();
                    s.invoke_run();
                    answer_all(&ipc, &sent, first_phrase());
                }
                "offline" => {
                    s.invoke_record();
                    s.invoke_record();
                    ipc.deliver(&Event::Link(Link::Offline));
                    window.set_daemon_text(oxi_core::strings::DAEMON_OFFLINE.into());
                    window.set_daemon_start(true);
                }
                "long-phrase" => {
                    let own = i32::try_from(s.get_phrases().row_count() - 1).unwrap();
                    s.set_phrase(own);
                    s.invoke_phrase_picked(own);
                    s.set_own_text(long_own.as_str().into());
                }
                _ => {}
            }
            return;
        }
        if wait.get() < SETTLE_STEPS {
            wait.set(wait.get() + 1);
            return;
        }
        let Ok(shot) = window.window().take_snapshot() else {
            return;
        };
        let size = tiny_skia::IntSize::from_wh(shot.width(), shot.height()).unwrap();
        let pixmap = tiny_skia::Pixmap::from_vec(shot.as_bytes().to_vec(), size).unwrap();
        let tone = if theme == ThemeChoice::Dark {
            "dark"
        } else {
            "light"
        };
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "whole window pixels"
        )]
        let file = dir.join(format!("{name}-{}-{tone}.png", w as u32));
        pixmap.save_png(file).unwrap();
        wait.set(0);
        next.set(i + 1);
    });
    window.run().unwrap();
}
