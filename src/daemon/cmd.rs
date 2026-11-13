//! Messages between the coordinator and the daemon threads (ARCHITECTURE section 2, Channels).
//!
//! Wiring contract for the worker modules and `run.rs`:
//! - Every thread sends into one coordinator inbox, a cloned `mpsc::Sender<Inbox>`.
//! - The coordinator owns one `mpsc::Sender` per worker, bundled in [`Workers`]. Workers never
//!   call each other: audio hands its samples to the coordinator, which forwards them to whisper.
//! - A worker module exposes `pub fn spawn(rx: Receiver<XCmd>, inbox: Sender<Inbox>, ..) ->
//!   std::io::Result<JoinHandle<()>>` (a named `thread::Builder`). It handles commands in order
//!   and returns when `rx` closes, which happens when the coordinator ends. Extra arguments are
//!   the worker's own: audio the shared `LevelMeter`, whisper the `Layout` and the `PhaseCell`,
//!   io a sender for IPC pushes. io answers `Inbox::ModelDownloaded` for each finished download.
//! - What each worker answers: audio `Inbox::JobAudio`, or `Event::JobAudioEmpty`, for
//!   `Stop(job)`, plus `Inbox::Capture` with `Event::DeviceLost`, `CaptureFailed` or
//!   `MicFellBack`; whisper `Event::ModelLoaded` or `ModelFailed` for
//!   `Preload` and `Inbox::Transcribed` for `Transcribe` (after `ModelFailed` when the model
//!   could not be loaded for it), plus `Inbox::EngineWarning` when its device changes and `Inbox::Backend` after each load; paste `Event::PasteDone` for `Paste`
//!   and nothing for `CopyOnly`.
//! - The hook thread sends the machine's `Event` on its own `Sender<Event>`; `run.rs` forwards
//!   that channel into the inbox as [`Inbox::Event`]. The coordinator calls the hook only
//!   through [`ChordHook`].
//! - The UI thread drains `Receiver<UiCmd>` each time [`Workers::wake_ui`] wakes its loop.
//!
//! Types that carry dictated text have no `Debug`, so the text cannot reach a log by `{:?}`.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use oxi_core::config::{
    Accent, Audio, Config, ConfigError, FileFormat, History, Hotkey, Overlay, OverlayPosition,
    Paste, Sounds,
};
use oxi_core::device::DeviceChoice;
use oxi_core::ipc::SoundWarning;
use oxi_core::machine::{
    EngineWarning, Event, JobId, ModelFailure, Notice, OverlayState, Phase, Sound,
};
use oxi_core::paste::{Focus, Profile};
use oxi_core::tray::TrayBadge;
use oxi_core::tuning::SettingsSet;
use oxi_core::types::Language;
use oxi_platform::os::overlay::Progress;

use super::ipc::{Origin, Pushes};

