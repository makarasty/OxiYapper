//! Audio worker: owns the microphone capture between key-down and key-up (ARCHITECTURE sections 9
//! and 11). It opens a [`Capture`] on `Start`, hands the finished 16 kHz audio to the coordinator
//! on `Stop`, and turns capture events into machine events. Audio is never logged.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{Builder, JoinHandle, Scope};
use std::time::Instant;

use oxi_core::audio::AudioError;
use oxi_core::config::Audio;
use oxi_core::machine::{EmptyAudio, Event, JobId};
use oxi_core::stats::Stats;
use oxi_platform::capture::{
    Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter, Recorded,
};

use super::cmd::{AudioCmd, Inbox};

/// Start the audio worker on the real microphone. It returns when `rx` closes.
pub fn spawn(
    rx: Receiver<AudioCmd>,
    inbox: Sender<Inbox>,
    meter: LevelMeter,
) -> std::io::Result<JoinHandle<()>> {
    Builder::new()
        .name("audio".into())
        .spawn(move || run::<CpalCapture>(&rx, &inbox, &meter))
}

/// The command loop. A capture is opened here and ended on a second thread: its flush (the
/// drain, the speech gate, the history stream) never holds up the next `Start`, so a re-press
/// records at once (d11: re-press to first sample under load p50 387 -> 210 ms). That one
/// thread ends captures in the order they stopped, so job answers keep their order, and every
/// other message but `SpeechHeard` goes out behind the answers queued before it (see [`say`]).
fn run<C: Capture + Send + 'static>(
    rx: &Receiver<AudioCmd>,
    inbox: &Sender<Inbox>,
    meter: &LevelMeter,
) {
    std::thread::scope(|s| run_in::<C>(s, rx, inbox, meter));
}

fn run_in<'s, C: Capture + Send + 'static>(
    s: &'s Scope<'s, '_>,
    rx: &Receiver<AudioCmd>,
    inbox: &'s Sender<Inbox>,
    meter: &LevelMeter,
) {
    let mut opts = Audio::default();
    let mut capture: Option<C> = None;
    let (to_end, ends) = channel::<ToEnd<C>>();
    let ender = Builder::new()
        .name("audio-end".into())
        .spawn_scoped(s, move || {
            for msg in ends {
                match msg {
                    ToEnd::End(capture, job) => end(capture, job, inbox),
                    ToEnd::Say(msg) => {
                        let _ = inbox.send(msg);
                    }
                }
            }
        });
    // Without that thread (the OS refused it) captures end here, as before.
    let to_end = ender.is_ok().then_some(to_end);
    let retire = |mut capture: Option<C>, job: Option<JobId>| {
        // The stream stops here, at once: what the mic hears next belongs to the next capture
        // only, and the device is free for its open (d11 review #1, #2). Only the flush waits.
        if let Some(c) = capture.as_mut() {
            c.stop_stream();
        }
        match &to_end {
            Some(tx) => {
                let _ = tx.send(ToEnd::End(capture, job));
            }
            None => end(capture, job, inbox),
        }
    };

    // Speech comes from the capture as an event: no timer here, the loop only waits for commands.
    while let Ok(cmd) = rx.recv() {
        match cmd {
            AudioCmd::Config(audio) => opts = audio,
            AudioCmd::Start { session, pressed } => {
                // A second Start must not hold two streams.
                if capture.is_some() {
                    retire(capture.take(), None);
                }
                let (queue, direct) = (to_end.clone(), inbox.clone());
                let opening = Instant::now();
                let pressed = pressed.unwrap_or(opening);
                let opened = C::open_pressed(&opts, meter.clone(), pressed, move |e| {
                    let event = match e {
                        CaptureEvent::DeviceLost => Event::DeviceLost,
                        CaptureEvent::Failed => Event::CaptureFailed,
                        // It only marks this capture's recording, never the old job's answer:
                        // behind a slow flush it would come after the shortcut window closed.
                        CaptureEvent::SpeechHeard => {
                            let event = Event::SpeechHeard;
                            let _ = direct.send(Inbox::Capture { session, event });
                            return;
                        }
                    };
                    say(queue.as_ref(), &direct, Inbox::Capture { session, event });
                });
                // Speech before the stream runs is never recorded: this says how long that was.
                let open_ms = opening.elapsed().as_millis();
                match opened {
                    Ok(c) => {
                        tracing::info!(session, open_ms, "microphone opened");
                        if c.fell_back_to_default() {
                            let event = Event::MicFellBack;
                            say(to_end.as_ref(), inbox, Inbox::Capture { session, event });
                        }
                        capture = Some(c);
                    }
                    Err(e) => {
                        tracing::warn!("microphone not opened: {e}");
                        let event = Event::CaptureFailed;
                        say(to_end.as_ref(), inbox, Inbox::Capture { session, event });
                    }
                }
            }
            AudioCmd::Discard => {
                if capture.is_some() {
                    retire(capture.take(), None);
                }
            }
            AudioCmd::Stop(job) => retire(capture.take(), Some(job)),
        }
    }
}

