//! Public vocabulary of the state machine.

use crate::types::Millis;

/// Recording mode (SPEC "Recording").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Hold to record, release to stop; a double tap locks recording on.
    Push,
    /// Press to start, press to stop; key-up is ignored.
    Toggle,
}

/// The values the machine reads from the user config; `Default` is the SPEC set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MachineConfig {
    pub mode: Mode,
    /// A press shorter than this can be half of a double tap.
    pub double_tap_max_press: Millis,
    /// Largest gap between the two presses of a double tap.
    pub double_tap_gap: Millis,
    /// A press (push) or span between presses (toggle) shorter than this is a tap: discarded.
    pub tap_max: Millis,
    /// Recording is stopped and transcribed at this length.
    pub max_recording: Millis,
    /// The overlay warns this long before `max_recording`.
    pub warn_before: Millis,
    /// How often the physical chord state is polled while a push key is held.
    pub watchdog_period: Millis,
    /// Idle time before the model is unloaded; zero means never.
    pub idle_unload: Millis,
    /// How long a transient overlay text (cancelled, nothing heard) stays up.
    pub overlay_hide: Millis,
}

impl Default for MachineConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Push,
            double_tap_max_press: Millis(250),
            double_tap_gap: Millis(300),
            tap_max: Millis(300),
            max_recording: Millis::from_secs(600),
            warn_before: Millis::from_secs(30),
            watchdog_period: Millis(100),
            idle_unload: Millis::from_secs(300),
            overlay_hide: Millis(1500),
        }
    }
}

/// Identity of one dictation job (one finished recording). Ids grow with every job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct JobId(pub u64);

/// Why the model cannot be used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFailure {
    /// No model file (ARCHITECTURE 11 item 22).
    Missing,
    /// Hash check or whisper init failed (item 23).
    Bad,
}

/// Everything that happens, as the coordinator sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The hook saw the whole chord become held.
    ChordDown,
    /// The hook saw the chord stop being held.
    ChordUp,
    /// Answer to [`Effect::PollChord`]: is the chord physically held now.
    ChordPolled {
        held: bool,
    },
    /// Esc went down (the hook swallows it only while the phase is Recording).
    Esc,
    /// Another key went down while the chord was held.
    OtherKey,
    /// Capture ended on its own (microphone unplugged); the audio so far is kept.
    AudioStopped,
    /// Capture could not run (no permission, device error); nothing is kept.
    AudioFailed,
    /// The audio of a job is trimmed and ready for whisper.
    JobAudioReady(JobId),
    /// Whisper finished; `empty` is true when nothing is left to paste.
    TranscriptionDone {
        job: JobId,
        empty: bool,
    },
    TranscriptionFailed(JobId),
    PasteDone(JobId),
    ModelLoaded,
    ModelFailed(ModelFailure),
    /// New settings; applied when the machine is Idle (item 11).
    ConfigChanged(MachineConfig),
    Suspend,
    Resume,
    Lock,
    Unlock,
    /// Time passed; the coordinator sends it at [`crate::machine::State::next_deadline`].
    Tick,
}

/// The phase shown to the user and mirrored into the `PHASE` atomic.
/// The most urgent one wins: Recording, then Transcribing, then Pasting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Recording,
    Transcribing,
    Pasting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Start,
    Stop,
    Cancel,
    Error,
}

/// Text for the overlay or a notification; the UI owns the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Notice {
    Cancelled,
    NothingHeard,
    NoModel,
    ModelBad,
    MicLost,
    MicFailed,
    TranscriptionFailed,
}

/// What the overlay shows; emitted only when it changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    Hidden,
    /// `locked`: double tap locked it on. `ending_soon`: `warn_before` has passed.
    Recording {
        locked: bool,
        ending_soon: bool,
    },
    LoadingModel,
    Transcribing,
    Pasting,
    Text(Notice),
}

/// A command for a worker or the UI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    StartCapture,
    /// Stop capture, flush, and answer with [`Event::JobAudioReady`] for this job.
    StopCapture(JobId),
    /// Stop capture and throw the audio away.
    DiscardCapture,
    /// Free the audio of a job that will never be transcribed.
    DropJob(JobId),
    Preload,
    Unload,
    /// Send the job to whisper.
    Transcribe(JobId),
    /// Send the job's text to the paste worker.
    Paste(JobId),
    /// Read the physical chord state and answer with [`Event::ChordPolled`].
    PollChord,
    ReinstallHook,
    /// Pending config became active: re-read hotkey, mic, sounds.
    ApplyConfig,
    Play(Sound),
    Overlay(Overlay),
    /// OS notification.
    Notify(Notice),
}

pub type Effects = Vec<Effect>;