/// Everything the coordinator hears, on one channel with many senders.
pub enum Inbox {
    /// Hook, worker and session events, fed straight into the state machine.
    Event(Event),
    /// Audio worker: the trimmed 16 kHz mono samples of a stopped job, never empty, and its raw
    /// recording streamed to the history folder (`None` while saving is off). The spool goes
    /// with the job to its `HistoryCmd::Save`, or to `history::no_transcript`. `voiced_ms` is
    /// the part of the recording the speech gate scored as voice, for the speech rate and the
    /// pause share in the usage stats.
    JobAudio {
        job: JobId,
        samples: Vec<f32>,
        spool: Option<PathBuf>,
        voiced_ms: u64,
    },
    /// Audio worker: an event of the capture opened by `AudioCmd::Start { session }`. The
    /// coordinator drops it once a later capture started, so a stale `DeviceLost` cannot stop
    /// the next recording.
    Capture { session: u64, event: Event },
    /// Whisper worker: `Some` is the final text (empty when nothing is left to paste), `None`
    /// a failed transcription. `words` are whisper's words with their confidence, for the
    /// history entry (empty when `text` is `None`). `held` are the parts of the dictation the
    /// confidence filter held back: their text is not in `text`, the entry keeps it. When all
    /// of it was held, `text` is empty and `held` has everything.
    Transcribed {
        job: JobId,
        text: Option<String>,
        words: Vec<oxi_core::history::Word>,
        held: Vec<oxi_core::history::HeldPart>,
    },
    /// Whisper worker: why the model runs elsewhere than configured; `None` once it runs as
    /// configured again or no model is loaded.
    EngineWarning(Option<EngineWarning>),
    /// Whisper worker: how far the transcription of `job` is, 0 to 100.
    Progress { job: JobId, percent: u8 },
    /// IO worker: registry model `id` is on disk now; a load that failed for it may work.
    ModelDownloaded(String),
    /// Whisper worker: the device a model load ended up on (`LoadInfo.device`, `cpu` for none).
    Backend(String),
    /// Sound worker: why a sound did not play, opened slowly or is not used (a custom file that
    /// cannot be read), each with its kind; empty once every sound plays as configured.
    /// Dictation goes on with the default.
    SoundWarnings(Vec<SoundWarning>),
    /// History worker: why dictations are not saved (a write or clean-up error); `None` once a
    /// save or clean-up worked again. Never carries the text.
    HistoryWarning(Option<String>),
    /// Paste worker: how the to-file write of `job` went: why dictations do not reach the
    /// folder (the error kind, never the text), `None` when it worked (arch-plan 9.4). Each write
    /// answers once, a refused one at once, so the job says which write it is (d03-label).
    FileWarning { job: JobId, line: Option<String> },
    /// Watcher: a good `config.toml`, with the values the load corrected (`ConfigFile::warnings`).
    /// Held until the machine is Idle, then sent to the workers.
    Config {
        config: Box<Config>,
        warnings: Vec<String>,
    },
    /// Watcher: `config.toml` did not parse; the last good config stays active.
    ConfigError(ConfigError),
    /// Watcher: `dictionary.txt` or `replace.txt` changed.
    AssetsChanged,
    /// IPC server: a request only the coordinator can answer.
    Ipc(IpcAsk),
    /// Tray menu "Copy last text".
    CopyLastText,
    /// IPC `Shutdown` or tray Quit: the coordinator loop ends and drops every worker sender.
    Shutdown,
}

/// IPC requests that need coordinator state.
pub enum IpcAsk {
    /// `Status`: the coordinator answers once on `reply`.
    Status(Sender<StatusReport>),
    /// `PreviewOverlay { position }`: show the pill at `position` for 2 s.
    PreviewOverlay(OverlayPosition),
    /// `PreviewSound`: play `choice` once at `volume_percent`.
    PreviewSound { choice: String, volume_percent: u8 },
    /// `TestTranscribe`: refused while a dictation runs, else handed to the whisper worker.
    Test(TestJob),
    /// The settings connection that ran tests closed.
    TestDone,
    /// `DeleteModel { id }`: refused while a dictation or test runs, else the whisper worker
    /// unloads that model if it holds it and deletes its files; the answer goes out on `reply`.
    DeleteModel { id: String, reply: Pushes },
}

/// One recognition test run. `samples` live in memory only. No `Debug`, like every type that
/// carries speech.
pub struct TestJob {
    pub id: u32,
    pub set: SettingsSet,
    pub language: Language,
    pub samples: Vec<f32>,
    /// Where `samples` came from: a stored, processed recording is not processed again.
    pub origin: Origin,
    /// The worker queues its `TestResult` here and never waits for the window.
    pub reply: Pushes,
}

