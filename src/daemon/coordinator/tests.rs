use std::cell::{Cell, RefCell};
use std::fs;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc::channel;

use oxi_core::config::{ConfigError, Hotkey};
use oxi_core::machine::{BOUNCE, EmptyAudio, ModelFailure, Notice, OverlayState, Sound};
use oxi_core::tuning::SettingsSet;
use oxi_core::types::Language;

use crate::daemon::cmd::TestJob;
use oxi_platform::os::overlay::Progress;

use super::*;
use crate::log::scratch::scratch_dir;

#[derive(Default)]
struct FakeHook {
    held: Cell<bool>,
    polls: Cell<u32>,
    hotkeys: RefCell<Vec<Hotkey>>,
    pressed: Cell<Option<std::time::Instant>>,
}

impl ChordHook for Rc<FakeHook> {
    fn chord_held(&self) -> bool {
        self.polls.set(self.polls.get() + 1);
        self.held.get()
    }
    fn reinstall(&self) {}
    fn set_hotkey(&self, hotkey: &Hotkey) {
        self.hotkeys.borrow_mut().push(hotkey.clone());
    }
    fn take_pressed(&self) -> Option<std::time::Instant> {
        self.pressed.take()
    }
}

struct Rig {
    c: Coordinator<Rc<FakeHook>>,
    hook: Rc<FakeHook>,
    phase: PhaseCell,
    audio: Receiver<AudioCmd>,
    whisper: Receiver<WhisperCmd>,
    paste: Receiver<PasteCmd>,
    sound: Receiver<SoundCmd>,
    history: Receiver<HistoryCmd>,
    ui: Receiver<UiCmd>,
    /// What the overlay would draw.
    progress: Progress,
}

fn drain<T>(rx: &Receiver<T>) -> Vec<T> {
    rx.try_iter().collect()
}

impl Rig {
    /// A coordinator with the default config; the start-up config commands are drained.
    fn new() -> Self {
        let mut config = Config::default();
        config.hotkey.esc_cancels = true; // the Esc tests; off is the default
        Self::with(config)
    }

    fn with(config: Config) -> Self {
        let (audio_tx, audio) = channel();
        let (whisper_tx, whisper) = channel();
        let (paste_tx, paste) = channel();
        let (sound_tx, sound) = channel();
        let (history_tx, history) = channel();
        let (ui_tx, ui) = channel();
        let progress = Progress::default();
        let workers = Workers {
            audio: audio_tx,
            whisper: whisper_tx,
            paste: paste_tx,
            sound: sound_tx,
            history: history_tx,
            ui: ui_tx,
            wake_ui: Box::new(|| {}),
            progress: progress.clone(),
        };
        let hook = Rc::new(FakeHook {
            held: Cell::new(true),
            ..FakeHook::default()
        });
        let phase = PhaseCell::default();
        let mut c = Coordinator::new(config, Vec::new(), workers, Rc::clone(&hook), phase.clone());
        c.foreground = Box::new(|| "notepad.exe".into());
        let rig = Self {
            c,
            hook,
            phase,
            audio,
            whisper,
            paste,
            sound,
            history,
            ui,
            progress,
        };
        rig.drain_all();
        rig
    }

    fn drain_all(&self) {
        drain(&self.audio);
        drain(&self.whisper);
        drain(&self.paste);
        drain(&self.sound);
        drain(&self.history);
        drain(&self.ui);
    }

    fn event(&mut self, event: Event, at: u64) {
        self.c.handle(Inbox::Event(event), Millis(at));
    }

    /// The paste worker's answer: `job` was pasted (the entry is filed now).
    fn land(&mut self, job: JobId, at: u64) {
        let outcome = PasteOutcome::Pasted;
        self.event(Event::PasteDone { job, outcome }, at);
    }

    /// Holds the chord from 0 to 1000 ms; returns the stopped job.
    fn record(&mut self) -> JobId {
        self.event(Event::ChordDown, 0);
        self.event(Event::ChordUp, 1000);
        self.event(Event::Tick, 2000); // past the release tail
        let stops: Vec<_> = drain(&self.audio)
            .into_iter()
            .filter_map(|cmd| match cmd {
                AudioCmd::Stop(job) => Some(job),
                _ => None,
            })
            .collect();
        assert_eq!(stops.len(), 1, "one Stop per recording");
        stops[0]
    }

    /// Records, transcribes `text` and returns the job being pasted.
    fn transcribed(&mut self, text: &str) -> JobId {
        self.event(Event::ModelLoaded, 0);
        let job = self.record();
        self.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.5; 4],
                spool: None,
                voiced_ms: 0,
            },
            Millis(2100),
        );
        self.c.handle(
            Inbox::Transcribed {
                job,
                text: Some(text.to_owned()),
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(2500),
        );
        job
    }
}

#[test]
fn the_last_model_load_failure_shows_in_status_until_a_load_works() {
    let mut rig = Rig::new();
    assert_eq!(rig.c.status().model_failure, None);
    for why in [ModelFailure::Bad, ModelFailure::OutOfMemory] {
        rig.event(Event::ModelFailed(why), 0);
        assert_eq!(rig.c.status().model_failure, Some(why));
        rig.event(Event::ModelLoaded, 10);
        assert_eq!(rig.c.status().model_failure, None);
    }
}

#[test]
fn a_resume_or_an_unlock_tells_the_window_to_redraw() {
    let bus = crate::daemon::ipc::test_bus::OnBus::connected();
    let mut rig = Rig::new();
    let woke = || bus.next(500, |p| matches!(p, Push::Woke).then_some(()));
    rig.event(Event::Tick, 0);
    assert!(woke().is_none(), "nothing woke yet");
    for (event, at) in [(Event::Resume, 10), (Event::Unlock, 20)] {
        let name = format!("{event:?}");
        rig.event(event, at);
        assert!(woke().is_some(), "no Woke after {name}");
    }
}

#[test]
fn the_overlay_bar_follows_the_phases_and_never_waits_for_whisper() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    assert!(!rig.progress.active());
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    assert_eq!(rig.c.state.phase(), Phase::Transcribing);
    assert!(rig.progress.active(), "the estimate starts the bar at once");
    assert_eq!(rig.c.bar, Bar::Transcribing(job));
    let other = JobId(job.0 + 1);
    rig.c.handle(
        Inbox::Progress {
            job: other,
            percent: 70,
        },
        Millis(2150),
    );
    assert_eq!(
        rig.c.bar,
        Bar::Transcribing(job),
        "another job's report is late"
    );
    rig.c
        .handle(Inbox::Progress { job, percent: 40 }, Millis(2200));
    rig.c.handle(
        Inbox::Transcribed {
            job,
            text: Some("x".into()),
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(2500),
    );
    assert_eq!(rig.c.state.phase(), Phase::Pasting);
    assert_eq!(rig.c.bar, Bar::None, "a new phase is a new bar");
    assert!(
        rig.progress.finishing_left().is_some(),
        "the transcription bar runs to its end"
    );
    let scale = rig.c.speed.values().next().copied().unwrap_or(1.0);
    assert!(scale > 0.0, "the run was timed");
    // A clipboard paste only reports that the text landed: no bar, and the Pasting pill
    // shows none (review c04 #4: not the finished transcription's 100).
    rig.c
        .handle(Inbox::Progress { job, percent: 100 }, Millis(2520));
    assert_eq!(rig.c.bar, Bar::None);
    assert_eq!(rig.progress.get(OverlayState::Pasting), None);
    // Typing: the first report starts its bar, the landing finishes it. The held
    // Transcribing pill keeps its own bar (review c04 #3: no 100 -> 3 jump).
    rig.c
        .handle(Inbox::Progress { job, percent: 40 }, Millis(2540));
    assert_eq!(rig.c.bar, Bar::Typing(job));
    assert!(rig.progress.get(OverlayState::Pasting).is_some());
    assert!(
        rig.progress.get(OverlayState::Transcribing).is_some(),
        "the transcription bar is still finishing"
    );
    rig.c
        .handle(Inbox::Progress { job, percent: 100 }, Millis(2560));
    assert_eq!(rig.c.bar, Bar::None);
    assert!(rig.progress.finishing_left().is_some());
    rig.event(
        Event::PasteDone {
            job,
            outcome: PasteOutcome::Typed,
        },
        2600,
    );
    assert!(!rig.progress.active(), "the next pill starts clean");
    // Review c04 #5: hiding does not cut a finish the held pill is showing.
    assert!(rig.progress.finishing_left().is_some());
    // A late report after the job ended is not kept.
    rig.c
        .handle(Inbox::Progress { job, percent: 90 }, Millis(2700));
    assert_eq!(rig.c.bar, Bar::None);
}

/// Review c04 #6: a job sent to whisper while an earlier one is still in it gets its own
/// bar from 0 (its reports are not dropped), and its time, which includes the wait, is not
/// learned as the speed.
#[test]
fn a_job_queued_behind_another_gets_its_own_bar_and_teaches_no_speed() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    assert_eq!(rig.c.bar, Bar::Transcribing(job));
    let next = JobId(job.0 + 1);
    rig.c.audio.insert(next, vec![0.5; 4]);
    assert!(rig.c.transcribe(next).is_none());
    assert_eq!(rig.c.bar, Bar::Transcribing(next));
    let run = rig.c.run.expect("timed");
    assert!(run.job == next && run.started.is_some() && run.queued);
    rig.c.overlay_progress(OverlayState::Pasting);
    assert!(rig.c.speed.is_empty(), "a queued run teaches nothing");
}

#[test]
fn the_window_is_told_when_its_status_changes_and_not_for_the_phase_alone() {
    // A bus another test set would make `handle` take the changed status before we do.
    let _no_bus = crate::daemon::lock(&crate::daemon::ipc::test_bus::BUS_TEST);
    let mut rig = Rig::new();
    assert!(
        rig.c.changed_status().is_some(),
        "the first one after it connects"
    );
    assert!(rig.c.changed_status().is_none());
    rig.record();
    assert_ne!(rig.c.state.phase(), Phase::Idle);
    let pushed = rig.c.changed_status();
    assert!(pushed.is_none(), "a phase alone is no push: {pushed:?}");
    rig.c.handle(Inbox::Backend("Vulkan0".into()), Millis(3000));
    let report = rig.c.changed_status().expect("a new backend is");
    assert_eq!(report.backend, "Vulkan0");
    assert!(rig.c.changed_status().is_none());
    // Review c04 #7: what the window asked for itself is not pushed a second time.
    rig.c.handle(Inbox::Backend("Vulkan1".into()), Millis(3100));
    let (tx, rx) = channel();
    rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(3200));
    assert_eq!(rx.try_recv().unwrap().backend, "Vulkan1");
    assert!(rig.c.changed_status().is_none());
}

#[test]
fn a_model_delete_waits_for_idle() {
    let mut rig = Rig::new();
    let reply = crate::daemon::ipc::Pushes::default();
    let (_, window) = reply.claim(1);
    let ask = || {
        Inbox::Ipc(IpcAsk::DeleteModel {
            id: "turbo".into(),
            reply: reply.clone(),
        })
    };
    rig.c.handle(ask(), Millis(0));
    assert!(
        drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::DeleteModel { id, .. } if id == "turbo"))
    );
    assert!(window.try_recv().is_err(), "the whisper worker answers");
    rig.event(Event::ChordDown, 100);
    rig.c.handle(ask(), Millis(200));
    assert!(
        !drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::DeleteModel { .. }))
    );
    assert_eq!(
        window.try_recv().unwrap(),
        Push::ModelDeleted {
            id: "turbo".into(),
            result: DeleteResult::InUse
        }
    );
}

#[test]
fn a_recognition_test_runs_only_while_the_machine_is_idle() {
    let mut rig = Rig::new();
    let reply = crate::daemon::ipc::Pushes::default();
    let (_, pushes) = reply.claim(1);
    let job = |id| {
        Inbox::Ipc(IpcAsk::Test(TestJob {
            id,
            set: SettingsSet::defaults(),
            language: Language::RU,
            samples: vec![0.1],
            origin: crate::daemon::ipc::Origin::Mic,
            reply: reply.clone(),
        }))
    };
    rig.c.handle(job(1), Millis(0));
    assert!(
        drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::Test(j) if j.id == 1))
    );
    assert!(pushes.try_recv().is_err());

    rig.event(Event::ChordDown, 0);
    drain(&rig.whisper);
    rig.c.handle(job(2), Millis(1));
    assert!(drain(&rig.whisper).is_empty());
    assert_eq!(
        pushes.try_recv().ok(),
        Some(Push::TestResult {
            id: 2,
            outcome: TestOutcome::Failed {
                why: TestFailure::Busy
            }
        })
    );
}

