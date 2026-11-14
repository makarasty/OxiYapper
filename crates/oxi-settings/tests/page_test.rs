//! Test page under the testing backend, on a temporary config directory, a fake daemon link, a
//! fake microphone and a silent speaker.

mod common;

use std::any::Any;
use std::cell::Cell;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::thread;
use std::time::Duration;

use common::{
    assert_nothing_moved, config, fake_model, has, layout, open_linked, spots, with_role,
};
use i_slint_backend_testing::{AccessibleRole, ElementHandle, mock_elapsed_time};
use oxi_core::config::Audio;
use oxi_core::device::Device;
use oxi_core::history::{HistoryId, Status};
use oxi_core::ipc::{
    DownloadResult, HistoryItem, HistoryOp, Push, Request, Spoken, TestFailure, TestOutcome,
};
use oxi_core::models::{self, MODELS};
use oxi_core::paths::Layout;
use oxi_core::tuning::{self, Goal, PCM_CHUNK_SAMPLES, SettingsSet, word_errors};
use oxi_core::types::Language;
use oxi_platform::capture::CaptureError;
use oxi_settings::ipc::{Cmd, Event, Link};
use oxi_settings::{AppWindow, Ctx, Finish, Ipc, Page, TestState, Toast, bind_test_page};
use slint::platform::{Key, WindowEvent};
use slint::{ComponentHandle, LogicalSize, Model};

const MIC_SAMPLES: usize = 16_000;
const HISTORY_ID: &str = "2026-11-08/130005-123";
const OLDER_ID: &str = "2026-11-07/090000-000";

thread_local! {
    /// How many samples the next fake recording gives.
    static SAMPLES: Cell<usize> = const { Cell::new(MIC_SAMPLES) };
    /// Pulls the plug of the fake recording that is open.
    static PLUG: Cell<Option<Arc<AtomicBool>>> = const { Cell::new(None) };
    /// Clips the silent speaker was given.
    static PLAYED: Cell<usize> = const { Cell::new(0) };
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

#[expect(clippy::unnecessary_wraps, reason = "has the Player signature")]
fn fake_play(_: &[f32], _: u32) -> Result<Box<dyn Any>, CaptureError> {
    PLAYED.set(PLAYED.get() + 1);
    Ok(Box::new(()))
}

/// A recording's card as the page shows it (the generated struct is not exported).
struct Take {
    source: String,
    duration: String,
    peaks: usize,
    reference: String,
    recording: bool,
    play_off: String,
}

/// System memory, graphics adapters and free space of the models drive, as the page measures them.
type Hardware = (Option<u64>, Vec<models::Adapter>, Option<u64>);

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

    /// A machine of fixed memory, adapters and free disk, so no hardware is read.
    fn with_hardware(name: &str, hardware: Hardware) -> Self {
        Self::build(name, None, fake_mic, Some(hardware))
    }

    /// The model in use is on disk; the goal is the best settings (four sets) and the
    /// Recordings view is open, where most tests pick their recordings.
    fn with_mic(name: &str, config: Option<&str>, mic: oxi_settings::Mic) -> Self {
        Self::build(name, config, mic, None)
    }

    fn build(
        name: &str,
        config: Option<&str>,
        mic: oxi_settings::Mic,
        hardware: Option<Hardware>,
    ) -> Self {
        SAMPLES.set(MIC_SAMPLES);
        let layout = layout(name, config);
        fake(&layout, "turbo");
        let (ipc, sent) = Ipc::fake();
        let window = open_linked(&layout, Page::Test, ipc.clone());
        let (mut ctx, _) = Ctx::load(layout.clone()).unwrap();
        ctx.ipc = ipc.clone();
        bind_test_page(&window, &ctx, mic, fake_play, hardware);
        // The rebind above replaced what the first bind drew.
        let s = window.global::<TestState>();
        s.invoke_shown();
        s.invoke_goal_picked(1);
        s.set_view(1);
        let f = Self {
            window,
            ipc,
            sent,
            layout,
            ctx,
        };
        // The history lists asked for on open.
        f.take_sent();
        f
    }

    fn state(&self) -> TestState<'_> {
        self.window.global::<TestState>()
    }

    fn record_and_stop(&self) {
        self.state().invoke_record();
        self.state().invoke_record();
    }

    fn takes(&self) -> Vec<Take> {
        self.state()
            .get_takes()
            .iter()
            .map(|t| Take {
                source: t.source.to_string(),
                duration: t.duration.to_string(),
                peaks: t.peaks.row_count(),
                reference: t.reference.to_string(),
                recording: t.recording,
                play_off: t.play_off.to_string(),
            })
            .collect()
    }

    fn take_sent(&self) -> Vec<Request> {
        self.sent
            .try_iter()
            .filter_map(|cmd| match cmd {
                // The page's own requests; the shell's (status, devices) are not its business.
                Cmd::Send(
                    request @ (Request::TestAudio { .. }
                    | Request::TestTranscribe { .. }
                    | Request::HistoryList { .. }
                    | Request::HistoryRetranscribe { .. }),
                ) => Some(request),
                _ => None,
            })
            .collect()
    }

    /// The one request that is out: nothing else was sent since the last look but the audio.
    fn one_job(&self) -> (u32, SettingsSet, Option<HistoryId>) {
        let sent: Vec<Request> = self
            .take_sent()
            .into_iter()
            .filter(|r| !matches!(r, Request::TestAudio { .. }))
            .collect();
        assert_eq!(sent.len(), 1, "exactly one job at a time: {sent:?}");
        match sent.into_iter().next().unwrap() {
            Request::TestTranscribe { id, set, .. } => (id, set, None),
            Request::HistoryRetranscribe { run, id, set, .. } => (run, set, Some(id)),
            other => panic!("expected a job, got {other:?}"),
        }
    }

    fn answer(&self, id: u32, outcome: TestOutcome) {
        self.ipc
            .deliver(&Event::Push(Push::TestResult { id, outcome }));
    }

    /// Whisper's progress of the job `id`.
    fn progress(&self, id: u32, percent: u8, est_ms: Option<u32>) {
        self.ipc.deliver(&Event::Push(Push::TestProgress {
            id,
            percent,
            est_ms,
        }));
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

    fn list_history(&self, items: Vec<HistoryItem>) {
        self.ipc
            .deliver(&Event::Push(Push::History { items, more: false }));
    }

    /// Picks the first history recording offered (line 0 is the prompt).
    fn add_first(&self) {
        self.state().set_history_pick(1);
        self.state().invoke_add_history();
    }
}

/// Registry model `id` on disk: a sparse file of its size.
fn fake(layout: &Layout, id: &str) {
    let entry = models::find(id).unwrap();
    fs::create_dir_all(layout.models_dir()).unwrap();
    fake_model(&layout.models_dir().join(entry.file_name), entry.size);
}

/// The sets of the fixture's goal, the best settings, on the model in use.
fn sets() -> Vec<SettingsSet> {
    tuning::wizard_sets(Goal::Settings, &SettingsSet::defaults(), &["turbo"])
}

fn item(id: &str, text: &str) -> HistoryItem {
    HistoryItem {
        id: HistoryId::parse(id).unwrap(),
        status: Status::Done,
        duration_ms: 2_500,
        // Dictated with the model in use and never corrected: the typical history.
        model: "turbo".to_owned(),
        language: "ru".to_owned(),
        text: Spoken(text.to_owned()),
        held: Vec::new(),
        edited: false,
    }
}

/// [`item`] with its text corrected by hand on the History page.
fn edited(id: &str, text: &str) -> HistoryItem {
    HistoryItem {
        edited: true,
        ..item(id, text)
    }
}