/// What the IPC `Status` push carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub phase: Phase,
    /// Active `[model] id`.
    pub model: String,
    /// Active `[audio] device`; empty is the system default.
    pub device: String,
    /// Active `[model] gpu`.
    pub gpu: DeviceChoice,
    /// Device of the last model load (`cpu` after a fallback); `none` before the first load.
    pub backend: String,
    /// Why the last model load failed; `None` once a load worked. The files alone cannot say:
    /// a file of the right size can still be corrupt.
    pub model_failure: Option<ModelFailure>,
    /// Load corrections of the active config, the engine warning, then the last parse error.
    pub warnings: Vec<String>,
    /// The sound worker's warnings, typed; their lines are in `warnings` too.
    pub sounds: Vec<SoundWarning>,
}

/// Commands for the audio worker.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCmd {
    /// Open the capture with the current `Audio` options; its events carry `session`. `pressed`
    /// is the key-down that started it, for the `capture started` latency (d11).
    Start {
        session: u64,
        pressed: Option<std::time::Instant>,
    },
    /// Stop, flush the tail, answer `Inbox::JobAudio` or `Event::JobAudioEmpty` for this job.
    Stop(JobId),
    /// Stop and throw the audio away; no answer.
    Discard,
    /// Options for the next `Start`.
    Config(Audio),
}

/// Commands for the whisper worker.
pub enum WhisperCmd {
    /// Load the model now (key-down), answer `ModelLoaded` or `ModelFailed`.
    Preload,
    /// Drop the context (idle unload, sleep).
    Unload,
    /// Delete the files of registry model `id` (unloading it first when it is the loaded one)
    /// and push `ModelDeleted` on `reply`.
    DeleteModel { id: String, reply: Pushes },
    /// Model for the next `Preload` (key-down, from the profile of the app in front).
    UseModel(String),
    /// Transcribe with the dictation's profile (language, model, text switches) and answer
    /// `Inbox::Transcribed`.
    Transcribe {
        job: JobId,
        samples: Vec<f32>,
        profile: Box<Profile>,
    },
    /// Recognition test: run one set, answer `Push::TestResult`, write nothing to disk. A run
    /// that a dictation overtook in the queue answers `Busy` when its turn comes.
    Test(TestJob),
    /// Tests are over: drop a model only a test loaded.
    TestDone,
    /// New active config: model, language, paste trailing space. A changed model reloads on the
    /// next `Preload`.
    Config(Box<Config>),
    /// Re-read `dictionary.txt` and `replace.txt`.
    ReloadAssets,
}

/// Commands for the sound worker. It answers `Inbox::SoundWarnings` for each `Config`.
#[derive(Debug, Clone, PartialEq)]
pub enum SoundCmd {
    /// The active `[sounds]`: loads the start and stop clips now, so a custom file is read once.
    Config(Sounds),
    /// A machine effect: plays the configured clip, or the fixed one for cancel and error. Silent
    /// while `enabled` is false.
    Play(Sound),
    /// Settings preview: plays `choice` at once, also when sounds are disabled.
    Preview { choice: String, volume_percent: u8 },
}

/// Where to-file mode writes one dictation (arch-plan 9.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToFile {
    pub folder: PathBuf,
    pub format: FileFormat,
    pub only: bool,
}

/// Commands for the paste worker.
pub enum PasteCmd {
    /// Deliver the text and answer `Event::PasteDone { job, outcome }`.
    Paste {
        job: JobId,
        text: String,
        cfg: Paste,
        /// To-file mode: also append the text to the daily file; with `only` nothing is pasted.
        file: Option<ToFile>,
        /// `paste.press_enter`: the focus at the record start of the dictation (its first part
        /// in a hold-gap continuation); `None` when the setting was off, the focus unknown or the
        /// session locked or slept since. `None` sends no Enter.
        focus_at_start: Option<Focus>,
    },
    /// "Copy last text": set the clipboard only, never send a chord, no answer.
    CopyOnly { text: String, in_history: bool },
    /// Quit: append the text of a dictation that is not pasted to its daily file, giving up
    /// at `until` (the quit grace); no answer.
    File {
        text: String,
        to: ToFile,
        until: std::time::Instant,
    },
}

