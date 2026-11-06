//! Audio worker: owns the microphone capture between key-down and key-up (ARCHITECTURE sections 9
//! and 11). It opens a [`Capture`] on `Start`, hands the finished 16 kHz audio to the coordinator
//! on `Stop`, and turns capture events into machine events. Audio is never logged.

#![allow(
    dead_code,
    reason = "run.rs (t40) spawns the worker; until then only the tests do"
)]

use std::sync::mpsc::{Receiver, Sender};
use std::thread::{Builder, JoinHandle};

use oxi_core::config::Audio;
use oxi_core::machine::{Event, JobId};
use oxi_platform::capture::{Capture, CaptureEvent, CpalCapture, LevelMeter};

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
    let event = |e: Event| {
        let _ = inbox.send(Inbox::Event(e));
    };

    for cmd in rx {
        match cmd {
            AudioCmd::Config(audio) => opts = audio,
            AudioCmd::Start => {
                capture = None; // a second Start must not hold two streams
                let events = inbox.clone();
                let opened = C::open(&opts, meter.clone(), move |e| {
                    let _ = events.send(Inbox::Event(match e {
                        CaptureEvent::DeviceLost => Event::DeviceLost,
                        CaptureEvent::Failed => Event::CaptureFailed,
                    }));
                });
                match opened {
                    Ok(c) => {
                        if c.fell_back_to_default() {
                            event(Event::MicFellBack);
                        }
                        capture = Some(c);
                    }
                    Err(e) => {
                        tracing::warn!("microphone not opened: {e}");
                        event(Event::CaptureFailed);
                    }
                }
            }
            AudioCmd::Discard => capture = None,
            AudioCmd::Stop(job) => {
                let samples = finish(capture.take(), job);
                let _ = inbox.send(Inbox::JobAudio { job, samples });
            }
        }
    }
}

/// The audio of a stopped capture. A capture that is missing or fails to finish gives no samples:
/// the job still gets its answer, so it is not stranded in `AwaitingAudio`, and whisper hears
/// nothing.
fn finish<C: Capture>(capture: Option<C>, job: JobId) -> Vec<f32> {
    let Some(capture) = capture else {
        tracing::warn!(?job, "stop without an open capture");
        return Vec::new();
    };
    let dropped = capture.dropped_samples();
    if dropped > 0 {
        tracing::warn!(?job, dropped, "capture ring overflowed");
    }
    capture.finish().unwrap_or_else(|e| {
        tracing::warn!(?job, "capture failed to finish: {e}");
        Vec::new()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::channel;

    use oxi_platform::capture::CaptureError;

    use super::*;

    /// No microphone: `opts.device` picks the behaviour. "fail" cannot open, "fallback" reports
    /// the default device, "lose" loses the device right after opening, "bad-finish" fails in
    /// `finish`.
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
                _ => Ok(FULL.to_vec()),
            }
        }
    }

    const J: JobId = JobId(7);

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

    #[test]
    fn start_then_stop_gives_one_job_audio() {
        let msgs = drive(vec![AudioCmd::Start, AudioCmd::Stop(J)]);
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec())]);
        assert_eq!(msgs.len(), 1); // the job audio, no event
    }

    #[test]
    fn discard_gives_no_audio_and_a_later_stop_gets_empty() {
        let msgs = drive(vec![AudioCmd::Start, AudioCmd::Discard]);
        assert!(msgs.is_empty());
        let msgs = drive(vec![AudioCmd::Start, AudioCmd::Discard, AudioCmd::Stop(J)]);
        assert_eq!(job_audio(&msgs), [(J, Vec::new())]);
    }

    #[test]
    fn lost_device_still_gives_partial_audio() {
        let msgs = drive(vec![cfg("lose"), AudioCmd::Start, AudioCmd::Stop(J)]);
        assert_eq!(events(&msgs), [Event::DeviceLost]);
        assert_eq!(job_audio(&msgs), [(J, PARTIAL.to_vec())]);
    }

    #[test]
    fn failed_open_gives_capture_failed() {
        let msgs = drive(vec![cfg("fail"), AudioCmd::Start]);
        assert_eq!(events(&msgs), [Event::CaptureFailed]);
        assert_eq!(msgs.len(), 1); // the event, no job audio
    }

    #[test]
    fn fallback_device_reports_mic_fell_back() {
        let msgs = drive(vec![cfg("fallback"), AudioCmd::Start, AudioCmd::Stop(J)]);
        assert_eq!(events(&msgs), [Event::MicFellBack]);
        assert_eq!(job_audio(&msgs).len(), 1);
    }

    #[test]
    fn failed_finish_still_answers_the_job_with_no_samples() {
        let msgs = drive(vec![cfg("bad-finish"), AudioCmd::Start, AudioCmd::Stop(J)]);
        assert_eq!(job_audio(&msgs), [(J, Vec::new())]);
    }

    #[test]
    fn config_applies_to_the_next_start_only() {
        let msgs = drive(vec![
            AudioCmd::Start, // default device opens
            cfg("fail"),
            AudioCmd::Stop(J), // the open capture is untouched
            AudioCmd::Start,   // now fails
        ]);
        assert_eq!(job_audio(&msgs), [(J, FULL.to_vec())]);
        assert_eq!(events(&msgs), [Event::CaptureFailed]);
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