#[test]
fn chord_down_starts_capture_preloads_and_shows_overlay() {
    let mut rig = Rig::new();
    rig.event(Event::ChordDown, 0);

    assert_eq!(
        drain(&rig.audio),
        [AudioCmd::Start {
            session: 1,
            pressed: None
        }]
    );
    assert!(
        drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::Preload))
    );
    let ui = drain(&rig.ui);
    assert!(ui.contains(&UiCmd::Tray(Phase::Recording)));
    assert!(
        ui.iter()
            .any(|c| matches!(c, UiCmd::Overlay(OverlayState::Recording { .. })))
    );
    assert_eq!(rig.phase.load(), Phase::Recording);
}

#[test]
fn job_audio_goes_to_whisper_and_text_transcribed() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples: vec![0.25, -0.5],
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    let transcribe: Vec<_> = drain(&rig.whisper)
        .into_iter()
        .filter_map(|c| match c {
            WhisperCmd::Transcribe { job, samples, .. } => Some((job, samples)),
            _ => None,
        })
        .collect();
    assert_eq!(transcribe, [(job, vec![0.25, -0.5])]);

    rig.c.handle(
        Inbox::Transcribed {
            job,
            text: Some("hello".into()),
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(2500),
    );
    let pastes = drain(&rig.paste);
    let [
        PasteCmd::Paste {
            job: j, text, cfg, ..
        },
    ] = pastes.as_slice()
    else {
        panic!("expected one Paste");
    };
    assert_eq!((*j, text.as_str()), (job, "hello"));
    assert_eq!(*cfg, Config::default().paste);
}

#[test]
fn paste_done_keeps_last_text_for_copy_even_when_it_failed() {
    for outcome in [PasteOutcome::Pasted, PasteOutcome::Failed] {
        let mut rig = Rig::new();
        rig.c.handle(Inbox::CopyLastText, Millis(0));
        assert!(drain(&rig.paste).is_empty(), "no text yet, nothing to copy");

        let job = rig.transcribed("hello");
        rig.drain_all(); // the Paste itself
        rig.event(Event::PasteDone { job, outcome }, 3000);
        assert!(drain(&rig.ui).contains(&UiCmd::CanCopy));

        rig.c.handle(Inbox::CopyLastText, Millis(3100));
        let copies = drain(&rig.paste);
        let [PasteCmd::CopyOnly { text, .. }] = copies.as_slice() else {
            panic!("expected one CopyOnly after {outcome:?}");
        };
        assert_eq!(text, "hello");
    }
}

#[test]
fn copy_last_text_follows_the_history_setting_applied_after_the_dictation() {
    for dictated_with in [true, false] {
        let mut config = Config::default();
        config.paste.keep_in_clipboard_history = dictated_with;
        let mut rig = Rig::with(config.clone());
        let job = rig.transcribed("hello");
        rig.event(
            Event::PasteDone {
                job,
                outcome: PasteOutcome::Pasted,
            },
            3000,
        );
        rig.drain_all();

        config.paste.keep_in_clipboard_history = !dictated_with;
        rig.c.handle(
            Inbox::Config {
                config: Box::new(config),
                warnings: Vec::new(),
            },
            Millis(3050),
        );
        rig.drain_all();

        rig.c.handle(Inbox::CopyLastText, Millis(3100));
        let copies = drain(&rig.paste);
        let [PasteCmd::CopyOnly { in_history, .. }] = copies.as_slice() else {
            panic!("expected one CopyOnly");
        };
        assert_eq!(*in_history, !dictated_with);
    }
}

#[test]
fn history_gets_the_audio_and_text_only_when_on_and_only_for_a_heard_dictation() {
    let saves = |rig: &Rig| {
        drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save {
                    samples,
                    text,
                    words,
                    cfg,
                    ..
                } => Some((samples.len(), text, words, cfg)),
                HistoryCmd::Config(_) => None,
            })
            .collect::<Vec<_>>()
    };
    let heard = vec![oxi_core::history::Word {
        w: "hello".into(),
        p: 0.3,
        t0: None,
        t1: None,
    }];
    let dictate = |rig: &mut Rig, text: Option<&str>| {
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        let samples = vec![0.1; 16_000];
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples,
                spool: None,
                voiced_ms: 0,
            },
            Millis(2100),
        );
        let pasted = text.is_some_and(|t| !t.trim().is_empty());
        let text = text.map(str::to_owned);
        let words = heard.clone();
        let held = Vec::new();
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words,
                held,
            },
            Millis(2500),
        );
        if pasted {
            rig.land(job, 2600);
        }
    };
    let mut config = Config::default();
    config.history.enabled = false;
    let mut off = Rig::with(config);
    dictate(&mut off, Some("hello"));
    assert!(saves(&off).is_empty(), "off writes nothing");
    assert!(off.c.history_audio.is_empty());

    let mut config = Config::default();
    config.history.enabled = true;
    let mut on = Rig::with(config);
    dictate(&mut on, Some("hello"));
    let [(len, text, words, cfg)] = saves(&on).try_into().unwrap();
    assert_eq!((len, text.as_str(), cfg.enabled), (16_000, "hello", true));
    assert_eq!(words, heard, "whisper's words go into the entry");
    assert!(on.c.history_audio.is_empty() && on.c.words.is_empty());

    for text in [None, Some("  ")] {
        let mut rig = Rig::with(on.c.active.clone());
        dictate(&mut rig, text);
        assert!(saves(&rig).is_empty(), "nothing heard is not saved");
        assert!(rig.c.history_audio.is_empty(), "no audio kept for it");
    }
}

#[test]
fn held_parts_go_to_the_entry_and_an_all_held_dictation_is_filed_unpasted() {
    use oxi_core::history::{Delivered, HeldPart};
    use oxi_core::text::HeldWhy;
    let part = |text: &str| HeldPart {
        text: text.into(),
        t0: 100,
        t1: 900,
        why: HeldWhy::LowConfidence,
    };
    let dictate = |text: &str, held: Vec<HeldPart>| {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.1; 16_000],
                spool: None,
                voiced_ms: 0,
            },
            Millis(2100),
        );
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text: Some(text.into()),
                words: Vec::new(),
                held,
            },
            Millis(2500),
        );
        if !text.is_empty() {
            rig.land(job, 2600);
        }
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save {
                    text,
                    held,
                    delivered,
                    ..
                } => Some((text, held, delivered)),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        (rig, saves)
    };
    // Some held: the entry gets them beside the inserted text, filed once pasted; "Copy last
    // text" has both, so the held part outlives a failed write (review d03 #3).
    let (rig, saves) = dictate("kept", vec![part("doubtful")]);
    let pasted = Some(Delivered::Pasted);
    assert_eq!(saves, [("kept".to_owned(), vec![part("doubtful")], pasted)]);
    assert_eq!(rig.c.last_text.as_deref(), Some("kept doubtful"));
    // All held: nothing is pasted, the entry's text is the held parts, filed as filtered.
    let (rig, saves) = dictate("", vec![part("first"), part("second")]);
    assert_eq!(
        saves,
        [(
            "first second".to_owned(),
            vec![part("first"), part("second")],
            Some(Delivered::Filtered)
        )]
    );
    assert!(rig.c.text.is_empty() && rig.c.held.is_empty() && rig.c.history_audio.is_empty());
    assert_eq!(rig.c.state.phase(), Phase::Idle, "no paste follows");
    assert_eq!(
        rig.c.last_text.as_deref(),
        Some("first second"),
        "review d03 #3"
    );
}

/// The `Save`s sent since the last look: audio length, text, how it arrived.
fn saved(rig: &Rig) -> Vec<(usize, String, Option<oxi_core::history::Delivered>)> {
    drain(&rig.history)
        .into_iter()
        .filter_map(|c| match c {
            HistoryCmd::Save {
                samples,
                text,
                delivered,
                ..
            } => Some((samples.len(), text, delivered)),
            HistoryCmd::Config(_) => None,
        })
        .collect()
}

/// Review d03 #7: an all-held part a press inside the gap carries on is not filed alone (that
/// took its audio from the merge); given back by a failed continuation, it is filed then.
#[test]
fn an_all_held_part_carried_on_waits_for_the_merge() {
    use oxi_core::history::{Delivered, HeldPart};
    let held = || {
        vec![HeldPart {
            text: "maybe".into(),
            t0: 0,
            t1: 500,
            why: oxi_core::text::HeldWhy::Hallucination,
        }]
    };
    for give_back in [false, true] {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100); // job 0, its gap lasts until 1400
        let audio = Inbox::JobAudio {
            job: JobId(0),
            samples: vec![0.1; 8000],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio, Millis(1150));
        rig.event(Event::ChordDown, 1300); // inside the gap: carries job 0 on
        let all_held = Inbox::Transcribed {
            job: JobId(0),
            text: Some(String::new()),
            words: Vec::new(),
            held: held(),
        };
        rig.c.handle(all_held, Millis(1350));
        assert_eq!(saved(&rig), [], "not filed alone");
        assert!(rig.c.history_audio.contains_key(&JobId(0)), "audio kept");
        if give_back {
            let fail = Inbox::Event(Event::ModelFailed(ModelFailure::Bad));
            rig.c.handle(fail, Millis(1500));
            let want = (8000, "maybe".to_owned(), Some(Delivered::Filtered));
            assert_eq!(saved(&rig), [want]);
            assert_eq!(rig.c.last_text.as_deref(), Some("maybe"));
            continue;
        }
        rig.event(Event::ChordUp, 2000);
        rig.event(Event::Tick, 2100);
        let audio = Inbox::JobAudio {
            job: JobId(1),
            samples: vec![0.1; 4000],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio, Millis(2150));
        rig.c
            .handle(transcribed(JobId(1), Some("one two")), Millis(2200));
        rig.event(Event::Tick, 2600);
        rig.land(JobId(1), 2700);
        let want = (12_000, "one two".to_owned(), Some(Delivered::Pasted));
        assert_eq!(saved(&rig), [want], "one entry with both parts' audio");
        assert!(rig.c.held.is_empty() && rig.c.words.is_empty());
    }
}

/// Review d03 (e): the entry says how the text reached the user, typed and pasted alike;
/// History turned off before the paste ends is off at the save, and the held part is still
/// in "Copy last text" (review d03 #4).
#[test]
fn the_entry_says_how_the_text_arrived() {
    use oxi_core::history::{Delivered, HeldPart};
    let outcomes = [
        (PasteOutcome::Typed, Delivered::Typed),
        (PasteOutcome::PastedNoRestore, Delivered::Pasted),
        (
            PasteOutcome::ClipboardOnly(ClipboardOnlyWhy::ModifiersHeld),
            Delivered::Clipboard,
        ),
        (PasteOutcome::Failed, Delivered::Failed),
        (PasteOutcome::FileFailed, Delivered::FileFailed),
    ];
    for (outcome, want) in outcomes {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        let job = rig.transcribed("hi");
        rig.event(Event::PasteDone { job, outcome }, 3000);
        assert_eq!(saved(&rig), [(4, "hi".to_owned(), Some(want))]);
    }
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config.clone());
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    let audio = Inbox::JobAudio {
        job,
        samples: vec![0.5; 4],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(2100));
    config.history.enabled = false;
    let config = Box::new(config);
    let warnings = Vec::new();
    rig.c
        .handle(Inbox::Config { config, warnings }, Millis(2200));
    let part = HeldPart {
        text: "maybe".into(),
        t0: 0,
        t1: 1,
        why: oxi_core::text::HeldWhy::LowConfidence,
    };
    let heard = Inbox::Transcribed {
        job,
        text: Some("hi".into()),
        words: Vec::new(),
        held: vec![part],
    };
    rig.c.handle(heard, Millis(2500));
    rig.land(job, 3000);
    assert_eq!(rig.c.last_text.as_deref(), Some("hi maybe"));
}

