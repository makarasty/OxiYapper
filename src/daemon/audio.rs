//! Audio worker: owns the microphone capture between key-down and key-up (ARCHITECTURE sections 9
//! and 11). It opens a [`Capture`] on `Start`, hands the finished 16 kHz audio to the coordinator
//! on `Stop`, and turns capture events into machine events. Audio is never logged.

use std::sync::mpsc::{Receiver, Sender};
use std::thread::{Builder, JoinHandle};

use oxi_core::audio::AudioError;
use oxi_core::config::Audio;
use oxi_core::machine::{EmptyAudio, Event, JobId};
use oxi_platform::capture::{Capture, CaptureError, CaptureEvent, CpalCapture, LevelMeter};

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

/// The command loop. The capture is created and dropped on this thread only (cpal streams are
/// not always `Send`), so `C` needs no `Send` bound.
fn run<C: Capture>(rx: &Receiver<AudioCmd>, inbox: &Sender<Inbox>, meter: &LevelMeter) {
    let mut opts = Audio::default();
    let mut capture: Option<C> = None;
    // A send fails only when the coordinator is gone; nothing is left to tell then.
    let send = |msg: Inbox| {
        let _ = inbox.send(msg);
    };

    for cmd in rx {
        match cmd {
            AudioCmd::Config(audio) => opts = audio,
            AudioCmd::Start { session } => {
                capture = None; // a second Start must not hold two streams
                let events = inbox.clone();
                let opened = C::open(&opts, meter.clone(), move |e| {
                    let event = match e {
                        CaptureEvent::DeviceLost => Event::DeviceLost,
                        CaptureEvent::Failed => Event::CaptureFailed,
                    };
                    let _ = events.send(Inbox::Capture { session, event });
                });
                match opened {
                    Ok(c) => {
                        if c.fell_back_to_default() {
                            let event = Event::MicFellBack;
                            send(Inbox::Capture { session, event });
                        }
                        capture = Some(c);
                    }
                    Err(e) => {
                        tracing::warn!("microphone not opened: {e}");
                        let event = Event::CaptureFailed;
                        send(Inbox::Capture { session, event });
                    }
                }
            }
            AudioCmd::Discard => capture = None,
            AudioCmd::Stop(job) => send(match finish(capture.take(), job) {
                Ok(samples) => Inbox::JobAudio { job, samples },
                Err(why) => Inbox::Event(Event::JobAudioEmpty { job, why }),
            }),
        }
    }
}

/// The audio of a stopped capture, or why there is none. Either way the job gets its answer, so
/// it is not stranded in `AwaitingAudio`, and whisper never gets empty audio.
fn finish<C: Capture>(capture: Option<C>, job: JobId) -> Result<Vec<f32>, EmptyAudio> {
    let Some(capture) = capture else {
        tracing::warn!(?job, "stop without an open capture");
        return Err(EmptyAudio::Failed);
    };
    let dropped = capture.dropped_samples();
    if dropped > 0 {
        tracing::warn!(?job, dropped, "capture ring overflowed");
    }
    match capture.finish() {
        Ok(samples) if !samples.is_empty() => Ok(samples),
        // A quiet tap is ordinary: no warning.
        Ok(_) | Err(CaptureError::Audio(AudioError::NothingHeard)) => Err(EmptyAudio::NothingHeard),
        Err(CaptureError::Audio(AudioError::MicSilence)) => Err(EmptyAudio::MicSilence),
        Err(e) => {
            tracing::warn!(?job, "capture failed to finish: {e}");
            Err(EmptyAudio::Failed)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use super::*;

    /// No microphone: `opts.device` picks the behaviour. "fail" cannot open, "fallback" reports
    /// the default device, "lose" loses the device right after opening, "bad-finish" fails in
    /// `finish`, "quiet" hears no speech, "mute" gives all zeros.
    struct Fake {
        device: String,
    }

    const FULL: [f32; 4] = [0.5; 4];
    const PARTIAL: [f32; 2] = [0.25; 2];

    impl Capture for Fake {
        fn open(
            opts: &Audio,
            _meter: LevelMeter,
            mut on_event: impl FnMut(CaptureEvent) + Send + 'static,
        ) -> Result<Self, CaptureError> {
            if opts.device == "fail" {
                return Err(CaptureError::NoDevice);
            }
            if opts.device == "lose" {
                on_event(CaptureEvent::DeviceLost);
            }
            Ok(Self {
                device: opts.device.clone(),
            })
        }
        fn input_rate(&self) -> u32 {
            48_000
        }
        fn dropped_samples(&self) -> u64 {
            0
        }
        fn fell_back_to_default(&self) -> bool {
            self.device == "fallback"
        }
        fn finish(self) -> Result<Vec<f32>, CaptureError> {
            match self.device.as_str() {
                "lose" => Ok(PARTIAL.to_vec()),
                "bad-finish" => Err(CaptureError::NoDevice),
                "quiet" => Err(AudioError::NothingHeard.into()),
                "mute" => Err(AudioError::MicSilence.into()),
                _ => Ok(FULL.to_vec()),
            }
        }
    }

    const J: JobId = JobId(7);

    fn start(session: u64) -> AudioCmd {
        AudioCmd::Start { session }
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
                Inbox::JobAudio { job, samples } => Some((*job, samples.clone())),
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
    fn spawned_worker_ends_when_the_channel_closes() {
        let (tx, rx) = channel();
        let (inbox_tx, _inbox_rx) = channel();
        let handle = spawn(rx, inbox_tx, LevelMeter::default()).unwrap();
        drop(tx);
        handle.join().unwrap();
    }
}