/// What the audio-end thread gets, in order.
enum ToEnd<C> {
    /// A stopped capture to end, and the job it answers.
    End(Option<C>, Option<JobId>),
    /// A message to pass on once the captures before it are ended.
    Say(Inbox),
}

/// Sends `msg` behind the captures still ending (`ender`), or straight to `inbox` without that
/// thread. Before d11 a capture ended before the next one opened, so no event of the new one
/// overtook the old job's answer; the machine relies on that order (a late "nothing heard"
/// would cover the new capture's failure on the pill). A send fails only when the coordinator
/// is gone; nothing is left to tell then.
fn say<C>(ender: Option<&Sender<ToEnd<C>>>, inbox: &Sender<Inbox>, msg: Inbox) {
    match ender {
        Some(tx) => {
            let _ = tx.send(ToEnd::Say(msg));
        }
        None => {
            let _ = inbox.send(msg);
        }
    }
}

/// Ends `capture`: with `job`, answers it with the audio or why there is none; without, drops
/// the audio.
fn end<C: Capture>(capture: Option<C>, job: Option<JobId>, inbox: &Sender<Inbox>) {
    let Some(job) = job else {
        drop(capture);
        return;
    };
    let answer = match finish(capture, job) {
        Ok(Recorded {
            samples,
            spool,
            trim,
            stats,
        }) => Inbox::JobAudio {
            job,
            samples,
            spool,
            voiced_ms: Stats::voiced_time(trim, &stats).0,
        },
        Err(why) => Inbox::Event(Event::JobAudioEmpty { job, why }),
    };
    let _ = inbox.send(answer);
}