#[test]
fn a_dictation_with_history_off_still_sends_the_prune() {
    let mut config = Config::default();
    config.history.enabled = false;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    let samples = vec![0.1; 16_000];
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples,
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    let text = Some("hello".to_owned());
    rig.c.handle(
        Inbox::Transcribed {
            job,
            text,
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(2500),
    );
    rig.land(job, 2600);
    let sent = drain(&rig.history);
    let [HistoryCmd::Config(cfg)] = sent.as_slice() else {
        panic!("expected one prune, got {} commands", sent.len());
    };
    assert!(!cfg.enabled);
}

#[test]
fn a_re_press_inside_the_hold_gap_makes_one_dictation_from_both_parts() {
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1100); // the tail ends: job 0, its gap lasts until 1400
    let audio = |job, len| Inbox::JobAudio {
        job,
        samples: vec![0.1; len],
        spool: None,
        voiced_ms: 0,
    };
    let done = |job, text: &str| Inbox::Transcribed {
        job,
        text: Some(text.to_owned()),
        words: Vec::new(),
        held: Vec::new(),
    };
    rig.c.handle(audio(JobId(0), 8000), Millis(1150));
    rig.c.handle(done(JobId(0), "one"), Millis(1200));
    assert!(drain(&rig.paste).is_empty(), "the paste waits for the gap");
    rig.event(Event::ChordDown, 1300); // inside the gap: the dictation goes on
    rig.c.handle(Inbox::Event(Event::ChordUp), Millis(2000));
    rig.event(Event::Tick, 2100);
    rig.c.handle(audio(JobId(1), 4000), Millis(2150));
    rig.c.handle(done(JobId(1), "one two"), Millis(2200));
    rig.event(Event::Tick, 2600);

    let lens: Vec<_> = drain(&rig.whisper)
        .into_iter()
        .filter_map(|c| match c {
            WhisperCmd::Transcribe { job, samples, .. } => Some((job, samples.len())),
            _ => None,
        })
        .collect();
    assert_eq!(lens, [(JobId(0), 8000), (JobId(1), 12_000)]);
    let pasted: Vec<_> = drain(&rig.paste)
        .into_iter()
        .filter_map(|c| match c {
            PasteCmd::Paste { job, text, .. } => Some((job, text)),
            PasteCmd::CopyOnly { .. } | PasteCmd::File { .. } => None,
        })
        .collect();
    assert_eq!(pasted, [(JobId(1), "one two".to_owned())], "one paste");
    rig.land(JobId(1), 5000);
    let saves: Vec<_> = drain(&rig.history)
        .into_iter()
        .filter_map(|c| match c {
            HistoryCmd::Save { samples, text, .. } => Some((samples.len(), text)),
            HistoryCmd::Config(_) => None,
        })
        .collect();
    assert_eq!(saves, [(12_000, "one two".to_owned())], "one history entry");
    assert!(rig.c.history_audio.is_empty() && rig.c.audio.is_empty());
    assert!(rig.c.merges.is_empty());
    // Review d04 #3: the carried part's profile ends with the merge.
    assert!(rig.c.profiles.is_empty(), "{:?}", rig.c.profiles.keys());
}

/// Whisper answers the first part with no text (a short word, a filtered hallucination) or
/// fails while the user records the continuation: no notice, no error sound, and the first
/// part's audio still goes in front of the merged dictation, with history off too.
#[test]
fn no_text_for_a_carried_part_keeps_its_audio_for_the_merge() {
    for (history, text) in [(false, None), (false, Some(String::new())), (true, None)] {
        let case = format!("history {history}, text {text:?}");
        let mut config = Config::default();
        config.history.enabled = history;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100); // job 0, its gap lasts until 1400
        let audio = |job, len| Inbox::JobAudio {
            job,
            samples: vec![0.1; len],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio(JobId(0), 8000), Millis(1150)); // job 0 goes to whisper
        rig.event(Event::ChordDown, 1300); // carries job 0 on while whisper has it
        rig.drain_all();
        let job = JobId(0);
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(1350),
        );
        assert!(drain(&rig.sound).is_empty(), "{case}");
        let said = drain(&rig.ui)
            .into_iter()
            .any(|c| matches!(c, UiCmd::Notify(_)));
        assert!(!said, "{case}");
        rig.c.handle(Inbox::Event(Event::ChordUp), Millis(2000));
        rig.event(Event::Tick, 2100);
        rig.c.handle(audio(JobId(1), 4000), Millis(2150));
        let sent: Vec<_> = drain(&rig.whisper)
            .into_iter()
            .filter_map(|c| match c {
                WhisperCmd::Transcribe { job, samples, .. } => Some((job, samples.len())),
                _ => None,
            })
            .collect();
        assert_eq!(sent, [(JobId(1), 12_000)], "{case}");
        let text = Some("one two".to_owned());
        rig.c.handle(
            Inbox::Transcribed {
                job: JobId(1),
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(2200),
        );
        rig.event(Event::Tick, 2600);
        rig.land(JobId(1), 2700);
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save { samples, .. } => Some(samples.len()),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        let want: &[usize] = if history { &[12_000] } else { &[] };
        assert_eq!(saves, want, "{case}");
        assert!(rig.c.history_audio.is_empty() && rig.c.merges.is_empty());
    }
}

/// A lock or sleep during a continuation cancels the whole dictation: nothing pastes into
/// the locking session, and with history on both parts are kept as one cancelled entry.
#[test]
fn a_lock_during_a_continuation_keeps_both_parts_as_one_cancel() {
    for event in [Event::Lock, Event::Suspend] {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100);
        let audio = |job, len| Inbox::JobAudio {
            job,
            samples: vec![0.1; len],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio(JobId(0), 8000), Millis(1150));
        let text = Some("one".to_owned());
        let job = JobId(0);
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(1200),
        );
        rig.event(Event::ChordDown, 1300);
        rig.event(event.clone(), 1500);
        rig.c.handle(audio(JobId(1), 4000), Millis(1600));
        rig.event(Event::Tick, 5000);
        assert!(drain(&rig.paste).is_empty(), "{event:?}");
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save { samples, text, .. } => Some((samples.len(), text)),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        assert_eq!(saves, [(12_000, String::new())], "{event:?}");
        assert!(rig.c.history_audio.is_empty() && rig.c.text.is_empty());
        assert!(!rig.c.state.busy(), "{event:?}");
    }
}

/// A press inside the hold gap, then the mic or the model fails during the continuation:
/// the first part is no part of the failure. Its text pastes and its recording is saved as
/// it was, nothing of it is dropped.
#[test]
fn a_failed_continuation_gives_the_first_part_back_to_paste_and_save() {
    let capture_failed = |rig: &Rig| Inbox::Capture {
        session: rig.c.capture,
        event: Event::CaptureFailed,
    };
    let model_failed = |_: &Rig| Inbox::Event(Event::ModelFailed(ModelFailure::Bad));
    let failures: [&dyn Fn(&Rig) -> Inbox; 2] = [&capture_failed, &model_failed];
    for failure in failures {
        let mut config = Config::default();
        config.history.enabled = true;
        let mut rig = Rig::with(config);
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        rig.event(Event::ChordUp, 1000);
        rig.event(Event::Tick, 1100); // job 0, its gap lasts until 1400
        let audio = Inbox::JobAudio {
            job: JobId(0),
            samples: vec![0.1; 8000],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio, Millis(1150));
        let text = Some("one".to_owned());
        rig.c.handle(
            Inbox::Transcribed {
                job: JobId(0),
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(1200),
        );
        rig.event(Event::ChordDown, 1300); // carries job 0 on
        let fail = failure(&rig);
        rig.c.handle(fail, Millis(1500));
        // The failed part keeps its own audio as job 1, alone.
        let audio = Inbox::JobAudio {
            job: JobId(1),
            samples: vec![0.1; 4000],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio, Millis(1600));

        let pasted: Vec<_> = drain(&rig.paste)
            .into_iter()
            .filter_map(|c| match c {
                PasteCmd::Paste { job, text, .. } => Some((job, text)),
                PasteCmd::CopyOnly { .. } | PasteCmd::File { .. } => None,
            })
            .collect();
        assert_eq!(pasted, [(JobId(0), "one".to_owned())]);
        rig.land(JobId(0), 5000);
        let saves: Vec<_> = drain(&rig.history)
            .into_iter()
            .filter_map(|c| match c {
                HistoryCmd::Save { samples, text, .. } => Some((samples.len(), text)),
                HistoryCmd::Config(_) => None,
            })
            .collect();
        assert_eq!(saves, [(8000, "one".to_owned())]);
        assert!(rig.c.merges.is_empty());
        let outcome = PasteOutcome::Pasted;
        rig.event(
            Event::PasteDone {
                job: JobId(0),
                outcome,
            },
            1700,
        );
        assert!(rig.c.audio.is_empty() && rig.c.history_audio.is_empty());
        assert!(rig.c.spoken.is_empty() && rig.c.text.is_empty());
    }
}

/// History off, the failure comes before the first part's audio: that audio still arrives,
/// is transcribed and pasted, and nothing of it stays in memory.
#[test]
fn a_failure_before_the_first_parts_audio_leaks_nothing_and_loses_nothing() {
    let mut config = Config::default();
    config.history.enabled = false;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1100);
    rig.event(Event::ChordDown, 1300); // job 0 still waits for its audio
    let fail = Inbox::Capture {
        session: rig.c.capture,
        event: Event::CaptureFailed,
    };
    rig.c.handle(fail, Millis(1400));
    let audio = Inbox::JobAudio {
        job: JobId(0),
        samples: vec![0.1; 8000],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(1500));
    let sent: Vec<_> = drain(&rig.whisper)
        .into_iter()
        .filter_map(|c| match c {
            WhisperCmd::Transcribe { job, samples, .. } => Some((job, samples.len())),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [(JobId(0), 8000)]);
    let text = Some("one".to_owned());
    rig.c.handle(
        Inbox::Transcribed {
            job: JobId(0),
            text,
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(1600),
    );
    let pasted = drain(&rig.paste)
        .into_iter()
        .any(|c| matches!(c, PasteCmd::Paste { job: JobId(0), .. }));
    assert!(pasted);
    let outcome = PasteOutcome::Pasted;
    rig.event(
        Event::PasteDone {
            job: JobId(0),
            outcome,
        },
        1700,
    );
    assert!(rig.c.audio.is_empty() && rig.c.spoken.is_empty());
    assert!(rig.c.history_audio.is_empty() && rig.c.text.is_empty());
    assert!(!rig.c.state.busy());
}

/// A quiet word under the gate with saving on: the recording it streamed is filed at once.
#[test]
fn nothing_heard_files_the_streamed_recording_now() {
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    drain(&rig.history);
    let why = EmptyAudio::NothingHeard;
    rig.event(Event::JobAudioEmpty { job, why }, 2100);
    let sent = drain(&rig.history);
    assert!(
        matches!(sent.as_slice(), [HistoryCmd::Config(cfg)] if cfg.enabled),
        "one config pass files the kept part"
    );
}

/// Review d03-fix #3: History turned on during a dictation; the whisper worker still has the
/// old config, so the invention it holds is filed in the entry, not dropped. With History off
/// it is dropped (a count only).
#[test]
fn an_invention_is_filed_when_history_was_turned_on_during_the_dictation() {
    use oxi_core::history::{Delivered, HeldPart};
    let run = |turn_on: bool| {
        let mut config = Config::default();
        config.history.enabled = false;
        let mut rig = Rig::with(config.clone());
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        if turn_on {
            config.history.enabled = true;
            let config = Box::new(config);
            let warnings = Vec::new();
            rig.c
                .handle(Inbox::Config { config, warnings }, Millis(2050));
        }
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.5; 4],
                spool: None,
                voiced_ms: 0,
            },
            Millis(2100),
        );
        let held = vec![HeldPart {
            text: "Thanks for watching".into(),
            t0: 0,
            t1: 500,
            why: oxi_core::text::HeldWhy::Hallucination,
        }];
        let text = Some(String::new());
        let words = Vec::new();
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words,
                held,
            },
            Millis(2500),
        );
        let last = rig.c.last_text.clone();
        (saved(&rig), last)
    };
    let filed = (
        4,
        "Thanks for watching".to_owned(),
        Some(Delivered::Filtered),
    );
    let (kept, last) = run(true);
    assert_eq!(kept, [filed]);
    assert_eq!(last.as_deref(), Some("Thanks for watching"));
    // Dropped: the no-text path, so neither an entry nor a "Copy last text" (a revert of the
    // drop sends the invention down the filtered path, which keeps it as the last text).
    let (kept, last) = run(false);
    assert_eq!(kept, []);
    assert_eq!(last, None);
}

#[test]
fn history_turned_off_during_a_job_is_off_at_its_save() {
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config.clone());
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    let samples = vec![0.1; 16_000];
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples,
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    config.history.enabled = false;
    rig.c.handle(
        Inbox::Config {
            config: Box::new(config),
            warnings: Vec::new(),
        },
        Millis(2200),
    );
    let text = Some("hello".to_owned());
    rig.c.handle(
        Inbox::Transcribed {
            job,
            text,
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(2500),
    );
    rig.land(job, 2600);
    let saves: Vec<_> = drain(&rig.history)
        .into_iter()
        .filter_map(|c| match c {
            HistoryCmd::Save { cfg, .. } => Some(cfg.enabled),
            HistoryCmd::Config(_) => None,
        })
        .collect();
    assert_eq!(saves, [false]);
}