/// Commands for the history worker. It answers `Inbox::HistoryWarning` when the outcome changes.
/// No `Debug`: `Save` carries the dictated text.
pub enum HistoryCmd {
    /// The active `[history]`: applies the retention limits to the folder now, saving on or off.
    Config(History),
    /// Save one dictation as `<stem>.wav` and `<stem>.txt` in the configured folder, then apply
    /// the retention limits. Nothing is written when `cfg.enabled` is false. `spool` is the job's
    /// streamed recording (`Inbox::JobAudio`): the entry takes its name, or it is deleted when
    /// saving is off. `carried` are the streamed recordings of the earlier parts a press inside
    /// the hold gap carried on into this dictation: deleted once its audio is written, kept as
    /// failed when the write fails. `words` go into the entry's json (whisper's words and their
    /// confidence, no times).
    Save {
        samples: Vec<f32>,
        text: String,
        words: Vec<oxi_core::history::Word>,
        /// Held-back parts of the dictation, kept in the entry's json.
        held: Vec<oxi_core::history::HeldPart>,
        /// `Some(Filtered)` when `text` is only the held parts and nothing was inserted.
        delivered: Option<oxi_core::history::Delivered>,
        cfg: History,
        spool: Option<PathBuf>,
        carried: Vec<PathBuf>,
    },
}

/// Commands for the io worker, sent by the IPC server; progress goes back over IPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoCmd {
    /// Download the registry model `id` into the models directory.
    Download { id: String },
    /// Stop that download; its `.part` stays for a resume.
    CancelDownload { id: String },
    /// A settings window connected: push the progress of every running download to it.
    Subscribed,
}

/// Commands for the main (UI) thread.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCmd {
    /// Show this overlay state, or hide it (`OverlayState::Hidden`).
    Overlay(OverlayState),
    /// Tray icon and tooltip for this phase; sent when the phase changes.
    Tray(Phase),
    /// The badge on the tray icon; sent when it changes.
    Badge(TrayBadge),
    /// OS notification (tray balloon).
    Notify(Notice),
    /// Show the pill at this position for 2 s (settings preview).
    PreviewOverlay(OverlayPosition),
    /// The active config parts the overlay and the tray read: the overlay options, the meter
    /// sensitivity and the accent.
    Config {
        overlay: Overlay,
        meter_sensitivity_db: f32,
        accent: Accent,
    },
    /// Status warnings for the tray tooltip; sent when they change.
    Warnings(Vec<String>),
    /// Enables the tray item "Copy last text"; sent once, when the first text exists.
    CanCopy,
}

/// The coordinator's calls into the hook thread, so tests can fake them. `run.rs` implements it
/// for the platform hook handle.
pub trait ChordHook {
    /// Is the chord physically held now; answers `Effect::PollChord`. Must not block.
    fn chord_held(&self) -> bool;
    /// Install the low-level hook again (resume, unlock).
    fn reinstall(&self);
    /// The active hotkey, at start and on every `ApplyConfig`.
    fn set_hotkey(&self, hotkey: &Hotkey);
    /// When the chord went down for the last `ChordDown`, once (the hook's key time).
    fn take_pressed(&self) -> Option<std::time::Instant> {
        oxi_platform::os::hook::take_chord_down()
    }
}

/// The coordinator's ends of the worker channels.
pub struct Workers {
    pub audio: Sender<AudioCmd>,
    pub whisper: Sender<WhisperCmd>,
    pub paste: Sender<PasteCmd>,
    pub sound: Sender<SoundCmd>,
    pub history: Sender<HistoryCmd>,
    pub ui: Sender<UiCmd>,
    /// Wakes the UI loop after each `UiCmd` (Windows: `PostMessage(WM_APP)`).
    pub wake_ui: Box<dyn Fn() + Send>,
    /// Whisper's percent for the overlay pill, which reads it each frame.
    pub progress: Progress,
}