/// The audio of a stopped capture, or why there is none. Either way the job gets its answer, so
/// it is not stranded in `AwaitingAudio`, and whisper never gets empty audio. Every outcome is
/// logged with where the voice sat (never the audio), so a lost recording leaves a reason.
fn finish<C: Capture>(capture: Option<C>, job: JobId) -> Result<Recorded, EmptyAudio> {
    let Some(capture) = capture else {
        tracing::warn!(?job, "stop without an open capture");
        return Err(EmptyAudio::Failed);
    };
    let dropped = capture.dropped_samples();
    if dropped > 0 {
        tracing::warn!(?job, dropped, "capture ring overflowed");
    }
    let result = match capture.finish_recording() {
        Ok(rec) if !rec.samples.is_empty() => Ok(rec),
        // A quiet tap is ordinary: no warning.
        Ok(_) | Err(CaptureError::Audio(AudioError::NothingHeard)) => Err(EmptyAudio::NothingHeard),
        Err(CaptureError::Audio(AudioError::MicSilence)) => Err(EmptyAudio::MicSilence),
        Err(e) => {
            tracing::warn!(?job, "capture failed to finish: {e}");
            Err(EmptyAudio::Failed)
        }
    };
    match &result {
        Ok(Recorded { trim, stats, .. }) => tracing::info!(
            ?job,
            outcome = "ok",
            ?trim,
            audio_ms = stats.audio_ms,
            voiced_ms = stats.voiced_ms,
            first_voiced_ms = ?stats.first_voiced_ms,
            last_voiced_to_end_ms = ?stats.last_voiced_to_end_ms,
            dropped_samples = dropped,
            "capture finished"
        ),
        Err(why) => {
            tracing::info!(?job, outcome = ?why, dropped_samples = dropped, "capture finished");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::*;

    type Heard = Arc<Mutex<Vec<f32>>>;

    /// Streams open now: their mic group (the part of `opts.device` after ':') and what each
    /// heard. A group is one test's microphone, so tests running at once never share one.
    static LIVE: Mutex<Vec<(String, Heard)>> = Mutex::new(Vec::new());
    /// Groups in which a stream opened while another was still open.
    static OVERLAPS: Mutex<Vec<String>> = Mutex::new(Vec::new());
    /// The key-down each group's open was given.
    static PRESSED: Mutex<Vec<(String, Instant)>> = Mutex::new(Vec::new());
    /// Names put up on one thread and waited for on another.
    struct Board(Mutex<Vec<String>>, Condvar);

    impl Board {
        const fn new() -> Self {
            Self(Mutex::new(Vec::new()), Condvar::new())
        }

        fn put(&self, name: &str) {
            self.0.lock().unwrap().push(name.into());
            self.1.notify_all();
        }

        /// Waits until `name` is up `n` times; false after 10 s.
        fn wait(&self, name: &str, n: usize) -> bool {
            let count = |up: &Vec<String>| up.iter().filter(|g| *g == name).count();
            let up = self.0.lock().unwrap();
            let wait = Duration::from_secs(10);
            let (up, _) = self
                .1
                .wait_timeout_while(up, wait, |up| count(up) < n)
                .unwrap();
            count(&up) >= n
        }
    }

    /// Groups whose "hold" captures may end ([`release`]).
    static RELEASED: Board = Board::new();
    /// One entry per stream a group opened.
    static OPENED: Board = Board::new();

    fn release(group: &str) {
        RELEASED.put(group);
    }

    /// The person says `samples`: every stream of `group` still open hears them.
    fn speak(group: &str, samples: &[f32]) {
        for (_, heard) in LIVE.lock().unwrap().iter().filter(|(g, _)| g == group) {
            heard.lock().unwrap().extend_from_slice(samples);
        }
    }

    fn overlapped(group: &str) -> bool {
        OVERLAPS.lock().unwrap().iter().any(|g| g == group)
    }

    /// No microphone: `opts.device` is "kind" or "kind:group" and the kind picks the behaviour.
    /// "fail" cannot open, "fallback" reports the default device, "lose" loses the device right
    /// after opening, "bad-finish" fails in `finish`, "quiet" hears no speech, "mute" gives all
    /// zeros, "spooled" streamed to history, "speech" hears the user speak right after opening,
    /// "hold" ends (finish or drop) only after [`release`] of its group. A grouped stream hears
    /// what is [`speak`]-ed until its stream stops; its audio is [`FULL`] and then that.
    struct Fake {
        kind: String,
        group: String,
        heard: Heard,
    }

    const FULL: [f32; 4] = [0.5; 4];
    const SPOOL: &str = "h/2026-11-10/120000-000.wav.part";
    const PARTIAL: [f32; 2] = [0.25; 2];
    const WORDS: [f32; 3] = [0.75; 3];

    fn split(device: &str) -> (&str, &str) {
        device.split_once(':').unwrap_or((device, ""))
    }

    impl Capture for Fake {
        fn open(
            opts: &Audio,
            _meter: LevelMeter,
            mut on_event: impl FnMut(CaptureEvent) + Send + 'static,
        ) -> Result<Self, CaptureError> {
            let (kind, group) = split(&opts.device);
            if kind == "fail" {
                return Err(CaptureError::NoDevice);
            }
            if kind == "lose" {
                on_event(CaptureEvent::DeviceLost);
            }
            if kind == "speech" {
                on_event(CaptureEvent::SpeechHeard);
            }
            let heard = Heard::default();
            if !group.is_empty() {
                let mut live = LIVE.lock().unwrap();
                if live.iter().any(|(g, _)| g == group) {
                    OVERLAPS.lock().unwrap().push(group.into());
                }
                live.push((group.into(), Arc::clone(&heard)));
                OPENED.put(group);
            }
            Ok(Self {
                kind: kind.into(),
                group: group.into(),
                heard,
            })
        }
        fn open_pressed(
            opts: &Audio,
            meter: LevelMeter,
            pressed: Instant,
            on_event: impl FnMut(CaptureEvent) + Send + 'static,
        ) -> Result<Self, CaptureError> {
            let group = split(&opts.device).1.to_owned();
            PRESSED.lock().unwrap().push((group, pressed));
            Self::open(opts, meter, on_event)
        }
        fn stop_stream(&mut self) {
            LIVE.lock()
                .unwrap()
                .retain(|(_, h)| !Arc::ptr_eq(h, &self.heard));
        }
        fn input_rate(&self) -> u32 {
            48_000
        }
        fn dropped_samples(&self) -> u64 {
            0
        }
        fn fell_back_to_default(&self) -> bool {
            self.kind == "fallback"
        }
        fn finish_recording(self) -> Result<Recorded, CaptureError> {
            let spool = (self.kind == "spooled").then(|| SPOOL.into());
            self.finish().map(|samples| Recorded {
                samples,
                spool,
                ..Recorded::default()
            })
        }
        fn finish(self) -> Result<Vec<f32>, CaptureError> {
            if self.kind == "hold" {
                RELEASED.wait(&self.group, 1);
            }
            // Read after the wait: a stream still open hears on until its finish stops it.
            let heard = self.heard.lock().unwrap().clone();
            match self.kind.as_str() {
                "lose" => Ok(PARTIAL.to_vec()),
                "bad-finish" => Err(CaptureError::NoDevice),
                "quiet" => Err(AudioError::NothingHeard.into()),
                "mute" => Err(AudioError::MicSilence.into()),
                _ => Ok([&FULL[..], &heard].concat()),
            }
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            if self.kind == "hold" {
                RELEASED.wait(&self.group, 1);
            }
            self.stop_stream();
        }
    }

    const J: JobId = JobId(7);

    fn start(session: u64) -> AudioCmd {
        AudioCmd::Start {
            session,
            pressed: None,
        }
    }

    fn cfg(device: &str) -> AudioCmd {
        AudioCmd::Config(Audio {
            device: device.into(),
            ..Audio::default()
        })
    }

    /// Feed `cmds` to a worker, wait for it to end, and return everything it sent.
    fn drive(cmds: Vec<AudioCmd>) -> Vec<Inbox> {
        let (tx, rx) = channel();
        let (inbox_tx, inbox_rx) = channel();
        for c in cmds {
            tx.send(c).unwrap();
        }
        drop(tx);
        run::<Fake>(&rx, &inbox_tx, &LevelMeter::default());
        drop(inbox_tx);
        inbox_rx.iter().collect()
    }

    fn job_audio(msgs: &[Inbox]) -> Vec<(JobId, Vec<f32>)> {
        msgs.iter()
            .filter_map(|m| match m {
                Inbox::JobAudio { job, samples, .. } => Some((*job, samples.clone())),
                _ => None,
            })
            .collect()
    }

    fn events(msgs: &[Inbox]) -> Vec<Event> {
        msgs.iter()
            .filter_map(|m| match m {
                Inbox::Event(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn captures(msgs: &[Inbox]) -> Vec<(u64, Event)> {
        msgs.iter()
            .filter_map(|m| match m {
                Inbox::Capture { session, event } => Some((*session, event.clone())),
                _ => None,
            })
            .collect()
    }

    fn empty(why: EmptyAudio) -> Event {
        Event::JobAudioEmpty { job: J, why }
    }

    #[test]
    fn start_then_stop_gives_one_job_audio() {
        let msgs = drive(vec![start(1), AudioCmd::Stop(J)]);
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec())]);
        assert_eq!(msgs.len(), 1); // the job audio, no event
    }

    #[test]
    fn the_job_audio_carries_the_streamed_recording_of_its_capture() {
        let spools = |msgs: Vec<Inbox>| -> Vec<Option<std::path::PathBuf>> {
            msgs.into_iter()
                .filter_map(|m| match m {
                    Inbox::JobAudio { spool, .. } => Some(spool),
                    _ => None,
                })
                .collect()
        };
        let on = drive(vec![cfg("spooled"), start(1), AudioCmd::Stop(J)]);
        assert_eq!(spools(on), [Some(SPOOL.into())]);
        let off = drive(vec![start(1), AudioCmd::Stop(J)]);
        assert_eq!(spools(off), [None]);
    }

    #[test]
    fn discard_gives_no_answer_and_a_later_stop_gets_failed() {
        let msgs = drive(vec![start(1), AudioCmd::Discard]);
        assert!(msgs.is_empty());
        let msgs = drive(vec![start(1), AudioCmd::Discard, AudioCmd::Stop(J)]);
        assert_eq!(events(&msgs), [empty(EmptyAudio::Failed)]);
        assert_eq!(msgs.len(), 1); // no job audio
    }

    #[test]
    fn lost_device_still_gives_partial_audio() {
        let msgs = drive(vec![cfg("lose"), start(1), AudioCmd::Stop(J)]);
        assert_eq!(captures(&msgs), [(1, Event::DeviceLost)]);
        assert_eq!(job_audio(&msgs), [(J, PARTIAL.to_vec())]);
    }

    #[test]
    fn capture_events_carry_the_session_of_their_start() {
        let msgs = drive(vec![cfg("lose"), start(1), AudioCmd::Stop(J), start(2)]);
        assert_eq!(
            captures(&msgs),
            [(1, Event::DeviceLost), (2, Event::DeviceLost)]
        );
    }

    #[test]
    fn failed_open_gives_capture_failed() {
        let msgs = drive(vec![cfg("fail"), start(3)]);
        assert_eq!(captures(&msgs), [(3, Event::CaptureFailed)]);
        assert_eq!(msgs.len(), 1); // the event, no job audio
    }

    #[test]
    fn fallback_device_reports_mic_fell_back() {
        let msgs = drive(vec![cfg("fallback"), start(1), AudioCmd::Stop(J)]);
        assert_eq!(captures(&msgs), [(1, Event::MicFellBack)]);
        assert_eq!(job_audio(&msgs).len(), 1);
    }

    #[test]
    fn stop_without_audio_answers_why_and_sends_no_samples() {
        let cases = [
            ("quiet", EmptyAudio::NothingHeard),
            ("mute", EmptyAudio::MicSilence),
            ("bad-finish", EmptyAudio::Failed),
        ];
        for (device, why) in cases {
            let msgs = drive(vec![cfg(device), start(1), AudioCmd::Stop(J)]);
            assert_eq!(events(&msgs), [empty(why)], "{device}");
            assert_eq!(msgs.len(), 1, "{device}: no job audio");
        }
    }

    #[test]
    fn config_applies_to_the_next_start_only() {
        let msgs = drive(vec![
            start(1), // default device opens
            cfg("fail"),
            AudioCmd::Stop(J), // the open capture is untouched
            start(2),          // now fails
        ]);
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec())]);
        assert_eq!(captures(&msgs), [(2, Event::CaptureFailed)]);
    }

    #[test]
    fn audio_speech_heard_by_the_capture_reaches_the_machine_with_its_session() {
        let msgs = drive(vec![cfg("speech"), start(4), AudioCmd::Stop(J)]);
        assert_eq!(captures(&msgs), [(4, Event::SpeechHeard)]);
        assert_eq!(job_audio(&msgs).len(), 1);
        // A recording the capture heard nothing in says nothing.
        let quiet = drive(vec![start(5), AudioCmd::Stop(J)]);
        assert_eq!(captures(&quiet), []);
    }

    /// Runs a worker on `cmds`. Once `group` opened `opens` streams, its "hold" captures still
    /// ending, it runs `meanwhile`, releases the group and sends `then`. Returns everything the
    /// worker sent, in order.
    fn drive_held(
        group: &str,
        cmds: Vec<AudioCmd>,
        opens: usize,
        meanwhile: impl FnOnce(),
        then: Vec<AudioCmd>,
    ) -> Vec<Inbox> {
        let (tx, rx) = channel();
        let (inbox_tx, inbox_rx) = channel();
        let worker =
            std::thread::spawn(move || run::<Fake>(&rx, &inbox_tx, &LevelMeter::default()));
        for c in cmds {
            tx.send(c).unwrap();
        }
        let opened = OPENED.wait(group, opens);
        meanwhile();
        release(group);
        for c in then {
            tx.send(c).unwrap();
        }
        drop(tx);
        worker.join().unwrap();
        assert!(
            opened,
            "{group}: the next capture waited for the old one to end"
        );
        inbox_rx.iter().collect()
    }

    /// Each message the worker sent, as text: the answered job or the session and its event.
    fn order(msgs: &[Inbox]) -> Vec<String> {
        msgs.iter()
            .map(|m| match m {
                Inbox::JobAudio { job, .. } => format!("audio {}", job.0),
                Inbox::Event(e) => format!("{e:?}"),
                Inbox::Capture { session, event } => format!("{session} {event:?}"),
                _ => "other".into(),
            })
            .collect()
    }

    /// A capture still ending never holds up the next `Start`: a re-press records at once (d11).
    /// The jobs are still answered in the order they stopped.
    #[test]
    fn a_start_opens_while_the_stopped_capture_still_ends() {
        let j2 = JobId(8);
        let cmds = vec![cfg("hold:aside"), start(1), AudioCmd::Stop(J), start(2)];
        let msgs = drive_held("aside", cmds, 2, || {}, vec![AudioCmd::Stop(j2)]);
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec()), (j2, FULL.to_vec())]);
    }

    /// d11 review, the new order: an event of the next capture (from its stream or from its
    /// open) never overtakes the answer of the job still ending, as before d11. The machine
    /// relies on it: a late "nothing heard" would cover the next capture's failure on the pill.
    #[test]
    fn the_next_captures_events_wait_behind_the_old_jobs_answer() {
        let cases = [
            ("fallback", "2 MicFellBack", 3),
            ("fail", "2 CaptureFailed", 2),
        ];
        for (kind, event, opens) in cases {
            let group = format!("order-{kind}");
            let cmds = vec![
                cfg(&format!("hold:{group}")),
                start(1),
                AudioCmd::Stop(J),
                cfg(&format!("{kind}:{group}")),
                start(2),
                // Opens once session 2's open is done: the wait below then covers it too.
                cfg(&format!("x:{group}")),
                start(3),
            ];
            let msgs = drive_held(&group, cmds, opens, || {}, vec![AudioCmd::Stop(JobId(8))]);
            assert_eq!(order(&msgs), ["audio 7", event, "audio 8"], "{kind}");
        }
    }

    /// d11 re-review: the next capture's `SpeechHeard` does not wait for a slow flush of the old
    /// job (the shortcut window would close first and drop the words).
    #[test]
    fn the_next_captures_speech_heard_overtakes_the_old_jobs_answer() {
        let cmds = vec![
            cfg("hold:speech-fast"),
            start(1),
            AudioCmd::Stop(J),
            cfg("speech:speech-fast"),
            start(2),
        ];
        let msgs = drive_held("speech-fast", cmds, 2, || {}, vec![]);
        assert_eq!(order(&msgs), ["2 SpeechHeard", "audio 7"]);
    }

    /// d11 review #1: a stopped capture's stream stops with the `Stop`, not when its flush
    /// ends: what is said after it goes to the next recording only, never into both.
    #[test]
    fn a_stopped_capture_keeps_nothing_said_after_its_stop() {
        let j2 = JobId(8);
        let cmds = vec![
            cfg("hold:after-stop"),
            start(1),
            AudioCmd::Stop(J),
            start(2),
        ];
        let say = || speak("after-stop", &WORDS);
        let msgs = drive_held("after-stop", cmds, 2, say, vec![AudioCmd::Stop(j2)]);
        let second = [&FULL[..], &WORDS].concat();
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec()), (j2, second)]);
        assert!(!overlapped("after-stop"), "two streams were open");
    }

    /// d11 review #2: a second `Start` and a `Discard` free the device before the next open,
    /// also while the old capture still ends: one stream at a time (ALSA `hw:` would be busy).
    #[test]
    fn never_two_streams_open_at_once() {
        let cmds = vec![
            cfg("hold:one-stream"),
            start(1),
            start(2),
            AudioCmd::Discard,
            start(3),
        ];
        let msgs = drive_held("one-stream", cmds, 3, || {}, vec![AudioCmd::Stop(J)]);
        assert!(!overlapped("one-stream"), "two streams were open");
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec())]);
    }

    /// d11 review #3: the capture opens with the key-down time, so `capture started` counts
    /// from the press.
    #[test]
    fn the_capture_gets_the_key_down_time() {
        let t = Instant::now();
        let pressed = Some(t);
        drive(vec![
            cfg("x:pressed"),
            AudioCmd::Start {
                session: 1,
                pressed,
            },
            AudioCmd::Stop(J),
        ]);
        // The guard goes before the assert: a panic holding it would fail every later open.
        let seen = PRESSED
            .lock()
            .unwrap()
            .iter()
            .any(|(g, at)| g == "pressed" && *at == t);
        assert!(seen);
    }

    #[test]
    fn spawned_worker_ends_when_the_channel_closes() {
        let (tx, rx) = channel();
        let (inbox_tx, _inbox_rx) = channel();
        let handle = spawn(rx, inbox_tx, LevelMeter::default()).unwrap();
        drop(tx);
        handle.join().unwrap();
    }
}