#[test]
fn a_history_warning_shows_in_the_status_until_it_clears() {
    let mut rig = Rig::new();
    let line = "History not saved (StorageFull)";
    rig.c
        .handle(Inbox::HistoryWarning(Some(line.into())), Millis(10));
    assert_eq!(rig.c.warnings(), [line]);
    rig.c.handle(Inbox::HistoryWarning(None), Millis(20));
    assert_eq!(rig.c.warnings(), Vec::<String>::new());
}

#[test]
fn a_finished_dictation_updates_the_stats_file_a_failed_one_does_not() {
    let dir = scratch_dir(&format!("oxi-coord-stats-{}", std::process::id()));
    let path = dir.join(stats::FILE);
    let today = || stats::Date::from_ymd(2026, 11, 7).unwrap();
    for outcome in [PasteOutcome::Failed, PasteOutcome::Pasted] {
        let mut rig = Rig::new();
        rig.c.stats = Some(Recorder::spawn(path.clone(), today).unwrap());
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        let samples = vec![0.1; 16_000]; // 1 s
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples,
                spool: None,
                voiced_ms: 800,
            },
            Millis(2100),
        );
        let text = Some("Привет, мир".to_owned());
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(2500),
        );
        rig.event(Event::PasteDone { job, outcome }, 3000);
        drop(rig.c.stats.take()); // waits for the write
        assert!(rig.c.spoken.is_empty());
        if outcome == PasteOutcome::Failed {
            assert!(!path.exists(), "a failed paste is not counted");
        }
    }
    let sum = crate::daemon::stats::summary(Some(&path), today());
    assert_eq!(
        (sum.dictations, sum.words, sum.wpm, sum.streak),
        (1, 2, 150, 1),
        "2 words over the 800 ms of voice, not the 1 s of audio"
    );
    let file = oxi_platform::stats_store::read(&path).unwrap();
    assert_eq!(file.pause_pct(), Some(20));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_dictation_without_a_voiced_time_is_stored_without_one() {
    // `audio.trim` off: no gate ran, so no pauses were measured and none may be shown.
    let dir = scratch_dir(&format!("oxi-coord-stats-unknown-{}", std::process::id()));
    let path = dir.join(stats::FILE);
    let today = || stats::Date::from_ymd(2026, 11, 7).unwrap();
    let mut rig = Rig::new();
    rig.c.stats = Some(Recorder::spawn(path.clone(), today).unwrap());
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    let samples = vec![0.1; 16_000]; // 1 s
    let audio = Inbox::JobAudio {
        job,
        samples,
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(2100));
    let text = Some("Привет, мир".to_owned());
    let (words, held) = (Vec::new(), Vec::new());
    let done = Inbox::Transcribed {
        job,
        text,
        words,
        held,
    };
    rig.c.handle(done, Millis(2500));
    let outcome = PasteOutcome::Pasted;
    rig.event(Event::PasteDone { job, outcome }, 3000);
    drop(rig.c.stats.take()); // waits for the write
    let file = oxi_platform::stats_store::read(&path).unwrap();
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(
        (file.days[0].n.speech_ms, file.days[0].n.voiced_ms),
        (1000, 0)
    );
    assert_eq!(file.pause_pct(), None);
}

#[test]
fn a_job_without_audio_leaves_no_spoken_entry() {
    let mut rig = Rig::new();
    let job = JobId(9);
    rig.c.spoken.insert(job, (Millis(500), Millis(0)));
    let answer = rig.c.execute(&Effect::Transcribe(job));
    assert_eq!(answer, Some(Event::TranscriptionFailed(job)));
    assert!(rig.c.spoken.is_empty());
}

#[test]
fn empty_or_failed_transcription_pastes_nothing() {
    for text in [Some("  ".to_owned()), None] {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples: vec![0.0],
                spool: None,
                voiced_ms: 0,
            },
            Millis(2100),
        );
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(2500),
        );
        assert!(drain(&rig.paste).is_empty());
        assert_eq!(rig.phase.load(), Phase::Idle);
    }
}

#[test]
fn job_without_audio_never_reaches_whisper() {
    let cases = [
        (EmptyAudio::NothingHeard, None),
        (EmptyAudio::MicSilence, Some(Notice::MicSilence)),
        (EmptyAudio::Failed, Some(Notice::CaptureFailed)),
    ];
    for (why, notify) in cases {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.drain_all();
        rig.event(Event::JobAudioEmpty { job, why }, 2100);
        assert!(
            !drain(&rig.whisper)
                .iter()
                .any(|c| matches!(c, WhisperCmd::Transcribe { .. })),
            "{why:?}"
        );
        let notices: Vec<_> = drain(&rig.ui)
            .into_iter()
            .filter_map(|c| match c {
                UiCmd::Notify(n) => Some(n),
                _ => None,
            })
            .collect();
        assert_eq!(notices, Vec::from_iter(notify), "{why:?}");
        assert_eq!(rig.phase.load(), Phase::Idle);
    }
}

/// d11 review #3: the capture gets the key-down time, so its latency counts from the press.
#[test]
fn the_start_carries_the_key_down_time() {
    let mut rig = Rig::new();
    let t = std::time::Instant::now();
    rig.hook.pressed.set(Some(t));
    rig.event(Event::ChordDown, 0);
    let start = AudioCmd::Start {
        session: 1,
        pressed: Some(t),
    };
    assert_eq!(drain(&rig.audio), [start]);
}

#[test]
fn stale_capture_event_does_not_touch_the_next_recording() {
    let mut rig = Rig::new();
    rig.record(); // session 1
    rig.event(Event::ChordDown, 3000); // session 2
    assert_eq!(
        drain(&rig.audio),
        [AudioCmd::Start {
            session: 2,
            pressed: None
        }]
    );

    let lost = |session| Inbox::Capture {
        session,
        event: Event::DeviceLost,
    };
    rig.c.handle(lost(1), Millis(3500));
    assert!(drain(&rig.audio).is_empty(), "the old capture's loss");
    assert_eq!(rig.phase.load(), Phase::Recording);

    rig.c.handle(lost(2), Millis(3600));
    assert!(
        drain(&rig.audio)
            .iter()
            .any(|c| matches!(c, AudioCmd::Stop(_)))
    );
}

#[test]
fn esc_discards_and_never_pastes() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::Esc, 500);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 2000);

    let audio = drain(&rig.audio);
    assert!(audio.contains(&AudioCmd::Discard));
    assert!(!audio.iter().any(|c| matches!(c, AudioCmd::Stop(_))));
    assert!(
        !drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::Transcribe { .. }))
    );
    assert!(drain(&rig.paste).is_empty());
    assert_eq!(rig.phase.load(), Phase::Idle);
}

#[test]
fn esc_after_speech_keeps_the_audio_in_the_history_and_never_transcribes() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    // Capture session 1 heard speech; a stale session's speech is ignored.
    rig.c.handle(
        Inbox::Capture {
            session: 1,
            event: Event::SpeechHeard,
        },
        Millis(100),
    );
    rig.event(Event::Esc, 500);
    let job = drain(&rig.audio)
        .into_iter()
        .find_map(|c| match c {
            AudioCmd::Stop(job) => Some(job),
            _ => None,
        })
        .expect("capture stopped, not discarded");
    drain(&rig.history);
    let samples = vec![0.1; 8_000];
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples,
            spool: None,
            voiced_ms: 0,
        },
        Millis(600),
    );

    let saved = drain(&rig.history);
    assert!(
        matches!(saved.as_slice(), [HistoryCmd::Save { samples, text, .. }]
                if samples.len() == 8_000 && text.is_empty()),
        "{} history commands",
        saved.len()
    );
    assert!(
        !drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::Transcribe { .. }))
    );
    assert!(rig.c.audio.is_empty());
    assert_eq!(rig.phase.load(), Phase::Idle);
}

#[test]
fn config_received_while_recording_applies_when_idle() {
    let mut rig = Rig::new();
    rig.event(Event::ChordDown, 0);
    let mut config = Config::default();
    config.audio.device = "Mic X".into();
    config.audio.meter_sensitivity_db = -6.0;
    config.ui.accent = oxi_core::config::Accent::Hex(oxi_core::config::Rgb(0x5B, 0x5B, 0xD6));
    config.model.gpu = oxi_core::device::DeviceChoice::Named("Vulkan1".into());
    rig.c.handle(
        Inbox::Config {
            config: Box::new(config.clone()),
            warnings: vec!["gain clamped".into()],
        },
        Millis(100),
    );
    let configs = |audio: Vec<AudioCmd>| {
        audio
            .into_iter()
            .filter(|c| matches!(c, AudioCmd::Config(_)))
            .collect::<Vec<_>>()
    };
    assert!(configs(drain(&rig.audio)).is_empty(), "not while recording");
    let hotkeys_before = rig.hook.hotkeys.borrow().len();

    rig.event(Event::Esc, 500);
    assert_eq!(
        configs(drain(&rig.audio)),
        [AudioCmd::Config(config.audio.clone())]
    );
    assert_eq!(rig.hook.hotkeys.borrow().len(), hotkeys_before + 1);
    let ui = drain(&rig.ui);
    assert!(ui.contains(&UiCmd::Config {
        overlay: config.overlay.clone(),
        meter_sensitivity_db: config.audio.meter_sensitivity_db,
        accent: config.ui.accent,
    }));
    assert!(ui.contains(&UiCmd::Warnings(vec!["gain clamped".into()])));

    let (tx, rx) = channel();
    rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(600));
    let status = rx.try_recv().expect("status answered");
    assert_eq!(status.device, "Mic X");
    assert_eq!(
        status.gpu,
        oxi_core::device::DeviceChoice::Named("Vulkan1".into())
    );
    assert_eq!(status.warnings, ["gain clamped"]);
}

#[test]
fn sounds_go_to_the_sound_worker_and_its_warnings_show_in_status() {
    let mut rig = Rig::new();
    rig.event(Event::ChordDown, 0);
    assert_eq!(drain(&rig.sound), [SoundCmd::Play(Sound::Start)]);
    rig.c.handle(
        Inbox::Ipc(IpcAsk::PreviewSound {
            choice: "pop".into(),
            volume_percent: 30,
        }),
        Millis(10),
    );
    assert_eq!(
        drain(&rig.sound),
        [SoundCmd::Preview {
            choice: "pop".into(),
            volume_percent: 30
        }]
    );
    let status = |rig: &mut Rig| {
        let (tx, rx) = channel();
        rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(20));
        let report = rx.try_recv().expect("status answered");
        (report.warnings, report.sounds)
    };
    let warning = SoundWarning {
        kind: oxi_core::ipc::SoundWarningKind::Unused(Sound::Start),
        line: "sounds.start: cannot read the file (entity not found), using rise".into(),
    };
    rig.c
        .handle(Inbox::SoundWarnings(vec![warning.clone()]), Millis(20));
    let line = warning.line.clone();
    assert_eq!(status(&mut rig), (vec![line], vec![warning]));
    rig.c.handle(Inbox::SoundWarnings(Vec::new()), Millis(30));
    assert_eq!(status(&mut rig), (Vec::new(), Vec::new()));
}

#[test]
fn engine_warning_shows_in_status_until_cleared() {
    let mut rig = Rig::new();
    let status = |rig: &mut Rig| {
        let (tx, rx) = channel();
        rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(0));
        rx.try_recv().expect("status answered").warnings
    };
    rig.c
        .handle(Inbox::ConfigError(ConfigError::ReadOnly), Millis(0));
    let error = ConfigError::ReadOnly.to_string();
    assert_eq!(status(&mut rig), std::slice::from_ref(&error));
    let oom = EngineWarning::CpuFallback {
        reason: "out of memory".into(),
    };
    rig.c.handle(Inbox::EngineWarning(Some(oom)), Millis(0));
    assert_eq!(
        status(&mut rig),
        ["running on CPU: out of memory".to_owned(), error.clone()]
    );
    rig.c.handle(Inbox::EngineWarning(None), Millis(0));
    assert_eq!(status(&mut rig), std::slice::from_ref(&error));
    // The tray heard each change once, the last one being the current list.
    let tray: Vec<_> = drain(&rig.ui)
        .into_iter()
        .filter_map(|c| match c {
            UiCmd::Warnings(w) => Some(w),
            _ => None,
        })
        .collect();
    assert_eq!(
        tray,
        [
            vec![error.clone()],
            vec!["running on CPU: out of memory".to_owned(), error.clone()],
            vec![error],
        ]
    );
}

