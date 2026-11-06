//! Messages between the coordinator and the daemon threads (ARCHITECTURE section 2, Channels).
//!
//! Wiring contract for the worker modules and `run.rs`:
//! - Every thread sends into one coordinator inbox, a cloned `mpsc::Sender<Inbox>`.
//! - The coordinator owns one `mpsc::Sender` per worker, bundled in [`Workers`]. Workers never
//!   call each other: audio hands its samples to the coordinator, which forwards them to whisper.
//! - A worker module exposes `pub fn spawn(rx: Receiver<XCmd>, inbox: Sender<Inbox>, ..) ->
//!   std::io::Result<JoinHandle<()>>` (a named `thread::Builder`). It handles commands in order
//!   and returns when `rx` closes, which happens when the coordinator ends. Extra arguments are
//!   the worker's own: audio the shared `LevelMeter`, whisper the `Layout`, io a sender for IPC
//!   pushes.
//! - What each worker answers: audio `Inbox::JobAudio`, or `Event::JobAudioEmpty`, for
//!   `Stop(job)`, plus `Inbox::Capture` with `Event::DeviceLost`, `CaptureFailed` or
//!   `MicFellBack`; whisper `Event::ModelLoaded` or `ModelFailed` for
//!   `Preload` and `Inbox::Transcribed` for `Transcribe`; paste `Event::PasteDone` for `Paste`
//!   and nothing for `CopyOnly`.
//! - The hook thread sends the machine's `Event` on its own `Sender<Event>`; `run.rs` forwards
//!   that channel into the inbox as [`Inbox::Event`]. The coordinator calls the hook only
//!   through [`ChordHook`].
//! - The UI thread drains `Receiver<UiCmd>` each time [`Workers::wake_ui`] wakes its loop.
//!
//! Types that carry dictated text have no `Debug`, so the text cannot reach a log by `{:?}`.

#![allow(
    dead_code,
    reason = "the workers and run.rs (t28a-t40) use these; until then only the coordinator tests do"
)]

use std::sync::mpsc::Sender;

use oxi_core::config::{Audio, Config, ConfigError, Hotkey, Overlay, OverlayPosition, Paste};
use oxi_core::machine::{Event, JobId, Notice, OverlayState, Phase};

/// Everything the coordinator hears, on one channel with many senders.
pub enum Inbox {
    /// Hook, worker and session events, fed straight into the state machine.
    Event(Event),
    /// Audio worker: the trimmed 16 kHz mono samples of a stopped job, never empty.
    JobAudio { job: JobId, samples: Vec<f32> },
    /// Audio worker: an event of the capture opened by `AudioCmd::Start { session }`. The
    /// coordinator drops it once a later capture started, so a stale `DeviceLost` cannot stop
    /// the next recording.
    Capture { session: u64, event: Event },
    /// Whisper worker: `Some` is the final text (empty when nothing is left to paste), `None`
    /// a failed transcription.
    Transcribed { job: JobId, text: Option<String> },
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
}

/// The coordinator's part of the IPC `Status` push; the IPC server adds the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub phase: Phase,
    /// Active `[model] id`.
    pub model: String,
    /// Active `[audio] device`; empty is the system default.
    pub device: String,
    /// Load corrections of the active config, then the last parse error if any.
    pub warnings: Vec<String>,
}

/// Commands for the audio worker.
#[derive(Debug, Clone, PartialEq)]
pub enum AudioCmd {
    /// Open the capture with the current `Audio` options; its events carry `session`.
    Start { session: u64 },
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
    /// Transcribe and answer `Inbox::Transcribed`.
    Transcribe { job: JobId, samples: Vec<f32> },
    /// New active config: model, language, paste trailing space. A changed model reloads on the
    /// next `Preload`.
    Config(Box<Config>),
    /// Re-read `dictionary.txt` and `replace.txt`.
    ReloadAssets,
}

/// Commands for the paste worker.
pub enum PasteCmd {
    /// Deliver the text and answer `Event::PasteDone { job, outcome }`.
    Paste {
        job: JobId,
        text: String,
        cfg: Paste,
    },
    /// "Copy last text": set the clipboard only, never send a chord, no answer.
    CopyOnly(String),
}

/// Commands for the io worker, sent by the IPC server; progress goes back over IPC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IoCmd {
    /// Download the registry model `id` into the models directory.
    Download { id: String },
    /// Stop that download; its `.part` stays for a resume.
    CancelDownload { id: String },
}

/// Commands for the main (UI) thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiCmd {
    /// Show this overlay state, or hide it (`OverlayState::Hidden`).
    Overlay(OverlayState),
    /// Tray icon and tooltip for this phase; sent when the phase changes.
    Tray(Phase),
    /// OS notification (tray balloon).
    Notify(Notice),
    /// Show the pill at this position for 2 s (settings preview).
    PreviewOverlay(OverlayPosition),
    /// Active overlay options: enabled, position, hide over fullscreen.
    Config(Overlay),
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
}

/// The coordinator's ends of the worker channels.
pub struct Workers {
    pub audio: Sender<AudioCmd>,
    pub whisper: Sender<WhisperCmd>,
    pub paste: Sender<PasteCmd>,
    pub ui: Sender<UiCmd>,
    /// Wakes the UI loop after each `UiCmd` (Windows: `PostMessage(WM_APP)`).
    pub wake_ui: Box<dyn Fn() + Send>,
}