/// Registry models `ids` on disk too, and the page has looked again.
fn also_on_disk(f: &Fixture, ids: &[&str]) {
    for id in ids {
        fake(&f.layout, id);
    }
    f.state().invoke_shown();
    let _ = f.take_sent();
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
fn starts_empty_with_run_off() {
    let f = Fixture::new("empty", None);
    let s = f.state();
    assert!(f.takes().is_empty());
    assert!(has(&f.window, "No recordings picked"));
    // The wizard: the goal as cards, the models, Run; Run's "i" says nothing leaves the PC.
    s.set_view(0);
    for text in ["1. Goal", "Best model", "Best settings", "Quiet microphone"] {
        assert!(has(&f.window, text), "{text}");
    }
    assert!(
        has(&f.window, "2. Model"),
        "best settings tune the model in use"
    );
    s.invoke_goal_picked(0);
    assert!(has(&f.window, "2. Models"));
    assert!(has(&f.window, "3. Run"));
    assert!(s.get_plan_tip().contains("nothing leaves this PC"));
    assert_eq!(
        s.get_run_off(),
        "No recordings yet: dictate a few times with History on, or add one on Recordings"
    );
    assert_eq!(s.get_plan(), "", "nothing to run, nothing to count");
    assert!(s.get_history_on());
    assert_eq!(s.get_add_off(), "No recordings in history");
    s.invoke_run();
    // The page lists the history when it is first drawn; nothing else goes.
    let sent = f.take_sent();
    assert!(
        sent.iter()
            .all(|r| matches!(r, Request::HistoryList { .. })),
        "no recording, nothing to send: {sent:?}"
    );
    s.set_view(2);
    assert!(has(&f.window, "Nothing run yet"));
}

#[test]
fn opening_the_page_lists_the_history() {
    let f = Fixture::new("lists", None);
    f.state().invoke_shown();
    assert_eq!(f.take_sent(), [Request::HistoryList { older_than: None }]);
}

#[test]
fn a_new_recording_gets_a_ready_phrase_and_shows_its_length() {
    let f = Fixture::new("record", None);
    let s = f.state();
    s.invoke_record();
    assert!(s.get_recording());
    assert_eq!(f.takes().len(), 1, "the card comes with Record");
    assert!(f.takes()[0].recording);
    assert_eq!(s.get_run_off(), "Stop the recording first");
    tick_for(Duration::from_secs(2));
    assert_eq!(f.takes()[0].duration, "Recording 0:02");
    s.invoke_record();
    assert!(!s.get_recording());
    let take = &f.takes()[0];
    assert_eq!(take.duration, "0:01");
    assert_eq!(take.reference, first_phrase());
    assert_eq!(take.source, "New recording 1");
    assert_eq!(take.peaks, 48);
    assert_eq!(take.play_off, "");
    assert_eq!(s.get_run_off(), "");

    f.record_and_stop();
    assert_eq!(f.takes()[1].reference, tuning::phrases(Language::RU)[1]);
}

#[test]
fn a_language_with_no_ready_phrases_asks_for_the_text() {
    let f = Fixture::new("auto", Some("version = 2\n[model]\nlanguage = \"auto\"\n"));
    let s = f.state();
    assert!(s.get_record_tip().starts_with("No ready phrases"));
    f.record_and_stop();
    assert_eq!(f.takes()[0].reference, "");
    assert_eq!(s.get_run_off(), "Write what was said in every recording");
    s.invoke_reference_edited(0, "  mine  ".into());
    assert_eq!(s.get_run_off(), "");
    assert_eq!(f.takes()[0].reference, "  mine  ");
}

#[test]
fn a_text_without_a_word_cannot_run() {
    let f = Fixture::new("no-words", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_reference_edited(0, "...".into());
    assert_eq!(s.get_run_off(), "Write what was said in every recording");
    s.invoke_run();
    assert!(f.take_sent().is_empty(), "a text of no words is not sent");
    s.invoke_reference_edited(0, "one word...".into());
    assert_eq!(s.get_run_off(), "");
}

#[test]
fn no_microphone_says_so_and_adds_nothing() {
    let f = Fixture::with_mic("no-mic", None, fake_no_mic);
    f.state().invoke_record();
    assert!(!f.state().get_recording());
    assert!(f.takes().is_empty());
    assert_eq!(f.toast(), "No microphone");
}

#[test]
fn an_empty_recording_goes() {
    let f = Fixture::new("silent", None);
    SAMPLES.set(0);
    f.record_and_stop();
    assert!(f.takes().is_empty());
}

#[test]
fn leaving_the_page_ends_the_recording_and_keeps_it() {
    let f = Fixture::new("leave", None);
    f.state().invoke_record();
    f.window.set_page(Page::About);
    mock_elapsed_time(Duration::from_millis(300));
    assert!(!f.state().get_recording());
    assert_eq!(f.takes()[0].duration, "0:01");
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
fn a_lost_microphone_stops_the_recording_and_keeps_it() {
    let f = Fixture::new("unplugged", None);
    let s = f.state();
    s.invoke_record();
    tick_for(Duration::from_secs(2));
    PLUG.take().unwrap().store(true, Ordering::Relaxed);
    tick_for(Duration::from_millis(200));
    assert!(!s.get_recording());
    assert_eq!(f.takes()[0].duration, "0:01");
    assert_eq!(s.get_run_off(), "");
    assert_eq!(
        f.toast(),
        "Microphone disconnected. What was recorded is kept."
    );
}

#[test]
fn play_starts_stops_and_ends_with_the_clip() {
    let f = Fixture::new("play", None);
    let s = f.state();
    s.invoke_record();
    assert_eq!(f.takes()[0].play_off, "Stop the recording first");
    s.invoke_record();
    PLAYED.set(0);
    s.invoke_play(0);
    assert_eq!(s.get_playing(), 0);
    assert_eq!(PLAYED.get(), 1);
    s.invoke_play(0);
    assert_eq!(s.get_playing(), -1, "Stop");

    s.invoke_play(0);
    mock_elapsed_time(Duration::from_millis(1400));
    assert_eq!(s.get_playing(), -1, "the clip and its tail are over");
    assert_eq!(PLAYED.get(), 2);
}

#[test]
fn removing_a_recording_hands_its_place_to_the_next() {
    let f = Fixture::new("remove", None);
    let s = f.state();
    f.record_and_stop();
    f.record_and_stop();
    s.invoke_reference_edited(1, "second text".into());
    s.invoke_play(1);
    s.invoke_remove(0);
    let takes = f.takes();
    assert_eq!(takes.len(), 1);
    assert_eq!(takes[0].reference, "second text");
    assert_eq!(takes[0].source, "New recording 2");
    assert_eq!(s.get_playing(), 0, "the one playing moved up");
    s.invoke_remove(0);
    assert!(f.takes().is_empty());
    assert_eq!(s.get_playing(), -1);
}

#[test]
fn history_recordings_are_offered_picked_and_given_back() {
    let f = Fixture::new("history", None);
    let s = f.state();
    // A real file for the newer one: it plays and draws; the older one has none.
    let wav = f.layout.history_dir().join(format!("{HISTORY_ID}.wav"));
    fs::create_dir_all(wav.parent().unwrap()).unwrap();
    oxi_platform::wav::write_wav(&wav, &vec![0.2; 8_000]).unwrap();
    f.list_history(vec![
        item(OLDER_ID, "older"),
        item(HISTORY_ID, " newer text "),
    ]);
    let offered: Vec<String> = s.get_history().iter().map(|l| l.to_string()).collect();
    assert_eq!(
        offered,
        [
            "Pick a recording to add",
            "2026-11-08 13:00:05 · 0:02 · newer text",
            "2026-11-07 09:00:00 · 0:02 · older"
        ],
        "newest first"
    );
    assert_eq!(s.get_add_off(), "");

    f.add_first();
    let take = &f.takes()[0];
    assert_eq!(take.source, "2026-11-08 13:00:05");
    assert_eq!(take.reference, "newer text", "its transcript, to correct");
    assert_eq!(take.duration, "0:02");
    assert_eq!(take.play_off, "");
    assert_eq!(take.peaks, 48);
    assert_eq!(
        s.get_history().row_count(),
        2,
        "a picked one leaves the list"
    );
    assert_eq!(s.get_history_pick(), 0, "back on the prompt");

    f.add_first();
    assert_eq!(f.takes()[1].play_off, "The recording file is gone");
    assert_eq!(s.get_add_off(), "Every history recording is picked");

    s.invoke_remove(0);
    assert_eq!(s.get_history().row_count(), 2, "a removed one comes back");

    // Another page of the list joins it; a delete from the History page leaves it.
    f.ipc.deliver(&Event::Push(Push::HistoryResult {
        id: HistoryId::parse(HISTORY_ID).unwrap(),
        op: HistoryOp::Delete,
        ok: true,
    }));
    assert_eq!(s.get_history().row_count(), 0);
}

/// Picking a recording adds it (DS-27.2) and the picker goes back to its prompt; there is no Add
/// button.
#[test]
fn picking_a_history_recording_adds_it_and_there_is_no_add_button() {
    let f = Fixture::new("history-keys", None);
    let s = f.state();
    f.window.window().set_size(LogicalSize::new(1280.0, 860.0));
    f.list_history(vec![
        item("2026-11-06/080000-000", "oldest"),
        item(OLDER_ID, "older"),
        item(HISTORY_ID, "newest"),
    ]);
    assert_eq!(
        ElementHandle::find_by_accessible_label(&f.window, "Add").count(),
        0,
        "no confirm button"
    );

    // Focus the picker as a click does, then shut the list it opens.
    let combo = with_role(&f.window, "From history", AccessibleRole::Combobox);
    let (at, size) = (combo.absolute_position(), combo.size());
    let position = slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
    let button = slint::platform::PointerEventButton::Left;
    f.window
        .window()
        .dispatch_event(WindowEvent::PointerPressed { position, button });
    f.window
        .window()
        .dispatch_event(WindowEvent::PointerReleased { position, button });
    press(&f.window, Key::Escape);
    press(&f.window, Key::DownArrow);
    press(&f.window, Key::DownArrow);
    assert!(f.takes().is_empty(), "arrows on the closed field only walk");
    assert_eq!(s.get_history_pick(), 2);

    // A pick from the open list adds it.
    let click =
        |event: fn(slint::LogicalPosition, slint::platform::PointerEventButton) -> WindowEvent| {
            f.window.window().dispatch_event(event(position, button));
        };
    click(|position, button| WindowEvent::PointerPressed { position, button });
    click(|position, button| WindowEvent::PointerReleased { position, button });
    press(&f.window, Key::DownArrow);
    assert!(
        f.takes().is_empty(),
        "an arrow in the open list adds nothing"
    );
    press(&f.window, Key::Return);
    let takes = f.takes();
    assert_eq!(takes.len(), 1, "exactly the picked one");
    assert_eq!(takes[0].source, "2026-11-06 08:00:00");
    assert_eq!(s.get_history_pick(), 0, "back on the prompt");
}

/// Opens the history picker as a click does.
fn open_picker(f: &Fixture) {
    let combo = with_role(&f.window, "From history", AccessibleRole::Combobox);
    let (at, size) = (combo.absolute_position(), combo.size());
    let position = slint::LogicalPosition::new(at.x + size.width / 2.0, at.y + size.height / 2.0);
    click_at(f, position);
}

fn click_at(f: &Fixture, position: slint::LogicalPosition) {
    let button = slint::platform::PointerEventButton::Left;
    let w = f.window.window();
    w.dispatch_event(WindowEvent::PointerPressed { position, button });
    w.dispatch_event(WindowEvent::PointerReleased { position, button });
}

fn picker_fixture(name: &str) -> Fixture {
    let f = Fixture::new(name, None);
    f.window.window().set_size(LogicalSize::new(1280.0, 860.0));
    f.list_history(vec![
        item("2026-11-06/080000-000", "oldest"),
        item(OLDER_ID, "older"),
        item(HISTORY_ID, "newest"),
    ]);
    f
}

/// Esc closes the list and adds nothing, after a walk in it or after a walk on the closed field.
#[test]
fn escape_in_the_history_list_adds_nothing() {
    let f = picker_fixture("history-esc");
    open_picker(&f);
    press(&f.window, Key::DownArrow);
    press(&f.window, Key::DownArrow);
    press(&f.window, Key::Escape);
    mock_elapsed_time(Duration::from_millis(1));
    assert!(f.takes().is_empty(), "Esc after a walk in the open list");
    assert_eq!(
        f.state().get_history_pick(),
        0,
        "Esc puts the prompt back, not the walked name"
    );

    press(&f.window, Key::DownArrow);
    open_picker(&f);
    press(&f.window, Key::Escape);
    mock_elapsed_time(Duration::from_millis(1));
    assert!(f.takes().is_empty(), "Esc after a walk on the closed field");
    assert_eq!(f.state().get_history_pick(), 0, "closed field: prompt back");
}

/// A click outside the list closes it and adds nothing.
#[test]
fn a_click_outside_the_history_list_adds_nothing() {
    let f = picker_fixture("history-outside");
    open_picker(&f);
    press(&f.window, Key::DownArrow);
    click_at(&f, slint::LogicalPosition::new(2.0, 2.0));
    mock_elapsed_time(Duration::from_millis(1));
    assert!(f.takes().is_empty());
    assert_eq!(f.state().get_history_pick(), 0, "prompt back");
}

/// A click on a line adds that line.
#[test]
fn a_click_on_a_history_line_adds_it() {
    let f = picker_fixture("history-line");
    open_picker(&f);
    let line = f.state().get_history().row_data(2).unwrap().to_string();
    with_role(&f.window, &line, AccessibleRole::ListItem).invoke_accessible_default_action();
    mock_elapsed_time(Duration::from_millis(1));
    let takes = f.takes();
    assert_eq!(takes.len(), 1, "exactly the clicked one");
    assert_eq!(takes[0].source, "2026-11-07 09:00:00");
    assert_eq!(f.state().get_history_pick(), 0, "back on the prompt");
}

/// Under ~1180 px the control column is at its 240 px floor: the history picker must still sit
/// inside the card, whose inner right edge is where the Record button ends.
#[test]
fn history_picker_fits_at_720x520() {
    let f = Fixture::new("history-narrow", None);
    f.window.window().set_size(LogicalSize::new(720.0, 520.0));
    f.list_history(vec![
        item("2026-11-06/080000-000", "oldest"),
        item(OLDER_ID, "older"),
        item(HISTORY_ID, "newest"),
    ]);
    let right = |e: &ElementHandle| e.absolute_position().x + e.size().width;
    let card_right = right(&with_role(&f.window, "Record", AccessibleRole::Button));
    let picker = with_role(&f.window, "From history", AccessibleRole::Combobox);
    assert!(
        right(&picker) <= card_right + 0.5,
        "the picker stays inside too"
    );
}

#[test]
fn history_off_offers_to_turn_it_on() {
    let f = Fixture::new(
        "history-off",
        Some("version = 2\n[history]\nenabled = false\n"),
    );
    let s = f.state();
    assert!(!s.get_history_on());
    s.invoke_shown();
    assert!(f.take_sent().is_empty(), "no list while history is off");
    assert!(has(&f.window, "Turn on"));
    s.invoke_turn_on_history();
    assert!(config(&f.layout).config.history.enabled);
    assert!(s.get_history_on());
    assert_eq!(f.take_sent(), [Request::HistoryList { older_than: None }]);
}

#[expect(clippy::too_many_lines, reason = "one script of the whole flow")]
#[test]
fn the_flow_runs_every_set_over_every_recording_and_apply_writes_the_best() {
    let f = Fixture::new(
        "flow",
        Some(
            "version = 2
[audio]
per_device = true
",
        ),
    );
    let s = f.state();
    f.record_and_stop();
    // Corrected by hand, so it is its own reference and the judge has nothing to do here.
    f.list_history(vec![edited(HISTORY_ID, "раз два три")]);
    f.add_first();
    assert_eq!(s.get_plan(), "2 picked recordings · 8 runs");
    s.invoke_run();
    assert_eq!(s.get_view(), 2, "the table opens");
    assert!(s.get_running());

    let mut requests = f.take_sent();
    let Request::TestAudio { reset, pcm } = requests.remove(0) else {
        panic!("the new recording goes first");
    };
    assert!(reset);
    assert_eq!(pcm.len(), MIC_SAMPLES * 8);
    assert_eq!(requests.len(), 1, "one job, the others wait for its answer");
    let Request::TestTranscribe { id, set, language } = requests.remove(0) else {
        panic!("then the first set on it");
    };
    assert_eq!(language, Language::RU);
    assert_eq!(set, sets()[0]);
    assert_eq!(f.errors()[0], "Running…");
    assert_eq!(f.errors()[1], "Waiting");
    assert_eq!(s.get_run_off(), "The test is running");
    assert_eq!(s.get_record_off(), "The test is running");
    s.invoke_record();
    assert!(!s.get_recording(), "no recording during a run");

    // Live percent: 8 jobs, the first half done. The first push starts the bar with its
    // estimate; the shown value is the driver's (it chases what is known), so it is a few percent.
    assert!(s.get_progress() <= 0.0, "nothing known: the sweep");
    f.progress(id, 0, Some(1000));
    f.progress(id, 50, None);
    assert!(
        s.get_progress() < 0.0625,
        "1/16 raw: the driver moves only on its frame"
    );
    thread::sleep(Duration::from_millis(250));
    mock_elapsed_time(Duration::from_millis(100));
    // The first frame also runs the page's init, which lists the history again: not a job.
    let _ = f.take_sent();
    let shown = s.get_progress();
    assert!((0.02..=0.2).contains(&shown), "{shown}");
    assert!(
        s.get_progress_text()
            .starts_with(&format!("{:.0} %", (shown * 100.0).floor())),
        "the percent first, then the time left once it is known"
    );
    assert_eq!(s.get_pace(), "", "no recording heard yet");

    // Set 1: a word wrong on the new recording, the history one right.
    let wrong = format!("{first} нет", first = first_phrase());
    f.answer(id, heard(&wrong));
    assert_eq!(f.errors()[0], "Running…", "the history recording is next");
    let (id, set, from) = f.one_job();
    assert_eq!(from.as_deref(), Some(HISTORY_ID));
    assert_eq!(set, sets()[0]);
    assert_eq!(s.get_pace(), "1.5 s per recording");
    f.answer(id, heard("раз два три"));
    let want = word_errors(first_phrase(), &wrong).plus(word_errors("раз два три", "раз два три"));
    assert_eq!(f.errors()[0], format!("{:.1} %", want.percent()));
    let rows: Vec<_> = s.get_rows().iter().collect();
    assert_eq!(rows[0].time, "3.0 s");
    assert_eq!(rows[0].model, "Whisper large-v3 turbo");
    assert_eq!(rows[0].normalize, "On");
    let heard_rows: Vec<_> = s.get_heard().iter().collect();
    assert_eq!(heard_rows[0].text, wrong.as_str());
    assert_eq!(heard_rows[1].errors, "0.0 %");

    // Set 2: every word right; set 3 and 4 fail. The daemon still holds the new recording.
    let mut requests = f.take_sent();
    assert_eq!(requests.len(), 1, "no audio again: {requests:?}");
    let Request::TestTranscribe { id, set, .. } = requests.remove(0) else {
        panic!("set 2");
    };
    assert_eq!(set, sets()[1]);
    f.answer(id, heard(first_phrase()));
    let (id, _, _) = f.one_job();
    f.answer(id, heard("раз, два, три!"));
    let (id, _, _) = f.one_job();
    f.answer(id, failed(TestFailure::ModelBad));
    let (id, _, _) = f.one_job();
    f.answer(id, heard("раз два три"));
    let (id, _, _) = f.one_job();
    f.answer(id, failed(TestFailure::ModelMissing));
    let (id, _, _) = f.one_job();
    f.answer(id, failed(TestFailure::ModelMissing));
    assert_eq!(f.errors()[2], "Model failed");
    assert_eq!(f.errors()[3], "Not downloaded");
    assert!(f.take_sent().is_empty(), "the run is over");
    assert!(!s.get_running());

    let rows: Vec<_> = s.get_rows().iter().collect();
    assert!(rows[1].best && !rows[0].best && !rows[2].best);
    assert_eq!(s.get_selected(), 1, "the best set's recordings show");
    assert_eq!(
        s.get_best_text(),
        "Whisper large-v3 turbo · 0.0 % errors · 1.5 s per recording"
    );
    assert_eq!(s.get_drop_text(), "", "one model: nothing lost");
    assert_eq!(s.get_apply_off(), "");
    assert_eq!(s.get_run_off(), "");
    s.invoke_select(2);
    let heard_rows: Vec<_> = s.get_heard().iter().collect();
    assert_eq!(heard_rows[0].errors, "Model failed");
    assert_eq!(heard_rows[1].text, "раз два три");

    s.invoke_apply();
    let want = &sets()[1];
    let saved = config(&f.layout).config;
    assert!(saved.audio.denoise, "the best set is the denoise one");
    // "System default": the set is keyed by the default device's name ("" when none is known).
    assert!(
        !saved.audio.device_sets.is_empty() && saved.audio.device_sets.values().all(|s| s.denoise),
        "kept for this microphone too, or picking another mic and back loses it"
    );
    assert_eq!(saved.audio.normalize, want.normalize);
    assert_eq!(saved.audio.trim, want.trim);
    assert_eq!(saved.model.id, want.model);
    assert_eq!(f.toast(), "Best set applied.");
}

#[test]
fn each_new_recording_goes_before_its_job() {
    let f = Fixture::new("two-new", None);
    f.record_and_stop();
    SAMPLES.set(MIC_SAMPLES / 2);
    f.record_and_stop();
    f.state().invoke_run();
    let mut lengths = Vec::new();
    for _ in 0..3 {
        let sent = f.take_sent();
        lengths.push(sent.iter().find_map(|r| match r {
            Request::TestAudio { pcm, .. } => Some(pcm.len() / 8),
            _ => None,
        }));
        let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
            panic!("a job");
        };
        f.answer(*id, heard(first_phrase()));
    }
    assert_eq!(
        lengths,
        [Some(MIC_SAMPLES), Some(MIC_SAMPLES / 2), Some(MIC_SAMPLES)],
        "set 2 sends the first one again"
    );
}

#[test]
fn a_gone_history_file_fails_its_line_and_the_run_goes_on() {
    let f = Fixture::new("gone", None);
    f.list_history(vec![edited(HISTORY_ID, "text")]);
    f.add_first();
    f.state().invoke_run();
    let (id, _, from) = f.one_job();
    assert!(from.is_some());
    f.answer(id, failed(TestFailure::NoAudio));
    assert_eq!(f.errors()[0], "The recording file is gone");
    let (id, set, _) = f.one_job();
    assert_eq!(set, sets()[1]);
    f.answer(id, heard("text"));
    assert_eq!(f.errors()[1], "0.0 %");
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
    let Request::TestTranscribe { id, .. } = sent[3] else {
        unreachable!()
    };
    f.answer(id, heard(first_phrase()));
    assert!(
        matches!(f.take_sent()[..], [Request::TestTranscribe { .. }]),
        "the daemon still holds the only new recording"
    );
}

#[test]
fn the_goal_picks_the_sets() {
    let f = Fixture::new("goal", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_goal_picked(0);
    assert_eq!(s.get_plan(), "1 picked recording · 1 run");
    s.invoke_goal_picked(1);
    assert_eq!(s.get_plan(), "1 picked recording · 4 runs");
    s.invoke_run();
    assert_eq!(s.get_rows().row_count(), sets().len());
    s.invoke_goal_picked(0);
    assert_eq!(s.get_goal(), 1, "no new goal during a run");
}

/// Three clicks from a first visit: a goal card, a quick pick, Run. The recordings come from
/// History by themselves (recent ones of different lengths), all dictated with the model in use
/// and never corrected, the typical case. The most accurate model on disk writes their reference
/// and is not compared, so the model in use gets no head start from its own saved text.
#[test]
fn the_wizard_runs_on_recent_history_in_three_clicks() {
    let f = Fixture::new("wizard", None);
    let s = f.state();
    also_on_disk(&f, &["large-v3", "large-v3-compact", "small"]);
    s.set_view(0);
    let at = |id: &str, ms: u64, text: &str| HistoryItem {
        duration_ms: ms,
        ..item(id, text)
    };
    f.list_history(vec![
        at("2026-11-08/130005-123", 4_000, "четыре секунды"),
        at("2026-11-08/120000-000", 30_000, "тридцать секунд"),
        at("2026-11-08/110000-000", 12_000, "двенадцать секунд"),
        at("2026-11-08/100000-000", 2_000, "две"),
        at("2026-11-08/090000-000", 500, "too short"),
    ]);
    s.invoke_goal_picked(0);
    s.invoke_preset_picked(0);
    assert_eq!(
        s.get_plan(),
        "3 recordings from History · 9 runs\nReference: your edits where you made them, otherwise Whisper large-v3"
    );
    assert_eq!(s.get_run_off(), "");
    s.invoke_run();
    assert_eq!(s.get_view(), 2);
    let models: Vec<String> = s.get_rows().iter().map(|r| r.model.to_string()).collect();
    assert_eq!(
        models,
        ["Whisper large-v3 turbo", "Whisper small"],
        "the judge is not compared"
    );
    let judge = SettingsSet {
        model: "large-v3".into(),
        ..SettingsSet::defaults()
    };
    let saved = ["тридцать секунд", "двенадцать секунд", "две"];
    let judged = ["тридцать одна секунда", "двенадцать секунд", "две минуты"];
    let mut picked = Vec::new();
    for text in judged {
        let (id, set, from) = f.one_job();
        assert_eq!(
            set, judge,
            "the judge hears each one first, at the default settings"
        );
        picked.push(from.unwrap().to_string());
        f.answer(id, heard(text));
    }
    assert_eq!(
        picked,
        [
            "2026-11-08/120000-000",
            "2026-11-08/110000-000",
            "2026-11-08/100000-000"
        ],
        "the longest, one between, the shortest; newest first"
    );
    // Turbo hears what it once wrote, small what the judge heard.
    for (model, texts) in [("turbo", saved), ("small", judged)] {
        for text in texts {
            let (id, set, _) = f.one_job();
            assert_eq!(set.model, model);
            f.answer(id, heard(text));
        }
    }
    assert_eq!(f.errors(), ["42.9 %", "0.0 %"]);
    assert!(
        s.get_rows().row_data(1).unwrap().best,
        "graded on the judge's text, not on its own"
    );
    assert!(!s.get_running());
}

/// Each recording is scored against the person's text where they corrected it (on the History
/// page or on Recordings), else against what the judge heard. A run with every text corrected
/// needs no judge, so every model is compared.
#[test]
fn the_reference_is_the_edit_or_else_the_judge() {
    let f = Fixture::new("reference", None);
    let s = f.state();
    also_on_disk(&f, &["small", "base"]);
    s.set_view(0);
    f.list_history(vec![
        edited(HISTORY_ID, "своими словами"),
        item(OLDER_ID, "как записал turbo"),
    ]);
    s.invoke_goal_picked(0);
    s.invoke_preset_picked(0);
    assert_eq!(
        s.get_plan(),
        "2 recordings from History · 5 runs\nReference: your edits where you made them, otherwise Whisper large-v3 turbo"
    );
    s.invoke_run();
    let (id, set, from) = f.one_job();
    assert_eq!(
        set,
        SettingsSet::defaults(),
        "the judge: turbo, the best on disk"
    );
    assert_eq!(
        from.unwrap().to_string(),
        OLDER_ID,
        "the corrected one needs no judge"
    );
    f.answer(id, heard("что услышал судья"));
    for model in ["small", "base"] {
        let (id, set, from) = f.one_job();
        assert_eq!(set.model, model, "the judge is not compared");
        assert_eq!(from.unwrap().to_string(), HISTORY_ID);
        f.answer(id, heard("своими словами"));
        let (id, _, _) = f.one_job();
        f.answer(id, heard("что услышал судья"));
    }
    assert_eq!(f.errors(), ["0.0 %", "0.0 %"]);
    assert!(!s.get_running());
    assert!(
        s.get_best_text().ends_with(
            "\nReference: your edits where you made them, otherwise Whisper large-v3 turbo"
        ),
        "the result names what it was scored against: {}",
        s.get_best_text()
    );

    // Both picked by hand, the uncorrected one corrected here: no judge, every model runs.
    f.add_first();
    f.add_first();
    s.invoke_reference_edited(1, "как было сказано".into());
    assert_eq!(s.get_plan(), "2 picked recordings · 6 runs");
    s.invoke_run();
    let (_, set, _) = f.one_job();
    assert_eq!(set.model, "turbo", "the model in use is compared again");
}

/// Review 3 and 5: the best model needs two models beside the judge's family to compare, or every
/// text corrected; a judge's compact file is not compared (it matches the judge almost word for
/// word).
#[test]
fn best_model_needs_two_models_beside_the_judge_and_its_compact_file() {
    let f = Fixture::new("two-models", None);
    let s = f.state();
    also_on_disk(&f, &["small"]);
    s.set_view(0);
    f.list_history(vec![item(HISTORY_ID, "как записал turbo")]);
    s.invoke_goal_picked(0);
    s.invoke_preset_picked(0);
    let off = "Needs two models to compare besides Whisper large-v3 turbo, which writes the reference texts: download another model, or correct the texts on History";
    assert_eq!(s.get_run_off(), off, "small alone would win unopposed");
    assert_eq!(s.get_plan(), "");
    s.invoke_run();
    assert!(f.take_sent().is_empty(), "Run is off");
    also_on_disk(&f, &["large-v3", "large-v3-compact"]);
    assert_eq!(
        s.get_run_off(),
        "",
        "large-v3 judges; turbo and small compete"
    );
    s.invoke_run();
    let (id, set, _) = f.one_job();
    assert_eq!(set.model, "large-v3");
    f.answer(id, heard("как сказано"));
    let models: Vec<String> = s.get_rows().iter().map(|r| r.model.to_string()).collect();
    assert_eq!(
        models,
        ["Whisper large-v3 turbo", "Whisper small"],
        "neither the judge nor its compact file"
    );
    for (want, text) in [("turbo", "как"), ("small", "как сказано")] {
        let (id, set, _) = f.one_job();
        assert_eq!(set.model, want);
        f.answer(id, heard(text));
    }
    s.invoke_apply();
    assert_eq!(
        config(&f.layout).config.model.id,
        "small",
        "the best model's Apply switches the model"
    );
}

/// Review 2 and 4: best settings and the quiet microphone always tune the model in use, judged by
/// a model outside its family; with none, the recordings without the person's text cannot be
/// scored and Run says what is missing. Apply never switches the model.
#[test]
fn best_settings_tune_the_model_in_use_judged_by_another_model() {
    let f = Fixture::new("settings-judge", None);
    let s = f.state();
    s.set_view(0);
    f.list_history(vec![item(HISTORY_ID, "как записал turbo")]);
    for goal in [1, 2] {
        s.invoke_goal_picked(goal);
        assert_eq!(
            s.get_run_off(),
            "No second model to compare with: download another model, or correct the texts on History",
            "turbo would judge its own sets"
        );
        assert_eq!(s.get_plan(), "");
    }
    also_on_disk(&f, &["turbo-compact"]);
    assert_eq!(
        s.get_run_off(),
        "No second model to compare with: download another model, or correct the texts on History",
        "its own compact file cannot judge it"
    );
    also_on_disk(&f, &["small"]);
    s.invoke_goal_picked(1);
    assert_eq!(s.get_run_off(), "");
    assert_eq!(
        s.get_plan(),
        "1 recording from History · 5 runs\nReference: your edits where you made them, otherwise Whisper small"
    );
    s.invoke_run();
    let (id, set, _) = f.one_job();
    assert_eq!(
        set,
        SettingsSet {
            model: "small".into(),
            ..SettingsSet::defaults()
        },
        "the judge first, at the default settings"
    );
    f.answer(id, heard("раз два три"));
    let mut tuned = Vec::new();
    for text in ["раз два", "раз два три", "раз", "раз два"] {
        let (id, set, _) = f.one_job();
        tuned.push(set.model.clone());
        f.answer(id, heard(text));
    }
    assert_eq!(tuned, ["turbo"; 4], "only the model in use is tuned");
    assert!(!s.get_running());
    // Risk (c): a model picked on the Model page after the run stays; Apply writes the settings.
    f.ctx.config.borrow_mut().config.model.id = "small".into();
    s.invoke_apply();
    let saved = config(&f.layout).config;
    assert_eq!(saved.model.id, "small", "Apply never writes the model");
    assert_eq!(saved.audio.denoise, sets()[1].denoise, "the second set won");
}

/// Review 1: no downloaded model hears the language, so none can judge. The recordings without
/// the person's text are left out with the reason, never scored against a candidate's own answer;
/// with none corrected Run says so.
#[test]
fn with_no_judge_for_the_language_uncorrected_recordings_are_left_out() {
    let f = Fixture::new(
        "no-judge",
        Some("version = 2\n[model]\nlanguage = \"ru\"\n"),
    );
    let s = f.state();
    fake(&f.layout, "distil-en");
    fs::remove_file(
        f.layout
            .models_dir()
            .join(models::find("turbo").unwrap().file_name),
    )
    .unwrap();
    s.invoke_shown();
    let _ = f.take_sent();
    s.set_view(0);
    s.invoke_goal_picked(0);
    s.invoke_preset_picked(0);
    f.list_history(vec![item(HISTORY_ID, "как записал turbo")]);
    assert_eq!(
        s.get_run_off(),
        "No downloaded model hears this language to write the reference texts: correct them on History, or download one that does"
    );
    f.list_history(vec![
        edited(HISTORY_ID, "своими словами"),
        item(OLDER_ID, "как записал turbo"),
    ]);
    assert_eq!(s.get_run_off(), "");
    assert_eq!(
        s.get_plan(),
        "1 recording from History · 1 run\n1 recording left out: no second model to compare with. Correct its text on History, or download another model"
    );
    s.invoke_run();
    let (id, set, from) = f.one_job();
    assert_eq!(set.model, "distil-en");
    assert_eq!(
        from.unwrap().to_string(),
        HISTORY_ID,
        "the corrected one only"
    );
    f.answer(id, heard("своими словами"));
    assert!(f.take_sent().is_empty(), "nothing else to run");
    assert!(!s.get_running());
    assert_eq!(f.errors(), ["0.0 %"]);
}

/// Re-review: no fair judge for the uncorrected recordings, but some are corrected. The corrected
/// ones run alone, with the same rule and words as when no model hears the language; Run is off
/// only when none is corrected.
#[test]
fn without_a_fair_judge_the_corrected_recordings_run_alone() {
    let f = Fixture::new("mixed", None);
    let s = f.state();
    also_on_disk(&f, &["small"]);
    s.set_view(0);
    s.invoke_goal_picked(0);
    s.invoke_preset_picked(0);
    f.list_history(vec![
        edited(HISTORY_ID, "своими словами"),
        item(OLDER_ID, "как записал turbo"),
    ]);
    assert_eq!(s.get_run_off(), "", "turbo judges, small alone beside it");
    assert_eq!(
        s.get_plan(),
        "1 recording from History · 2 runs\n1 recording left out: no second model to compare with. Correct its text on History, or download another model"
    );
    s.invoke_run();
    let mut models = Vec::new();
    for _ in 0..2 {
        let (id, set, from) = f.one_job();
        assert_eq!(
            from.unwrap().to_string(),
            HISTORY_ID,
            "the corrected one only"
        );
        models.push(set.model.clone());
        f.answer(id, heard("своими словами"));
    }
    assert_eq!(
        models,
        ["turbo", "small"],
        "no judge: every ticked model competes"
    );
    assert!(f.take_sent().is_empty() && !s.get_running());
}

/// [`without_a_fair_judge_the_corrected_recordings_run_alone`] for best settings with only the
/// compact file of the model in use beside it.
#[test]
fn without_a_judge_outside_the_family_the_corrected_recordings_run_alone() {
    let f = Fixture::new("mixed-settings", None);
    let s = f.state();
    also_on_disk(&f, &["turbo-compact"]);
    s.set_view(0);
    f.list_history(vec![
        edited(HISTORY_ID, "своими словами"),
        item(OLDER_ID, "как записал turbo"),
    ]);
    assert_eq!(s.get_run_off(), "");
    assert_eq!(
        s.get_plan(),
        "1 recording from History · 4 runs\n1 recording left out: no second model to compare with. Correct its text on History, or download another model"
    );
    s.invoke_run();
    let (_, set, from) = f.one_job();
    assert_eq!(
        (set.model.as_str(), from.unwrap().to_string()),
        ("turbo", HISTORY_ID.to_owned())
    );
}

/// Re-review: the own model file in use is tuned by the settings goals, judged by a registry
/// model; Run is off only with no file set, and then says what to do.
#[test]
fn the_settings_goals_tune_the_own_model_file_in_use() {
    let f = Fixture::new(
        "custom",
        Some("version = 2\n[model]\nid = \"custom\"\ncustom_path = \"C:/nowhere/mine.bin\"\n"),
    );
    let s = f.state();
    s.set_view(0);
    f.list_history(vec![item(HISTORY_ID, "как записал свой")]);
    assert_eq!(s.get_tunes(), "Tunes the model in use: Own model file");
    assert_eq!(s.get_run_off(), "");
    assert_eq!(
        s.get_plan(),
        "1 recording from History · 5 runs\nReference: your edits where you made them, otherwise Whisper large-v3 turbo"
    );
    s.invoke_run();
    let (id, set, _) = f.one_job();
    assert_eq!(set.model, "turbo", "a registry model judges");
    f.answer(id, heard("раз два три"));
    for _ in 0..4 {
        let (id, set, _) = f.one_job();
        assert_eq!(set.model, "custom");
        f.answer(id, heard("раз два три"));
    }
    s.invoke_apply();
    assert_eq!(config(&f.layout).config.model.id, "custom");

    f.ctx.config.borrow_mut().config.model.custom_path = String::new();
    s.invoke_goal_picked(1);
    assert_eq!(
        s.get_run_off(),
        "These goals tune the model in use, and it is not on disk: download it, or pick its file, on the Model page"
    );
}

/// Re-review: the settings goals compare no models, so step 2 shows which one they tune instead
/// of ticks that would do nothing.
#[test]
fn the_settings_goals_show_the_model_they_tune_instead_of_the_list() {
    let f = Fixture::new("step-two", None);
    let s = f.state();
    s.set_view(0);
    s.invoke_goal_picked(0);
    let models = || ElementHandle::find_by_accessible_label(&f.window, "Models").count();
    let with_list = models();
    for goal in [1, 2] {
        s.invoke_goal_picked(goal);
        assert_eq!(models(), with_list - 1, "no model list");
        assert_eq!(
            s.get_tunes(),
            "Tunes the model in use: Whisper large-v3 turbo"
        );
    }
}

/// The quiet-microphone check needs no new recording: each one is heard again 6, 12 and 18 dB
/// quieter, sent as audio of its own.
#[test]
fn a_quiet_microphone_hears_each_recording_quieter() {
    let f = Fixture::new("quiet", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_goal_picked(2);
    assert_eq!(
        s.get_plan(),
        "1 picked recording · 4 loudness levels · 8 runs"
    );
    s.invoke_run();
    assert_eq!(
        s.get_rows().row_count(),
        2,
        "as set, and normalise with denoise"
    );
    let mut levels = Vec::new();
    loop {
        let sent = f.take_sent();
        let Some(id) = sent.iter().find_map(|r| match r {
            Request::TestTranscribe { id, .. } => Some(*id),
            _ => None,
        }) else {
            break;
        };
        if let Some(Request::TestAudio { pcm, .. }) = sent.first() {
            levels.push(tuning::decode_pcm(pcm).unwrap()[0]);
        }
        f.answer(id, heard(first_phrase()));
    }
    let want: Vec<f32> = [0.0_f32, -6.0, -12.0, -18.0]
        .iter()
        .map(|db| 0.1 * 10f32.powf(db / 20.0))
        .collect();
    assert_eq!(
        levels.len(),
        8,
        "every recording goes again for the second set"
    );
    for (got, want) in levels.iter().zip(want.iter().cycle()) {
        assert!((got - want).abs() < 1e-5, "{got} vs {want}");
    }
    let labels: Vec<String> = s.get_heard().iter().map(|h| h.source.to_string()).collect();
    assert_eq!(labels[3], "New recording 1 · -18 dB");
}

/// The quick picks tick the list, a tick by hand leaves them; a model not on disk is skipped
/// with its reason and never sent.
#[test]
fn presets_tick_the_list_and_a_missing_model_is_skipped() {
    let f = Fixture::new("presets", None);
    let s = f.state();
    let lines = || -> Vec<(String, bool, bool)> {
        s.get_models()
            .iter()
            .map(|m| (m.name.to_string(), m.on, m.downloaded))
            .collect()
    };
    let listed = MODELS.iter().filter(|m| m.id != "tiny").count();
    assert_eq!(lines().len(), listed, "every model but the test one");
    assert_eq!(s.get_preset(), 0, "Downloaded");
    assert_eq!(
        lines().iter().filter(|l| l.1).count(),
        1,
        "only the model on disk"
    );
    assert!(lines()[0].1 && lines()[0].2);
    s.invoke_preset_picked(1);
    assert_eq!(
        lines().iter().filter(|l| l.1).count(),
        1,
        "no recommendation known yet: the model in use"
    );
    s.invoke_preset_picked(2);
    assert!(lines().iter().all(|l| l.1), "All");
    s.invoke_model_ticked(1);
    assert_eq!(s.get_preset(), -1, "by hand");
    assert!(!lines()[1].1);
    s.invoke_model_ticked(1);
    assert_eq!(s.get_preset(), 2, "the ticks of All again");

    s.invoke_goal_picked(0);
    f.record_and_stop();
    s.invoke_run();
    assert_eq!(s.get_rows().row_count(), listed);
    let errors = f.errors();
    assert_eq!(errors[0], "Running…");
    assert!(
        errors[1..].iter().all(|e| e == "Not downloaded"),
        "{errors:?}"
    );
    let (id, _, _) = f.one_job();
    f.answer(id, heard(first_phrase()));
    assert!(f.take_sent().is_empty(), "the missing ones are never sent");
    assert!(!s.get_running());
    assert!(s.get_rows().row_data(0).unwrap().best);
}

/// Cancel stops after the job out; what was heard so far still competes.
#[test]
fn cancel_keeps_what_was_heard() {
    let f = Fixture::new("cancel", None);
    let s = f.state();
    f.record_and_stop();
    s.invoke_run();
    let (id, _, _) = f.one_job();
    f.answer(id, heard(first_phrase()));
    let (left, _, _) = f.one_job();
    s.invoke_cancel();
    assert!(!s.get_running());
    assert_eq!(f.toast(), "Test cancelled.");
    assert_eq!(f.errors(), ["0.0 %", "Not run", "Not run", "Not run"]);
    assert!(s.get_rows().row_data(0).unwrap().best);
    assert_eq!(s.get_apply_off(), "");
    f.answer(left, heard("late"));
    assert_eq!(f.errors()[1], "Not run", "the late answer scores nothing");
    assert!(f.take_sent().is_empty(), "and starts nothing");
}

/// A finished run offers to delete the downloaded models that lost, with their size, after a
/// question in the row; never the model in use.
#[test]
fn a_finished_run_offers_to_delete_the_models_that_lost() {
    let f = Fixture::new("losers", None);
    let s = f.state();
    fake(&f.layout, "small");
    s.invoke_shown();
    let _ = f.take_sent();
    s.invoke_goal_picked(0);
    f.record_and_stop();
    s.invoke_run();
    let (id, set, _) = f.one_job();
    assert_eq!(set.model, "turbo");
    f.answer(id, heard(first_phrase()));
    let (id, set, _) = f.one_job();
    assert_eq!(set.model, "small");
    f.answer(id, heard("совсем не то"));
    assert!(s.get_rows().row_data(0).unwrap().best);
    let small = models::find("small").unwrap();
    let frees = oxi_core::strings::gb(small.size);
    assert_eq!(s.get_drop_text(), format!("Delete Whisper small ({frees})"));
    assert_eq!(
        s.get_drop_ask(),
        format!("Delete Whisper small? Frees {frees}.")
    );
    assert_eq!(s.get_drop_off(), "");
    s.invoke_drop();
    let deletes: Vec<Request> = f
        .sent
        .try_iter()
        .filter_map(|cmd| match cmd {
            Cmd::Send(r @ Request::DeleteModel { .. }) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(deletes, [Request::DeleteModel { id: "small".into() }]);
    assert_eq!(s.get_drop_text(), "", "offered once");
}

/// Review c07: Delete was clicked (the row asks), then the next run leaves no model that lost:
/// the question row goes with the offer, and a Delete with nothing left sends nothing.
#[test]
fn the_delete_question_goes_with_the_next_run() {
    let f = Fixture::new("losers-again", None);
    let s = f.state();
    fake(&f.layout, "small");
    s.invoke_shown();
    let _ = f.take_sent();
    s.invoke_goal_picked(0);
    f.record_and_stop();
    s.invoke_run();
    let (id, _, _) = f.one_job();
    f.answer(id, heard(first_phrase()));
    let (id, _, _) = f.one_job();
    f.answer(id, heard("совсем не то"));
    assert_ne!(s.get_drop_text(), "");
    s.set_dropping(true);

    let small = models::find("small").unwrap().display_name;
    let line = s.get_models().iter().position(|m| m.name == small).unwrap();
    s.invoke_model_ticked(i32::try_from(line).unwrap());
    s.invoke_run();
    let (id, set, _) = f.one_job();
    assert_eq!(set.model, "turbo", "one model, so none can lose");
    f.answer(id, heard(first_phrase()));
    assert!(!s.get_running());
    assert_eq!(s.get_drop_text(), "");
    assert!(!s.get_dropping(), "no question about nothing");
    s.invoke_drop();
    assert!(
        !f.sent
            .try_iter()
            .any(|c| matches!(c, Cmd::Send(Request::DeleteModel { .. }))),
        "nothing to delete"
    );
    assert_ne!(f.toast(), "Deleting the other models.");
}

/// Review c07: the recommended model, ticked but not on disk, is skipped by the run and offered as
/// Download, which only opens the Model page (never a download on its own); once it is downloaded
/// the offer goes. The machine: an integrated GPU from the daemon, so the system memory picks
/// turbo compact (16 GB of memory, 100 GB free, given to the page, so no hardware is read).
#[test]
fn a_run_offers_to_download_the_recommended_model_it_skipped() {
    const GB: u64 = 1 << 30;
    let f = Fixture::with_hardware("get-offer", (Some(16 * GB), vec![], Some(100 * GB)));
    let s = f.state();
    f.ipc.deliver(&Event::Push(Push::Devices {
        mics: vec![],
        gpus: vec![Device {
            name: "Vulkan0".into(),
            description: "Integrated".into(),
            uma: true,
        }],
    }));
    let compact = models::find("turbo-compact").unwrap();
    let marked: Vec<String> = s
        .get_models()
        .iter()
        .filter(|m| m.recommended)
        .map(|m| m.name.to_string())
        .collect();
    assert_eq!(marked, [compact.display_name]);
    s.invoke_goal_picked(0);
    s.invoke_preset_picked(1);
    f.record_and_stop();
    s.invoke_run();
    let (id, set, _) = f.one_job();
    assert_eq!(set.model, "turbo", "the model on disk runs");
    f.answer(id, heard(first_phrase()));
    assert!(!s.get_running());
    assert_eq!(
        s.get_get_text(),
        format!("Download {}", compact.display_name)
    );

    s.invoke_get();
    assert_eq!(f.window.get_page(), Page::Model);
    assert!(
        !f.sent
            .try_iter()
            .any(|c| matches!(c, Cmd::Send(Request::DownloadModel { .. }))),
        "Download only opens the Model page"
    );

    fake(&f.layout, "turbo-compact");
    f.ipc.deliver(&Event::Push(Push::DownloadDone {
        id: "turbo-compact".into(),
        result: DownloadResult::Ok,
    }));
    assert_eq!(s.get_get_text(), "", "the model is on disk now");
}

/// An edit of another setting from outside rebinds the pages, and this one must go on.
#[test]
fn an_outside_config_edit_keeps_the_recordings_and_the_run() {
    let f = Fixture::new("reload", None);
    let s = f.state();
    s.invoke_record();
    fs::write(f.layout.config_file(), "version = 2\n# one\n").unwrap();
    f.window.invoke_files_check();
    assert!(s.get_recording(), "an open recording goes on");
    s.invoke_record();
    assert_eq!(f.takes()[0].duration, "0:01");

    s.invoke_run();
    let sent = f.take_sent();
    let Some(Request::TestTranscribe { id, .. }) = sent.last() else {
        panic!("first set");
    };
    fs::write(f.layout.config_file(), "version = 2\n# one and two\n").unwrap();
    f.window.invoke_files_check();
    assert_eq!(s.get_view(), 2, "the results stay open");
    assert_eq!(f.errors()[0], "Running…");
    assert_eq!(f.takes().len(), 1);
    f.answer(*id, heard(first_phrase()));
    assert_eq!(f.errors()[0], "0.0 %");
    let jobs: Vec<_> = f
        .take_sent()
        .into_iter()
        .filter(|r| matches!(r, Request::TestTranscribe { .. }))
        .collect();
    assert_eq!(jobs.len(), 1);
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
fn a_busy_daemon_ends_the_run_and_keeps_the_recordings() {
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
    assert_eq!(f.takes().len(), 1);
}

#[test]
fn an_answer_to_another_run_is_ignored() {
    let f = Fixture::new("stale", None);
    f.record_and_stop();
    f.state().invoke_run();
    let _ = f.take_sent();
    f.answer(999, heard("text"));
    f.ipc.deliver(&Event::Push(Push::TestProgress {
        id: 999,
        percent: 90,
        est_ms: Some(1000),
    }));
    assert_eq!(f.errors()[0], "Running…");
    assert_eq!(f.state().get_progress_text(), "0 %");
    assert_eq!(f.take_sent(), [] as [Request; 0]);
}

#[test]
fn offline_is_a_designed_state() {
    let f = Fixture::new("offline", None);
    f.ipc.deliver(&Event::Link(Link::Offline));
    f.record_and_stop();
    assert_eq!(f.takes()[0].duration, "0:01", "recording needs no daemon");
    assert_eq!(f.state().get_run_off(), "Start OxiYapper to run the test");
    f.state().invoke_run();
    assert_eq!(f.take_sent(), [] as [Request; 0]);
    f.ipc.deliver(&Event::Link(Link::Online));
    assert_eq!(f.state().get_run_off(), "");
    assert_eq!(
        f.take_sent(),
        [Request::HistoryList { older_than: None }],
        "back online, the history is listed"
    );
}

#[test]
fn losing_the_daemon_ends_the_run() {
    let f = Fixture::new("lost", None);
    f.record_and_stop();
    f.state().invoke_run();
    let (id, _, _) = f.one_job();
    f.progress(id, 50, Some(1000));
    thread::sleep(Duration::from_millis(100));
    mock_elapsed_time(Duration::from_millis(40));
    f.ipc.deliver(&Event::Link(Link::Offline));
    assert!(f.errors().iter().all(|e| e == "Not run"));
    assert_eq!(f.toast(), "OxiYapper stopped answering, so the test ended.");
    // DS-26: the bar of a failed run stops where it is, it never glides to 100 %.
    thread::sleep(Duration::from_millis(250));
    mock_elapsed_time(Duration::from_millis(100));
    let after = f.state().get_progress();
    assert!(after < 0.5, "{after}");
    assert_ne!(f.state().get_progress_text(), "100 %");
}

/// The named path of "new recordings are gone when the window closes": `PageState::forget`.
#[test]
fn closing_the_window_drops_the_new_recordings() {
    let f = Fixture::new("close", None);
    f.record_and_stop();
    f.list_history(vec![item(HISTORY_ID, "kept")]);
    f.add_first();
    f.record_and_stop();
    assert_eq!(f.takes().len(), 3);
    f.window
        .window()
        .dispatch_event(WindowEvent::CloseRequested);
    let takes = f.takes();
    assert_eq!(takes.len(), 1, "the history one is on disk anyway");
    assert_eq!(takes[0].reference, "kept");
}

#[test]
fn closing_while_recording_discards_it() {
    let f = Fixture::new("close-rec", None);
    f.state().invoke_record();
    f.window
        .window()
        .dispatch_event(WindowEvent::CloseRequested);
    assert!(!f.state().get_recording());
    assert!(f.takes().is_empty());
    assert_eq!(
        f.state().get_run_off(),
        "No recordings yet: dictate a few times with History on, or add one on Recordings"
    );
}

/// Runs the recordings held, with one more, every job heard as the first phrase.
fn finish_a_run(f: &Fixture) {
    f.record_and_stop();
    let last = i32::try_from(f.takes().len() - 1).unwrap();
    f.state()
        .invoke_reference_edited(last, first_phrase().into());
    f.state().invoke_run();
    while let Some((id, _, _)) = f.take_sent().into_iter().rev().find_map(|r| match r {
        Request::TestTranscribe { id, set, .. } => Some((id, set, None::<String>)),
        _ => None,
    }) {
        f.answer(id, heard(first_phrase()));
    }
}

/// d12: after Copy of a long single-line transcript (the page's clipboard helper holds the whole
/// text) nothing of the page passes the window's right edge. The testing backend's clipboard is in
/// memory: the real one is never touched.
/// e16: the card is placed by x/y/width in the scroll box, outside any layout: even a copier with
/// `min-width: 5000px` changes nothing here, while a 5000 px Transcript fails this test.
fn copy_fits(name: &str, w: f32, h: f32) {
    let f = Fixture::new(name, None);
    f.record_and_stop();
    f.state().invoke_reference_edited(0, first_phrase().into());
    f.state().invoke_run();
    while let Some(id) = f.take_sent().into_iter().rev().find_map(|r| match r {
        Request::TestTranscribe { id, .. } => Some(id),
        _ => None,
    }) {
        f.answer(id, heard(&"word".repeat(400)));
    }
    f.window.window().set_size(LogicalSize::new(w, h));
    mock_elapsed_time(Duration::from_millis(50));
    let button = ElementHandle::find_by_accessible_label(&f.window, "Copy")
        .next()
        .expect("the transcript card's Copy");
    button.invoke_accessible_default_action();
    mock_elapsed_time(Duration::from_millis(50));
    assert_eq!(f.window.global::<Toast>().get_text(), "Copied.");
    for label in ["Copy", "Transcript"] {
        let e = ElementHandle::find_by_accessible_label(&f.window, label)
            .next()
            .unwrap_or_else(|| panic!("no element labelled {label}"));
        let right = e.absolute_position().x + e.size().width;
        assert!(
            right <= w,
            "{label} ends at {right}, past the {w} px window"
        );
    }
}

#[test]
fn copying_a_long_transcript_leaves_the_page_inside_a_small_window() {
    copy_fits("copy-fits-720", 720.0, 520.0);
}

#[test]
fn copying_a_long_transcript_leaves_the_page_inside_a_large_window() {
    copy_fits("copy-fits-1280", 1280.0, 860.0);
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
        with_role(&f.window, "Run", AccessibleRole::Button).accessible_enabled(),
        Some(true)
    );
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
    f.state().set_view(0);
    // The rail, the view switch, the goal cards, the "i" after Run, Run (best settings: no list).
    for _ in 0..5 {
        press(&f.window, Key::Tab);
    }
    for space in [
        WindowEvent::KeyPressed { text: " ".into() },
        WindowEvent::KeyReleased { text: " ".into() },
    ] {
        f.window.window().dispatch_event(space);
    }
    assert_eq!(f.state().get_view(), 2, "Space on Run opened the results");
    mock_elapsed_time(Duration::from_millis(1));
    ctrl_tab(&f.window);
    assert_eq!(f.window.get_page(), Page::System);
}

#[test]
fn states_move_no_row() {
    let f = Fixture::new("no-move", None);
    f.window.window().set_size(LogicalSize::new(1280.0, 860.0));
    f.record_and_stop();
    let before = spots(&f.window);
    f.state().invoke_record();
    assert_nothing_moved(&before, &spots(&f.window));
    f.state().invoke_record();
    assert_nothing_moved(&before, &spots(&f.window));
    // Play turns into Stop in place.
    let play = with_role(&f.window, "Play", AccessibleRole::Button);
    let at = (play.absolute_position(), play.size());
    f.state().invoke_play(0);
    let stop = with_role(&f.window, "Stop", AccessibleRole::Button);
    assert_eq!((stop.absolute_position(), stop.size()), at);
}

/// The bottom of the lowest element outside the scrolling lists (a card cut by a list's edge
/// still reports its whole rect).
fn lowest(window: &AppWindow) -> f32 {
    const IN_LISTS: [&str; 7] = [
        "Play",
        "0:01",
        "Remove",
        "What was said",
        "Transcript",
        "Copy",
        "0.0 %",
    ];
    // The wizard's model list scrolls too: its names, badges and sizes.
    let in_model_list = |label: &str| {
        MODELS.iter().any(|m| m.display_name == label)
            || label.ends_with(" GB")
            || ["Recommended", "Not downloaded"].contains(&label)
    };
    spots(window)
        .iter()
        .filter(|(label, _, _)| {
            !IN_LISTS.contains(&label.as_str())
                && !label.contains("ecording")
                && !in_model_list(label)
        })
        .map(|(_, _, [_, y, _, h])| y + h)
        .fold(0.0, f32::max)
}

/// The page fits the window at every size in its fullest state: the recordings and the
/// transcripts scroll inside their own lists, so the page never does.
#[test]
fn no_scroll_at_the_minimum_or_the_default_size() {
    let f = Fixture::new("no-scroll", None);
    for i in 0..4 {
        f.record_and_stop();
        f.state().invoke_reference_edited(i, first_phrase().into());
    }
    finish_a_run(&f);
    for (w, h) in [(720.0, 520.0), (1000.0, 700.0), (1280.0, 800.0)] {
        f.window.window().set_size(LogicalSize::new(w, h));
        for view in [0, 1, 2] {
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
        f.state().set_view(2);
        spots(&f.window);
        let rows = sets().len();
        // Table cells are a line high; the transcripts' captions are not.
        let cells = ElementHandle::find_by_accessible_label(&f.window, "0.0 %")
            .filter(|e| e.size().height > 20.0)
            .count();
        assert_eq!(cells, rows, "all {rows} rows at {w}x{h}");
    }
}

/// Real-renderer shots of the page's states, for a person to read, off-screen and without taking
/// the focus: `OXI_TEST_SHOTS=<dir> cargo test -p oxi-settings --test page_test real_shots --
/// --test-threads=1` (the event loop needs the main thread). `real_window` cannot reach these
/// states: they come from recordings and a run.
#[cfg(windows)]
#[expect(clippy::too_many_lines, reason = "one script of shots")]
#[test]
fn real_shots_of_the_states() {
    use oxi_settings::ThemeChoice;
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

    /// Answers every job with `text`, up to `limit` of them, so the table fills in; each job
    /// first says its estimate, as the daemon does.
    fn answer_all(ipc: &Ipc, sent: &Receiver<Cmd>, text: &str, limit: usize) {
        let mut ms = 900;
        let mut answered = 0;
        while answered < limit {
            let Ok(cmd) = sent.try_recv() else { break };
            let id = match cmd {
                Cmd::Send(Request::TestTranscribe { id, .. }) => id,
                Cmd::Send(Request::HistoryRetranscribe { run, .. }) => run,
                _ => continue,
            };
            ipc.deliver(&Event::Push(Push::TestProgress {
                id,
                percent: 0,
                est_ms: Some(1500),
            }));
            ms += 700;
            answered += 1;
            ipc.deliver(&Event::Push(Push::TestResult {
                id,
                outcome: TestOutcome::Heard {
                    text: text.to_owned(),
                    ms,
                },
            }));
            if answered == limit {
                ipc.deliver(&Event::Push(Push::TestProgress {
                    id: id + 1,
                    percent: 40,
                    est_ms: None,
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
    fake(&layout, "turbo");
    fake(&layout, "small");
    let wav = layout.history_dir().join(format!("{HISTORY_ID}.wav"));
    fs::create_dir_all(wav.parent().unwrap()).unwrap();
    let speech: Vec<f32> = (0..40_000_u16)
        .map(|i| (f32::from(i) / 9.0).sin() * (f32::from(i) / 6_000.0).sin().abs() * 0.6)
        .collect();
    oxi_platform::wav::write_wav(&wav, &speech).unwrap();
    let (ipc, sent) = Ipc::fake();
    let window = oxi_settings::open_with(layout.clone(), ipc.clone()).unwrap();
    let (mut ctx, _) = Ctx::load(layout).unwrap();
    ctx.ipc = ipc.clone();
    window.set_page(Page::Test);
    window
        .window()
        .set_position(PhysicalPosition::new(-20_000, -20_000));
    let long_text = "A long reference text that wraps over many lines. ".repeat(12);
    let history = vec![
        edited(HISTORY_ID, "Добрый день, это проверка истории записей."),
        item(OLDER_ID, "Older one"),
    ];
    let mut shots: Vec<(&str, f32, f32, ThemeChoice)> = Vec::new();
    for name in ["wizard", "empty", "picked", "running", "results"] {
        for (w, h) in [(720.0, 520.0), (1280.0, 860.0), (1920.0, 1080.0)] {
            for theme in [ThemeChoice::Dark, ThemeChoice::Light] {
                shots.push((name, w, h, theme));
            }
        }
    }
    shots.push(("recording", 1280.0, 860.0, ThemeChoice::Dark));
    shots.push(("history-off", 1280.0, 860.0, ThemeChoice::Dark));

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
            ctx.config.borrow_mut().config.history.enabled = name != "history-off";
            bind_test_page(&window, &ctx, fake_mic, fake_play, None);
            let s = window.global::<TestState>();
            s.invoke_shown();
            ipc.deliver(&Event::Link(Link::Online));
            ipc.deliver(&Event::Push(Push::History {
                items: history.clone(),
                more: false,
            }));
            window.set_theme(theme);
            window.window().set_size(LogicalSize::new(w, h));
            s.set_view(i32::from(name != "wizard"));
            if matches!(name, "picked" | "running" | "results" | "recording") {
                s.invoke_goal_picked(1);
                s.invoke_record();
                s.invoke_record();
                s.set_history_pick(1);
                s.invoke_add_history();
                s.invoke_reference_edited(0, long_text.as_str().into());
            }
            match name {
                "recording" => s.invoke_record(),
                "running" => {
                    s.invoke_run();
                    answer_all(&ipc, &sent, first_phrase(), 3);
                }
                "results" => {
                    s.invoke_run();
                    answer_all(&ipc, &sent, first_phrase(), usize::MAX);
                }
                _ => {}
            }
            while sent.try_recv().is_ok() {}
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
        let file = dir.join(format!("{name}-{}x{}-{tone}.png", w as u32, h as u32));
        pixmap.save_png(file).unwrap();
        wait.set(0);
        next.set(i + 1);
    });
    window.run().unwrap();
}

/// Leaving the closed field after an arrow (Tab, a click elsewhere) puts the prompt back: the
/// walked name was only a preview.
#[test]
fn leaving_the_closed_history_field_after_an_arrow_puts_the_prompt_back() {
    let f = picker_fixture("history-leave");
    open_picker(&f);
    press(&f.window, Key::Escape);
    mock_elapsed_time(Duration::from_millis(1));
    press(&f.window, Key::DownArrow);
    assert_eq!(f.state().get_history_pick(), 1, "the arrow walked");
    press(&f.window, Key::Tab);
    mock_elapsed_time(Duration::from_millis(1));
    assert_eq!(f.state().get_history_pick(), 0, "Tab away");
    assert!(f.takes().is_empty());
}