#[test]
fn errors_raise_the_tray_badge_until_a_dictation_lands_or_the_config_is_fixed() {
    let badges = |rig: &Rig| -> Vec<TrayBadge> {
        drain(&rig.ui)
            .into_iter()
            .filter_map(|c| match c {
                UiCmd::Badge(b) => Some(b),
                _ => None,
            })
            .collect()
    };
    let mut rig = Rig::new();
    let job = rig.transcribed("one");
    rig.event(
        Event::PasteDone {
            job,
            outcome: PasteOutcome::Failed,
        },
        3000,
    );
    assert_eq!(badges(&rig), [TrayBadge::Error], "a failed paste");
    let job = rig.transcribed("two");
    rig.event(
        Event::PasteDone {
            job,
            outcome: PasteOutcome::Pasted,
        },
        4000,
    );
    assert_eq!(badges(&rig), [TrayBadge::None], "the next one landed");

    let job = rig.record();
    rig.event(
        Event::JobAudioEmpty {
            job,
            why: EmptyAudio::MicSilence,
        },
        2100,
    );
    assert!(badges(&rig).is_empty(), "a hint is no error");

    rig.c
        .handle(Inbox::ConfigError(ConfigError::ReadOnly), Millis(5000));
    assert_eq!(badges(&rig), [TrayBadge::Error], "an invalid config");
    rig.c.handle(
        Inbox::Config {
            config: Box::new(Config::default()),
            warnings: Vec::new(),
        },
        Millis(5100),
    );
    assert_eq!(badges(&rig), [TrayBadge::None], "the config was fixed");
}

#[test]
fn a_failure_badge_clears_when_its_cause_is_gone() {
    let badges = |rig: &Rig| -> Vec<TrayBadge> {
        drain(&rig.ui)
            .into_iter()
            .filter_map(|c| match c {
                UiCmd::Badge(b) => Some(b),
                _ => None,
            })
            .collect()
    };
    let audio = |rig: &mut Rig, job, at| {
        let samples = vec![0.5; 4];
        rig.c.handle(
            Inbox::JobAudio {
                job,
                samples,
                spool: None,
                voiced_ms: 0,
            },
            Millis(at),
        );
    };
    let mut rig = Rig::new();

    // No model: the load that follows clears it.
    rig.event(Event::ModelFailed(ModelFailure::Missing), 0);
    assert_eq!(badges(&rig), [TrayBadge::Error], "no model");
    rig.event(Event::ModelLoaded, 100);
    assert_eq!(badges(&rig), [TrayBadge::None], "a model loaded");

    // Lost mic: its own tail does not clear it, sound from the next recording does.
    rig.event(Event::ChordDown, 200);
    drain(&rig.audio);
    let lost = Inbox::Capture {
        session: 1,
        event: Event::DeviceLost,
    };
    rig.c.handle(lost, Millis(1200));
    assert_eq!(badges(&rig), [TrayBadge::Error], "the mic is lost");
    rig.event(Event::ChordUp, 1300);
    let job = drain(&rig.audio)
        .into_iter()
        .find_map(|c| match c {
            AudioCmd::Stop(job) => Some(job),
            _ => None,
        })
        .expect("the lost recording stops");
    audio(&mut rig, job, 400);
    assert!(badges(&rig).is_empty(), "the tail of the lost recording");
    // Jobs run in order: answer each with silence so the next one can be asked.
    let silence = |rig: &mut Rig, job, at| {
        let text = Some(String::new());
        rig.c.handle(
            Inbox::Transcribed {
                job,
                text,
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(at),
        );
    };
    silence(&mut rig, job, 450);
    let job = rig.record();
    audio(&mut rig, job, 2100);
    silence(&mut rig, job, 2150);
    assert_eq!(
        badges(&rig),
        [TrayBadge::None],
        "the next recording had sound"
    );

    // Failed transcription: the next one that works clears it, before any paste.
    let job = rig.record();
    audio(&mut rig, job, 2100);
    let failed = Inbox::Transcribed {
        job,
        text: None,
        words: Vec::new(),
        held: Vec::new(),
    };
    rig.c.handle(failed, Millis(2200));
    assert_eq!(badges(&rig), [TrayBadge::Error], "transcription failed");
    let job = rig.record();
    audio(&mut rig, job, 2100);
    assert!(badges(&rig).is_empty(), "sound alone does not fix it");
    let text = Some("three".to_owned());
    rig.c.handle(
        Inbox::Transcribed {
            job,
            text,
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(2300),
    );
    assert_eq!(badges(&rig), [TrayBadge::None], "a transcription worked");
}

/// History turned on while busy: the worker streams already (it gets the newest `[history]`)
/// but the active config, applied at Idle, still says off. The transcribed job must be saved,
/// not left waiting until quit and kept as failed.
#[test]
fn a_job_that_streamed_a_recording_is_saved_whatever_the_active_setting_says() {
    let mut config = Config::default();
    config.history.enabled = false;
    let mut rig = Rig::with(config.clone());
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    config.history.enabled = true;
    let (config, warnings) = (Box::new(config), Vec::new());
    rig.c
        .handle(Inbox::Config { config, warnings }, Millis(1500));
    let spool = PathBuf::from("h/2026-11-10/120000-000.wav.part");
    let (samples, text) = (vec![0.5; 4], Some("hello".to_owned()));
    let audio = Inbox::JobAudio {
        job,
        samples,
        spool: Some(spool.clone()),
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(2100));
    rig.c.handle(
        Inbox::Transcribed {
            job,
            text,
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(2500),
    );
    rig.land(job, 2600);
    let saves: Vec<_> = drain(&rig.history)
        .into_iter()
        .filter_map(|cmd| match cmd {
            HistoryCmd::Save { spool, cfg, .. } => Some((spool, cfg.enabled)),
            HistoryCmd::Config(_) => None,
        })
        .collect();
    assert_eq!(saves, [(Some(spool), true)]);
    assert!(rig.c.spools.is_empty() && rig.c.history_audio.is_empty());
}

/// t13: deleting the configured model fails it as `Missing`; downloading it again must let
/// dictation start without a config change.
#[test]
fn a_download_of_the_deleted_model_lets_dictation_start_again() {
    let mut config = Config::default();
    config.model.id = "t10b-downloaded-again".into(); // no other test downloads this id
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ModelFailed(ModelFailure::Missing), 10);
    let starts = |rig: &Rig| {
        drain(&rig.audio)
            .iter()
            .filter(|c| matches!(c, AudioCmd::Start { .. }))
            .count()
    };
    rig.event(Event::ChordDown, 100);
    rig.event(Event::ChordUp, 150);
    assert_eq!(starts(&rig), 0, "refused while the file is gone");
    assert_eq!(rig.c.badge, TrayBadge::Error);
    rig.c
        .handle(Inbox::ModelDownloaded("another-model".into()), Millis(200));
    assert!(
        drain(&rig.whisper)
            .iter()
            .any(|c| matches!(c, WhisperCmd::Config(_))),
        "any download clears whisper's failed-model list"
    );
    assert_eq!(rig.c.status().model_failure, Some(ModelFailure::Missing));
    rig.c.handle(
        Inbox::ModelDownloaded("t10b-downloaded-again".into()),
        Millis(300),
    );
    assert_eq!(rig.c.badge, TrayBadge::None, "the cause is gone");
    rig.event(Event::ChordDown, 5000);
    assert_eq!(starts(&rig), 1, "the downloaded model is tried again");
    assert_eq!(rig.c.status().model_failure, None);
}

#[test]
fn status_backend_is_the_device_the_model_loaded_on() {
    let mut rig = Rig::new();
    let backend = |rig: &mut Rig| {
        let (tx, rx) = channel();
        rig.c.handle(Inbox::Ipc(IpcAsk::Status(tx)), Millis(0));
        rx.try_recv().expect("status answered").backend
    };
    assert_eq!(backend(&mut rig), "none");
    rig.c.handle(Inbox::Backend("Vulkan0".into()), Millis(0));
    assert_eq!(backend(&mut rig), "Vulkan0");
    rig.c.handle(Inbox::Backend("cpu".into()), Millis(0));
    assert_eq!(backend(&mut rig), "cpu");
}

#[test]
fn poll_finding_the_chord_released_stops_the_recording() {
    let mut rig = Rig::new();
    rig.hook.held.set(false);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::Tick, 1000); // watchdog poll due; the key-up was never delivered
    rig.event(Event::Tick, 2000); // past the release tail
    assert!(rig.hook.polls.get() >= 1);
    assert!(
        drain(&rig.audio)
            .iter()
            .any(|c| matches!(c, AudioCmd::Stop(_)))
    );
}

#[test]
fn poll_answer_overlay_is_not_overridden_by_its_own_step() {
    let mut config = Config::default();
    config.audio.tail_ms = 0;
    config.audio.max_seconds = 31; // `ending_soon` from 1 s, the tick that also polls
    let mut rig = Rig::with(config);
    rig.hook.held.set(false);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.drain_all();
    rig.event(Event::Tick, 1000);
    // A held release waits out a bounce even with no tail: the stop comes at its end.
    rig.event(Event::Tick, 1000 + BOUNCE.0);
    let overlays: Vec<_> = drain(&rig.ui)
        .into_iter()
        .filter(|c| matches!(c, UiCmd::Overlay(_)))
        .collect();
    assert_eq!(
        overlays.last(),
        Some(&UiCmd::Overlay(OverlayState::Transcribing)),
        "{overlays:?}"
    );
}

/// Commit 69a531d: the answer of a poll waits until every effect of its step ran. The tick that
/// warns also polls; the poll finds the chord up, which locks the second press of a double tap.
/// Run inline, the answer's `locked` overlay would be overridden by the tick's own, later one.
#[test]
fn a_poll_that_locks_the_recording_keeps_its_overlay_through_the_warning_step() {
    let mut config = Config::default();
    config.audio.tail_ms = 0;
    config.audio.max_seconds = 30; // the warning is due from the start: the tick that polls warns
    let mut rig = Rig::with(config);
    rig.hook.held.set(false); // the second key-up never comes: only the poll sees it
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 100);
    rig.event(Event::Tick, 140); // the tap ends
    rig.event(Event::ChordDown, 250); // inside the double-tap gap
    rig.drain_all();
    rig.event(Event::Tick, 350); // warns and polls in one step
    let overlays: Vec<_> = drain(&rig.ui)
        .into_iter()
        .filter(|c| matches!(c, UiCmd::Overlay(_)))
        .collect();
    assert_eq!(
        overlays.last(),
        Some(&UiCmd::Overlay(OverlayState::Recording {
            locked: true,
            ending_soon: true
        })),
        "{overlays:?}"
    );
}

#[test]
fn run_handles_messages_until_shutdown() {
    let rig = Rig::new();
    let (tx, rx) = channel();
    for msg in [Inbox::Event(Event::ChordDown), Inbox::Shutdown] {
        tx.send(msg).expect("open channel");
    }
    rig.c.run(&rx, || Millis(0));
    // Quit stops the recording; nothing was heard, so nothing is kept.
    assert_eq!(
        drain(&rig.audio),
        [
            AudioCmd::Start {
                session: 1,
                pressed: None
            },
            AudioCmd::Discard
        ]
    );
}

/// What a quit left on the worker channels.
struct Quit {
    phase: PhaseCell,
    audio: Vec<AudioCmd>,
    whisper: Vec<WhisperCmd>,
    paste: Vec<PasteCmd>,
    /// The text of each `HistoryCmd::Save`.
    saved: Vec<String>,
    took: Duration,
}

/// Quits `rig` with `msgs` already queued for the grace period.
fn quit(rig: Rig, msgs: Vec<Inbox>, grace: u64) -> Quit {
    let (tx, rx) = channel();
    for msg in msgs {
        tx.send(msg).expect("open channel");
    }
    let start = std::time::Instant::now();
    let clock = || Millis(u64::try_from(start.elapsed().as_millis()).unwrap());
    rig.c.shutdown(&rx, &clock, Millis(grace));
    let saved = drain(&rig.history)
        .into_iter()
        .filter_map(|c| match c {
            HistoryCmd::Save { text, .. } => Some(text),
            HistoryCmd::Config(_) => None,
        })
        .collect();
    Quit {
        phase: rig.phase,
        audio: drain(&rig.audio),
        whisper: drain(&rig.whisper),
        paste: drain(&rig.paste),
        saved,
        took: start.elapsed(),
    }
}

// t15: a quit during transcription lost the text. It now waits for it: the text goes to
// the history and the clipboard, never pasted into whatever has the focus at quit.
#[test]
fn quit_during_transcription_keeps_the_text_on_the_clipboard_and_in_the_history() {
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    rig.drain_all();
    let msgs = vec![
        Inbox::Event(Event::ChordDown), // a press while quitting starts nothing
        Inbox::Transcribed {
            job,
            text: Some("hello".into()),
            words: Vec::new(),
            held: Vec::new(),
        },
    ];
    let q = quit(rig, msgs, 60_000);
    assert!(
        q.took < Duration::from_secs(10),
        "waited for the grace: {:?}",
        q.took
    );
    let pastes: Vec<_> = q
        .paste
        .into_iter()
        .map(|c| match c {
            PasteCmd::CopyOnly { text, .. } => text,
            PasteCmd::Paste { .. } => panic!("pasted at quit"),
            PasteCmd::File { .. } => panic!("no to-file mode"),
        })
        .collect();
    assert_eq!(pastes, ["hello"]);
    assert_eq!(q.saved, ["hello"]);
    assert!(!q.audio.iter().any(|c| matches!(c, AudioCmd::Start { .. })));
}

// t15: a transcription that does not end in the grace is cut: whisper's cancel check sees
// Idle, and the quit does not wait for it.
#[test]
fn quit_cuts_a_transcription_that_outlives_the_grace() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    assert_eq!(rig.phase.load(), Phase::Transcribing);
    let q = quit(rig, Vec::new(), 50);
    assert!(q.took >= Duration::from_millis(50), "{:?}", q.took);
    assert_eq!(q.phase.load(), Phase::Idle, "whisper is told to stop");
    assert!(q.paste.is_empty());
}

// Review d03-fix #1: a paste (typing ~400 chars, Win+V work) still running when the grace
// ends never answers PasteDone; its entry was only filed there, so the text was lost.
#[test]
fn a_paste_that_outlives_the_quit_grace_still_files_its_entry() {
    use oxi_core::history::Delivered;
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    rig.transcribed("typed at quit");
    assert_eq!(rig.phase.load(), Phase::Pasting);
    assert!(saved(&rig).is_empty(), "filed at PasteDone, not before");
    // A fake clock: every look is one grace later, so the grace is over at once.
    let at = Cell::new(3000);
    let clock = || {
        at.set(at.get() + SHUTDOWN_GRACE.0);
        Millis(at.get())
    };
    let (_tx, rx) = channel();
    let Rig { c, history, .. } = rig;
    c.shutdown(&rx, &clock, SHUTDOWN_GRACE);
    let sent: Vec<_> = drain(&history)
        .into_iter()
        .filter_map(|c| match c {
            HistoryCmd::Save {
                text, delivered, ..
            } => Some((text, delivered)),
            HistoryCmd::Config(_) => None,
        })
        .collect();
    assert_eq!(
        sent,
        [("typed at quit".to_owned(), Some(Delivered::Unknown))]
    );
}

// t15: a quit while recording after speech keeps the audio, as a session lock does.
#[test]
fn quit_while_recording_keeps_the_heard_audio() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.c.handle(
        Inbox::Capture {
            session: 1,
            event: Event::SpeechHeard,
        },
        Millis(100),
    );
    rig.drain_all();
    // The capture's answer to the quit's Stop, for the first job.
    let msgs = vec![Inbox::JobAudio {
        job: JobId(0),
        samples: vec![0.1; 800],
        spool: None,
        voiced_ms: 0,
    }];
    let q = quit(rig, msgs, 60_000);
    assert!(q.audio.contains(&AudioCmd::Stop(JobId(0))));
    assert_eq!(q.saved, [""], "kept with no text");
    assert!(
        !q.whisper
            .iter()
            .any(|c| matches!(c, WhisperCmd::Transcribe { .. }))
    );
}

#[test]
fn wait_until_the_deadline() {
    let cases = [
        (None, 500, None),
        (Some(800), 500, Some(300)),
        (Some(500), 500, Some(0)),
        (Some(200), 500, Some(0)),
    ];
    for (deadline, now, want) in cases {
        assert_eq!(
            wait(deadline.map(Millis), Millis(now)),
            want.map(Duration::from_millis),
            "deadline {deadline:?} at {now}"
        );
    }
}

/// The log lines of what `run` does, without a global subscriber: the machine's reasons
/// reach the log through the coordinator, so the text is what the test reads.
fn logged(run: impl FnOnce()) -> String {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let sink = Sink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .finish();
    tracing::subscriber::with_default(subscriber, run);
    let bytes = sink.0.lock().unwrap().clone();
    String::from_utf8(bytes).unwrap()
}

/// arch-plan 4.3: the log says why a recording was cancelled and whether its audio was kept,
/// with the machine's own reason; no text.
#[test]
fn a_cancel_is_logged_with_the_machines_reason_and_speech() {
    let cases = [
        (Event::Esc, false, "why=esc speech=false"),
        (Event::Esc, true, "why=esc speech=true"),
        (Event::Lock, false, "why=lock speech=false"),
        (Event::Suspend, true, "why=suspend speech=true"),
    ];
    for (event, speech, want) in cases {
        let mut rig = Rig::new();
        rig.event(Event::ModelLoaded, 0);
        rig.event(Event::ChordDown, 0);
        if speech {
            rig.event(Event::SpeechHeard, 100);
        }
        let log = logged(|| rig.event(event.clone(), 200));
        let line = log.lines().find(|l| l.contains("recording cancelled"));
        assert!(
            line.is_some_and(|l| l.contains(want)),
            "{event:?} speech {speech}: {log}"
        );
    }
}

#[test]
fn a_shortcut_and_another_key_are_logged_as_such() {
    let mut config = Config::default();
    config.hotkey.other_key = oxi_core::config::OtherKey::Auto;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::OtherKey, 50);
    let log = logged(|| rig.event(Event::Tick, 5000));
    assert!(log.contains("why=shortcut speech=false"), "{log}");

    let mut config = Config::default();
    config.hotkey.other_key = oxi_core::config::OtherKey::Always;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    let log = logged(|| rig.event(Event::OtherKey, 50));
    assert!(log.contains("why=other_key speech=false"), "{log}");
}

#[test]
fn a_refused_key_down_is_logged_with_its_reason() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    for t in [0, 10_000] {
        rig.event(Event::ChordDown, t);
        rig.event(Event::ChordUp, t + 1000);
        rig.event(Event::Tick, t + 2000); // a job waiting for its audio
    }
    let log = logged(|| rig.event(Event::ChordDown, 30_000));
    let line = log.lines().find(|l| l.contains("recording refused"));
    assert!(line.is_some_and(|l| l.contains("why=queue_full")), "{log}");
    assert!(!log.contains("recording cancelled"), "{log}");
}

/// Makes `dir` the folder new recordings stream into; off again when dropped.
struct SpoolDir;
impl SpoolDir {
    fn on(dir: PathBuf) -> Self {
        oxi_platform::wav::set_spool_dir(Some(dir));
        Self
    }
}
impl Drop for SpoolDir {
    fn drop(&mut self) {
        oxi_platform::wav::set_spool_dir(None);
    }
}

/// Records a first part (job 0, streamed to `part`) and a press inside the gap that carries
/// it on into job 1, whose audio has arrived; `config` has saving on.
fn carry_on(rig: &mut Rig, part: &Path) {
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1100);
    rig.c
        .handle(spooled_audio(JobId(0), Some(part.to_owned())), Millis(1150));
    rig.c
        .handle(transcribed(JobId(0), Some("one")), Millis(1200));
    rig.event(Event::ChordDown, 1300); // inside the gap: the dictation goes on
    rig.event(Event::ChordUp, 2000);
    rig.event(Event::Tick, 2100);
    rig.c.handle(spooled_audio(JobId(1), None), Millis(2150));
}

fn spooled_audio(job: JobId, spool: Option<PathBuf>) -> Inbox {
    Inbox::JobAudio {
        job,
        samples: vec![0.1; 4000],
        spool,
        voiced_ms: 0,
    }
}

fn transcribed(job: JobId, text: Option<&str>) -> Inbox {
    Inbox::Transcribed {
        job,
        text: text.map(str::to_owned),
        words: Vec::new(),
        held: Vec::new(),
    }
}

/// A finished first part streamed into a spool folder that is on for this test only.
fn first_part(name: &str) -> (SpoolDir, PathBuf) {
    // Holds the history tests' lock: the spool folder is process-wide.
    let on = SpoolDir::on(super::super::history::tests::temp(name));
    let mut spool = oxi_platform::wav::WavSpool::begin(48_000).unwrap();
    spool.push(&vec![0.1; 480]);
    (on, spool.finish())
}

/// The first part's recording on disk is all there is of it until the merged audio is
/// written: the save carries it to the history worker, which deletes it after the write.
#[test]
fn a_carried_parts_recording_goes_with_the_merged_save_and_stays_on_disk() {
    let (_on, part) = first_part("coord-carry");
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    carry_on(&mut rig, &part);
    assert_eq!(
        rig.c.carried_spools.get(&JobId(1)),
        Some(&vec![part.clone()])
    );

    drain(&rig.history);
    rig.c
        .handle(transcribed(JobId(1), Some("one two")), Millis(2200));
    rig.event(Event::Tick, 2600); // the gap ends: pasted
    rig.land(JobId(1), 2700); // and filed
    let carried = drain(&rig.history).into_iter().find_map(|c| match c {
        HistoryCmd::Save { text, carried, .. } if text == "one two" => Some(carried),
        _ => None,
    });
    assert_eq!(
        carried,
        Some(vec![part.clone()]),
        "the merged save carries it"
    );
    assert!(rig.c.carried_spools.is_empty());
    // Still waiting (a quit before the write keeps it as failed) and on disk once closed:
    // only the history worker deletes it, after the write. Taken here in its place.
    assert!(oxi_platform::wav::spool_take(&part));
    assert!(part.is_file());
}

/// The merged dictation's transcription fails: the first part's recording is kept as a
/// failed entry, not deleted with the merge.
#[test]
fn a_carried_parts_recording_is_kept_as_failed_when_the_merged_text_never_comes() {
    let (_on, part) = first_part("coord-carry-fail");
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    carry_on(&mut rig, &part);
    drain(&rig.history);
    rig.c.handle(transcribed(JobId(1), None), Millis(2200));
    let orphans = oxi_platform::wav::spool_take_orphans();
    assert_eq!(orphans, vec![part.clone()], "given up: filed as failed");
    assert!(part.is_file(), "the first part's audio must stay");
    let filed = drain(&rig.history)
        .iter()
        .any(|c| matches!(c, HistoryCmd::Config(_)));
    assert!(filed, "the history worker gets the pass that files it");
    assert!(rig.c.carried_spools.is_empty());
}

/// Saving is turned off after the first part was streamed: nothing may be saved, so the
/// carried part goes with the save the worker discards, not to the pass that files orphans.
#[test]
fn a_carried_part_is_deleted_not_filed_when_saving_is_turned_off() {
    let (_on, part) = first_part("coord-carry-off");
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config.clone());
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1100);
    rig.c
        .handle(spooled_audio(JobId(0), Some(part.clone())), Millis(1150));
    rig.c
        .handle(transcribed(JobId(0), Some("one")), Millis(1200));
    rig.event(Event::ChordDown, 1300); // inside the gap: the dictation goes on
    config.history.enabled = false;
    rig.c.handle(
        Inbox::Config {
            config: Box::new(config),
            warnings: Vec::new(),
        },
        Millis(1400),
    );
    rig.event(Event::ChordUp, 2000);
    rig.event(Event::Tick, 2100);
    drain(&rig.history);
    rig.c.handle(spooled_audio(JobId(1), None), Millis(2150));
    rig.c
        .handle(transcribed(JobId(1), Some("two")), Millis(2200));
    rig.event(Event::Tick, 2600);
    rig.land(JobId(1), 2700);
    let carried = drain(&rig.history).into_iter().find_map(|c| match c {
        HistoryCmd::Save { carried, cfg, .. } if !cfg.enabled => Some(carried),
        _ => None,
    });
    assert_eq!(carried, Some(vec![part]), "the worker deletes it");
    assert!(rig.c.carried_spools.is_empty());
}

/// Turns saving off while job 1, which carries the first part, is still in whisper.
fn saving_off(rig: &mut Rig, at: u64) {
    let mut config = Config::default();
    config.history.enabled = false;
    rig.c.handle(
        Inbox::Config {
            config: Box::new(config),
            warnings: Vec::new(),
        },
        Millis(at),
    );
}

/// The save `sent` holds that deletes `part`: one with saving off.
fn deleting_save(sent: Vec<HistoryCmd>) -> Option<Vec<PathBuf>> {
    sent.into_iter().find_map(|c| match c {
        HistoryCmd::Save { carried, cfg, .. } if !cfg.enabled => Some(carried),
        _ => None,
    })
}

/// Review c06 #1: saving turned off, then the merged transcription fails. The first part
/// was given up as an orphan, which the pass with saving off filed as failed.
#[test]
fn a_carried_part_is_deleted_when_saving_is_off_and_the_merged_text_never_comes() {
    let (_on, part) = first_part("coord-carry-off-fail");
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    carry_on(&mut rig, &part);
    saving_off(&mut rig, 2160);
    drain(&rig.history);
    rig.c.handle(transcribed(JobId(1), None), Millis(2200));
    let orphans = oxi_platform::wav::spool_take_orphans();
    assert!(orphans.is_empty(), "not to be filed: {orphans:?}");
    assert_eq!(deleting_save(drain(&rig.history)), Some(vec![part.clone()]));
    assert!(rig.c.carried_spools.is_empty());
    // Still waiting for the worker that deletes it; taken here in its place.
    assert!(oxi_platform::wav::spool_take(&part));
}

/// Review c06 #2: saving turned off, then a quit cuts the merged transcription short. The
/// first part stayed waiting and the worker's last pass filed it as interrupted.
#[test]
fn a_carried_part_is_deleted_when_saving_is_off_and_a_quit_cuts_the_merge() {
    let (_on, part) = first_part("coord-carry-off-quit");
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    carry_on(&mut rig, &part);
    saving_off(&mut rig, 2160);
    drain(&rig.history);
    let (_tx, rx) = channel();
    rig.c.shutdown(&rx, &|| Millis(0), Millis(0));
    assert_eq!(deleting_save(drain(&rig.history)), Some(vec![part.clone()]));
    let orphans = oxi_platform::wav::spool_take_orphans();
    assert!(orphans.is_empty(), "not to be filed: {orphans:?}");
    assert!(oxi_platform::wav::spool_take(&part));
}

/// The continuation had nothing after the trim: the earlier parts still go on as the
/// dictation, and the history worker still gets the pass that files the kept recording.
#[test]
fn a_continuation_with_nothing_heard_still_files_its_kept_recording() {
    let mut config = Config::default();
    config.history.enabled = true;
    let mut rig = Rig::with(config);
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1100);
    rig.c.handle(
        Inbox::JobAudio {
            job: JobId(0),
            samples: vec![0.1; 8000],
            spool: None,
            voiced_ms: 0,
        },
        Millis(1150),
    );
    rig.c.handle(
        Inbox::Transcribed {
            job: JobId(0),
            text: Some("one".to_owned()),
            words: Vec::new(),
            held: Vec::new(),
        },
        Millis(1200),
    );
    rig.event(Event::ChordDown, 1300);
    rig.event(Event::ChordUp, 2000);
    rig.event(Event::Tick, 2100);
    drain(&rig.history);
    let why = EmptyAudio::NothingHeard;
    rig.event(Event::JobAudioEmpty { job: JobId(1), why }, 2150);
    let sent = drain(&rig.history);
    assert!(
        sent.iter().any(|c| matches!(c, HistoryCmd::Config(_))),
        "the kept part is filed on the merge path too"
    );
    let sent: Vec<_> = drain(&rig.whisper)
        .into_iter()
        .filter_map(|c| match c {
            WhisperCmd::Transcribe {
                job: JobId(1),
                samples,
                ..
            } => Some(samples.len()),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [8000], "the earlier parts go on as the dictation");
}
fn profile_rig(to_file: FileMode, folder: &str) -> Rig {
    use oxi_core::config::PasteRule;
    let mut config = Config::default();
    config.history.enabled = true;
    config.output.file_folder = folder.into();
    config.paste.rules = vec![PasteRule {
        app: "notepad.exe".into(),
        language: Some(Language::EN),
        model: Some("tiny".into()),
        to_file: Some(to_file),
        dictionary: Some(false),
        ..PasteRule::default()
    }];
    Rig::with(config)
}

/// The last overlay state sent to the UI since the last look.
fn last_overlay(rig: &Rig) -> Option<OverlayState> {
    drain(&rig.ui)
        .into_iter()
        .filter_map(|c| match c {
            UiCmd::Overlay(s) => Some(s),
            _ => None,
        })
        .next_back()
}

/// d03-label: the pill tells a dictation the filter held back, and the file it went to.
#[test]
fn the_pill_says_held_back_and_saved_to_file() {
    use oxi_core::history::HeldPart;
    use oxi_core::text::HeldWhy;
    let part = |why| HeldPart {
        text: "x".into(),
        t0: 0,
        t1: 100,
        why,
    };
    // The dictation ends in the pill: what it shows once the text came (or did not).
    let end = |mut rig: Rig, text: &str, held: Vec<HeldPart>| {
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        let audio = Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio, Millis(2100));
        let done = Inbox::Transcribed {
            job,
            text: Some(text.into()),
            words: Vec::new(),
            held,
        };
        rig.c.handle(done, Millis(2500));
        if text.is_empty() {
            return last_overlay(&rig);
        }
        drain(&rig.ui);
        rig.land(job, 3000);
        rig.event(Event::Tick, 6000);
        last_overlay(&rig)
    };
    let low = || vec![part(HeldWhy::LowConfidence)];
    let nothing = Some(OverlayState::Text(Notice::NothingHeard));
    let plain = || profile_rig(FileMode::Off, "");
    let all = Some(OverlayState::HeldBack { all: true });
    assert_eq!(end(plain(), "", low()), all, "all held: its own label");
    // An invention kept for the history was never heard: still "nothing heard".
    assert_eq!(
        end(plain(), "", vec![part(HeldWhy::Hallucination)]),
        nothing
    );
    assert_eq!(end(plain(), "", Vec::new()), nothing);
    let some = Some(OverlayState::HeldBack { all: false });
    assert_eq!(end(plain(), "kept", low()), some, "some held: a cue");
    assert_eq!(end(plain(), "kept", Vec::new()), Some(OverlayState::Hidden));
    let file = "C:/synthetic/notes";
    let saved = |also| Some(OverlayState::SavedToFile { also });
    assert_eq!(
        end(profile_rig(FileMode::Only, file), "hi", Vec::new()),
        saved(false)
    );
    // Next to a paste the file is told once its write answers: see the test below.
    assert_eq!(
        end(profile_rig(FileMode::Also, file), "hi", Vec::new()),
        Some(OverlayState::Hidden)
    );
    // Held text is the news when the file only repeats the paste.
    assert_eq!(end(profile_rig(FileMode::Also, file), "hi", low()), some);
    assert_eq!(
        end(profile_rig(FileMode::Only, file), "hi", low()),
        saved(false)
    );
}

/// d03-label review #1: "Also saved" only for a write that worked, never for one that failed
/// or before it answered.
#[test]
fn also_saved_waits_for_the_write_and_a_failed_write_says_nothing() {
    let saved = Some(OverlayState::SavedToFile { also: true });
    for (answer, want) in [(None, saved), (Some("failed".to_owned()), None)] {
        let mut rig = profile_rig(FileMode::Also, "C:/synthetic/notes");
        let job = rig.transcribed("hi");
        rig.land(job, 3000);
        assert_eq!(last_overlay(&rig), Some(OverlayState::Hidden), "not yet");
        rig.c
            .handle(Inbox::FileWarning { job, line: answer }, Millis(3010));
        assert_eq!(last_overlay(&rig), want);
    }
    // A part held back is the news: the file does not replace it.
    let mut rig = profile_rig(FileMode::Also, "C:/synthetic/notes");
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    let audio = Inbox::JobAudio {
        job,
        samples: vec![0.5; 4],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(2100));
    rig.c.handle(low_held(job), Millis(2500));
    rig.land(job, 3000);
    assert_eq!(
        last_overlay(&rig),
        Some(OverlayState::HeldBack { all: false })
    );
    rig.c
        .handle(Inbox::FileWarning { job, line: None }, Millis(3010));
    assert_eq!(last_overlay(&rig), None);
}

/// `job` transcribed to "kept" with one part the confidence filter held back.
fn low_held(job: JobId) -> Inbox {
    Inbox::Transcribed {
        job,
        text: Some("kept".into()),
        words: Vec::new(),
        held: vec![oxi_core::history::HeldPart {
            text: "x".into(),
            t0: 0,
            t1: 100,
            why: oxi_core::text::HeldWhy::LowConfidence,
        }],
    }
}

/// d03-label review #2: a dictation queued behind one with a part held back starts at its
/// paste; the cue still goes to the pill, before the next step.
#[test]
fn a_held_back_cue_is_not_lost_to_the_next_queued_dictation() {
    let mut rig = Rig::new();
    rig.event(Event::ModelLoaded, 0);
    let first = rig.record();
    let audio = |job| Inbox::JobAudio {
        job,
        samples: vec![0.5; 4],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio(first), Millis(2100));
    rig.event(Event::ChordDown, 2200);
    rig.event(Event::ChordUp, 3200);
    rig.event(Event::Tick, 4200);
    let second = drain(&rig.audio)
        .into_iter()
        .find_map(|cmd| match cmd {
            AudioCmd::Stop(job) => Some(job),
            _ => None,
        })
        .expect("the second recording stopped");
    rig.c.handle(audio(second), Millis(4300));
    rig.c.handle(low_held(first), Millis(4400));
    drain(&rig.ui);
    rig.land(first, 5000);
    let shown: Vec<_> = drain(&rig.ui)
        .into_iter()
        .filter_map(|c| match c {
            UiCmd::Overlay(s) => Some(s),
            _ => None,
        })
        .collect();
    assert_eq!(
        shown,
        [
            OverlayState::HeldBack { all: false },
            OverlayState::Transcribing
        ]
    );
}

#[test]
fn the_app_in_front_at_key_down_sets_the_model_and_the_dictation_runs_with_its_profile() {
    let mut rig = profile_rig(FileMode::Off, "");
    let job = rig.transcribed("hi");
    let sent = drain(&rig.whisper);
    assert!(
        matches!(sent.first(), Some(WhisperCmd::UseModel(m)) if m == "tiny"),
        "the model goes before the preload"
    );
    let profile = sent.iter().find_map(|c| match c {
        WhisperCmd::Transcribe {
            job: j, profile, ..
        } if *j == job => Some(profile),
        _ => None,
    });
    let p = profile.expect("a Transcribe with its profile");
    assert_eq!((p.model.as_str(), p.language), ("tiny", Language::EN));
    assert!(!p.dictionary && p.replacements);
    // Another app in front: the globals.
    let mut rig = profile_rig(FileMode::Off, "");
    rig.c.foreground = Box::new(|| "other.exe".into());
    rig.transcribed("hi");
    let sent = drain(&rig.whisper);
    assert!(matches!(sent.first(), Some(WhisperCmd::UseModel(m)) if m == "turbo"));
}

#[test]
fn to_file_also_pastes_and_writes_and_only_files_the_entry_as_a_file() {
    for (mode, only) in [(FileMode::Also, false), (FileMode::Only, true)] {
        let mut rig = profile_rig(mode, "C:/synthetic/notes");
        let job = rig.transcribed("hi");
        let pastes = drain(&rig.paste);
        let [PasteCmd::Paste { file: Some(f), .. }] = pastes.as_slice() else {
            panic!("expected one Paste with a file");
        };
        assert_eq!(
            (f.only, f.folder.to_str()),
            (only, Some("C:/synthetic/notes"))
        );
        rig.land(job, 3000);
        let want = if only {
            oxi_core::history::Delivered::File
        } else {
            oxi_core::history::Delivered::Pasted
        };
        assert_eq!(saved(&rig), [(4, "hi".to_owned(), Some(want))]);
    }
    // No folder: no file, whatever the rule says.
    let mut rig = profile_rig(FileMode::Only, " ");
    rig.transcribed("hi");
    let pastes = drain(&rig.paste);
    assert!(matches!(
        pastes.as_slice(),
        [PasteCmd::Paste { file: None, .. }]
    ));
}
/// The models whisper was told to use, in order.
fn used_models(rig: &Rig) -> Vec<String> {
    drain(&rig.whisper)
        .into_iter()
        .filter_map(|c| match c {
            WhisperCmd::UseModel(m) => Some(m),
            _ => None,
        })
        .collect()
}

/// Review d04 #1, arch-plan 9.2: a recording that starts while a job still waits for whisper
/// keeps that job's model, whatever app is in front; once the queue is through, the next
/// press takes its own app's model again.
#[test]
fn a_busy_machine_keeps_its_model_for_the_next_recording() {
    let mut rig = profile_rig(FileMode::Off, "");
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1500);
    assert_eq!(used_models(&rig), ["tiny"]);
    rig.c.foreground = Box::new(|| "other.exe".into());
    rig.event(Event::ChordDown, 3000);
    rig.event(Event::ChordUp, 4000);
    rig.event(Event::Tick, 4500);
    assert_eq!(used_models(&rig), ["tiny"], "no switch mid-queue");
    let jobs: Vec<JobId> = {
        let mut jobs: Vec<JobId> = rig.c.profiles.keys().copied().collect();
        jobs.sort_unstable();
        jobs
    };
    assert_eq!(jobs.len(), 2);
    let second = &rig.c.profiles[&jobs[1]];
    assert_eq!(second.model, "tiny");
    assert_eq!(
        second.language, rig.c.active.model.language,
        "the app's own language still applies"
    );
    for (i, job) in jobs.iter().enumerate() {
        let at = 5000 + 100 * i as u64;
        rig.c.handle(
            Inbox::JobAudio {
                job: *job,
                samples: vec![0.5; 4],
                spool: None,
                voiced_ms: 0,
            },
            Millis(at),
        );
        rig.c.handle(
            Inbox::Transcribed {
                job: *job,
                text: Some("hi".into()),
                words: Vec::new(),
                held: Vec::new(),
            },
            Millis(at + 50),
        );
    }
    drain(&rig.whisper);
    rig.event(Event::ChordDown, 9000);
    assert_eq!(used_models(&rig), ["turbo"], "the queue is through");
}

/// Review d04 #5: the paste worker's to-file warning shows in the status until it clears.
#[test]
fn a_to_file_warning_shows_in_the_status_until_it_clears() {
    let mut rig = Rig::new();
    let line = "Dictations are not written to the file: entity not found";
    rig.c.handle(fw(JobId(0), Some(line)), Millis(10));
    assert!(rig.c.status().warnings.iter().any(|w| w == line));
    rig.c.handle(fw(JobId(0), None), Millis(20));
    assert!(!rig.c.status().warnings.iter().any(|w| w == line));
}

/// d04-fix 3: the warning of a failing folder goes when file mode is turned off or the folder
/// changes; another setting keeps it.
#[test]
fn a_to_file_warning_goes_when_its_folder_is_no_longer_written() {
    let line = "Dictations are not written to the file: entity not found";
    let output = |mode, folder: &str| {
        let mut config = Config::default();
        config.output.file_mode = mode;
        config.output.file_folder = folder.into();
        config
    };
    let mut rig = Rig::with(output(FileMode::Also, "C:/synthetic/a"));
    let warned = |rig: &Rig| rig.c.status().warnings.iter().any(|w| w == line);
    let mut at = 10;
    let mut apply = |rig: &mut Rig, config: Config| {
        rig.c.handle(fw(JobId(0), Some(line)), Millis(at));
        let config = Box::new(config);
        let warnings = Vec::new();
        rig.c
            .handle(Inbox::Config { config, warnings }, Millis(at + 5));
        at += 10;
    };
    let mut other = output(FileMode::Also, "C:/synthetic/a");
    other.paste.trailing_space = !other.paste.trailing_space;
    apply(&mut rig, other);
    assert!(warned(&rig), "the same folder is still written");
    apply(&mut rig, output(FileMode::Also, "C:/synthetic/b"));
    assert!(!warned(&rig), "another folder");
    apply(&mut rig, output(FileMode::Off, "C:/synthetic/b"));
    assert!(!warned(&rig), "file mode off");
}

/// d04-fix 2: a write queued for the old folder that fails after the folder changed (or file
/// mode went off) brings no warning back; a write of the folder in use does.
#[test]
fn a_late_failure_of_a_folder_no_longer_written_is_not_a_warning() {
    let line = "Dictations are not written to the file: entity not found";
    let warned = |rig: &Rig| rig.c.status().warnings.iter().any(|w| w == line);
    let mut rig = profile_rig(FileMode::Also, "C:/synthetic/a");
    let job = rig.transcribed("hi");
    rig.land(job, 3000);
    let mut config = rig.c.active.clone();
    config.output.file_folder = "C:/synthetic/b".into();
    let config = Box::new(config);
    let warnings = Vec::new();
    rig.c
        .handle(Inbox::Config { config, warnings }, Millis(3100));
    rig.c.handle(fw(job, Some(line)), Millis(3200));
    assert!(!warned(&rig), "the old folder's queued write");
    rig.c.handle(fw(JobId(99), Some(line)), Millis(3300));
    assert!(warned(&rig), "a write of the folder in use");
}

/// d04-fix 2: a job that ends with no audio (a silent tap, a muted mic, a failed capture)
/// leaves no profile behind, so the next press in another app takes its own model.
#[test]
fn a_job_with_no_audio_leaves_no_profile_to_hold_the_next_model() {
    for why in [
        EmptyAudio::NothingHeard,
        EmptyAudio::MicSilence,
        EmptyAudio::Failed,
    ] {
        let mut rig = profile_rig(FileMode::Off, "");
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        rig.event(Event::JobAudioEmpty { job, why }, 2100);
        assert!(rig.c.profiles.is_empty(), "{why:?}");
        drain(&rig.whisper);
        rig.c.foreground = Box::new(|| "other.exe".into());
        rig.event(Event::ChordDown, 9000);
        assert_eq!(used_models(&rig), ["turbo"], "{why:?}");
    }
}

/// Review d04 #7: a write-only dictation still transcribing at quit reaches its file too
/// (within the grace), after the clipboard.
#[test]
fn quit_writes_the_text_of_a_to_file_dictation_to_its_file() {
    let mut rig = profile_rig(FileMode::Only, "C:/synthetic/notes");
    rig.event(Event::ModelLoaded, 0);
    let job = rig.record();
    rig.c.handle(
        Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        },
        Millis(2100),
    );
    rig.drain_all();
    let msgs = vec![Inbox::Transcribed {
        job,
        text: Some("hello".into()),
        words: Vec::new(),
        held: Vec::new(),
    }];
    let q = quit(rig, msgs, 60_000);
    let sent: Vec<_> = q
        .paste
        .into_iter()
        .map(|c| match c {
            PasteCmd::CopyOnly { text, .. } => format!("copy {text}"),
            PasteCmd::File { text, to, until } => {
                assert!(to.only && to.folder.ends_with("notes"));
                assert!(until > std::time::Instant::now(), "the grace bounds it");
                format!("file {text}")
            }
            PasteCmd::Paste { .. } => panic!("pasted at quit"),
        })
        .collect();
    assert_eq!(sent, ["copy hello", "file hello"]);
}

fn fw(job: JobId, line: Option<&str>) -> Inbox {
    let line = line.map(str::to_owned);
    Inbox::FileWarning { job, line }
}

/// d03-label: a write the full queue refuses answers before the writes ahead of it. By job,
/// the late failure of the old folder is dropped and the refusal of the folder in use shows;
/// in arrival order the refusal was taken for the stale write and dropped.
#[test]
fn a_file_answer_belongs_to_its_own_job_not_to_the_oldest_write() {
    let (full, gone) = ("queue full", "entity not found");
    let mut rig = profile_rig(FileMode::Also, "C:/synthetic/a");
    let first = rig.transcribed("hi");
    rig.land(first, 3000);
    let mut config = rig.c.active.clone();
    config.output.file_folder = "C:/synthetic/b".into();
    let config = Box::new(config);
    let warnings = Vec::new();
    rig.c
        .handle(Inbox::Config { config, warnings }, Millis(3100));
    rig.event(Event::ChordDown, 4000);
    rig.event(Event::ChordUp, 5000);
    rig.event(Event::Tick, 6000);
    let second = JobId(first.0 + 1);
    assert!(drain(&rig.audio).contains(&AudioCmd::Stop(second)));
    let audio = Inbox::JobAudio {
        job: second,
        samples: vec![0.5; 4],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(6100));
    rig.c.handle(transcribed(second, Some("hi")), Millis(6200));
    rig.c.handle(fw(second, Some(full)), Millis(6300));
    rig.c.handle(fw(first, Some(gone)), Millis(6400));
    let warnings = rig.c.status().warnings;
    assert!(warnings.iter().any(|w| w == full), "{warnings:?}");
    assert!(!warnings.iter().any(|w| w == gone), "{warnings:?}");
}

/// The focus the fake OS reports: window `w`, control 2, no keys.
fn at(w: usize) -> Focus {
    Focus {
        window: w,
        control: 2,
        keys: 0,
    }
}

/// A rig with `paste.press_enter` on whose OS focus is the window in the returned cell.
fn enter_rig() -> (Rig, Rc<Cell<usize>>) {
    let mut config = Config::default();
    config.paste.press_enter = true;
    let mut rig = Rig::with(config);
    let window = Rc::new(Cell::new(1));
    let w = Rc::clone(&window);
    rig.c.focus = Box::new(move || Some(at(w.get())));
    (rig, window)
}

/// The `focus_at_start` of each paste sent since the last look.
fn starts(rig: &Rig) -> Vec<Option<Focus>> {
    drain(&rig.paste)
        .into_iter()
        .filter_map(|c| match c {
            PasteCmd::Paste { focus_at_start, .. } => Some(focus_at_start),
            _ => None,
        })
        .collect()
}

/// Review d08 #1 (the owner's case): dictate in chat A, switch to chat B while speaking or
/// while whisper runs. The paste carries A, the focus of the record start, so the worker's
/// check at Enter time sees the switch.
#[test]
fn the_paste_carries_the_focus_of_the_record_start() {
    let (mut rig, window) = enter_rig();
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    window.set(2); // switched while speaking
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 2000);
    let job = JobId(0);
    let audio = Inbox::JobAudio {
        job,
        samples: vec![0.5; 4],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio, Millis(2100));
    window.set(3); // and again while whisper runs
    rig.c.handle(transcribed(job, Some("hi")), Millis(2500));
    assert_eq!(starts(&rig), [Some(at(1))]);
    // The setting off: the OS is not asked and the paste carries none.
    let mut rig = Rig::new();
    rig.c.focus = Box::new(|| panic!("asked with the setting off"));
    rig.transcribed("hi");
    assert_eq!(starts(&rig), [None]);
}

/// Review d08 #1: a hold-gap continuation is one dictation: its Enter checks against where
/// the first part started, not where the re-press was.
#[test]
fn a_continued_dictation_carries_the_focus_of_its_first_part() {
    let (mut rig, window) = enter_rig();
    rig.event(Event::ModelLoaded, 0);
    rig.event(Event::ChordDown, 0);
    rig.event(Event::ChordUp, 1000);
    rig.event(Event::Tick, 1100);
    let audio = |job| Inbox::JobAudio {
        job,
        samples: vec![0.1; 800],
        spool: None,
        voiced_ms: 0,
    };
    rig.c.handle(audio(JobId(0)), Millis(1150));
    rig.c
        .handle(transcribed(JobId(0), Some("one")), Millis(1200));
    window.set(2);
    rig.event(Event::ChordDown, 1300); // inside the gap, in another window
    rig.event(Event::ChordUp, 2000);
    rig.event(Event::Tick, 2100);
    rig.c.handle(audio(JobId(1)), Millis(2150));
    rig.c
        .handle(transcribed(JobId(1), Some("one two")), Millis(2200));
    rig.event(Event::Tick, 2600);
    assert_eq!(starts(&rig), [Some(at(1))]);
}

/// Review d08 #5: a lock or a sleep while a dictation runs, or a recording started while
/// locked, gives its paste no start focus: the lock screen may hold the focus at both reads.
#[test]
fn a_lock_or_a_sleep_takes_the_start_focus_away() {
    // Its Resume and Unlock announce `Woke` on the process bus: out of the way of the test that
    // counts the `Woke`s on it.
    let _bus = crate::daemon::ipc::test_bus::OnBus::connected();
    for (off, on) in [
        (Event::Lock, Event::Unlock),
        (Event::Suspend, Event::Resume),
    ] {
        let (mut rig, _) = enter_rig();
        rig.event(Event::ModelLoaded, 0);
        let job = rig.record();
        let audio = Inbox::JobAudio {
            job,
            samples: vec![0.5; 4],
            spool: None,
            voiced_ms: 0,
        };
        rig.c.handle(audio, Millis(2100));
        rig.event(off.clone(), 2200);
        rig.event(on, 2300);
        rig.c.handle(transcribed(job, Some("hi")), Millis(2500));
        assert_eq!(starts(&rig), [None], "{off:?}");
    }
    // Started while locked: none either; after the unlock the next one has its own.
    let (mut rig, _) = enter_rig();
    rig.event(Event::Lock, 0);
    rig.c.start_profile();
    assert_eq!(rig.c.next_profile.as_ref().unwrap().focus_at_start, None);
    rig.event(Event::Unlock, 10);
    rig.c.start_profile();
    let next = rig.c.next_profile.as_ref().unwrap().focus_at_start;
    assert_eq!(next, Some(at(1)));
}
