//! IPC messages between the daemon and the settings window (ARCHITECTURE section 5).
//!
//! Framing: one JSON object per line, tagged `{"t": "..."}`, at most [`MAX_LINE`] bytes.
//! This module is transport-free: the daemon and settings feed bytes to a [`Decoder`] and
//! write what [`encode`] returns. Adding an optional field (`#[serde(default)]`) keeps
//! [`PROTO`]; renaming, removing or changing the meaning of a field bumps it.

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::APP_NAME;
use crate::config::OverlayPosition;
use crate::device::{Device, DeviceChoice};
use crate::history::Status as HistoryStatus;
use crate::hotkey::Key;
use crate::machine::{Phase, Sound};
use crate::models::ModelStatus;
use crate::stats::Summary;
use crate::strings;
use crate::tuning::SettingsSet;
use crate::types::Language;

/// Protocol version. `Hello`, `HelloAck` and `Shutdown` never change shape. 2: `Stats`. 3: `PreviewSound`.
/// 4: `TestAudio`, `TestTranscribe`. 5: `Attention`, the history requests, `DeleteModel`,
/// `TestProgress`.
pub const PROTO: u32 = 5;

/// Longest accepted line, newline excluded.
pub const MAX_LINE: usize = 1024 * 1024;

/// Settings to daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Request {
    Hello {
        proto: u32,
        version: String,
    },
    /// Also sent by a second daemon launch.
    OpenSettings,
    Status,
    ListDevices,
    /// `id` is a `ModelEntry::id`.
    DownloadModel {
        id: String,
    },
    CancelDownload {
        id: String,
    },
    CaptureHotkey {
        on: bool,
    },
    PreviewOverlay {
        position: OverlayPosition,
    },
    /// The Home page numbers; answered with [`Push::Stats`].
    Stats,
    /// Play `choice` (a `sounds.start` value) once at `volume_percent`, 0 to 100, as the sliders
    /// stand, before anything is saved. Whole percent keeps the message `Eq`.
    PreviewSound {
        choice: String,
        volume_percent: u8,
    },
    /// Recognition test: one piece of the recording this connection holds, in memory only and
    /// gone when the connection closes. `pcm` is `tuning::encode_pcm` of raw 16 kHz mono
    /// samples, before gain, normalise, noise suppression and trim; `reset` starts a new
    /// recording. Pieces of at most `tuning::PCM_CHUNK_SAMPLES`.
    TestAudio {
        reset: bool,
        pcm: String,
    },
    /// Recognition test: run the held recording through `set` in `language`. Answered with
    /// [`Push::TestResult`] carrying `id`; send the next set after that answer, so a dictation
    /// waits for one run at most.
    TestTranscribe {
        id: u32,
        set: SettingsSet,
        language: Language,
    },
    /// The saved recordings, newest first, answered with [`Push::History`]. `older_than` is the
    /// `id` of the last item already held (the next page); `None` starts at the newest.
    HistoryList {
        older_than: Option<String>,
    },
    /// Transcribe recording `id` again with `set` in `language`. Answered like a
    /// [`Request::TestTranscribe`], with [`Push::TestResult`] carrying `run`, and it takes the
    /// same one-run-at-a-time slot. The result is stored only by [`Request::HistorySetText`].
    HistoryRetranscribe {
        run: u32,
        id: String,
        set: SettingsSet,
        language: Language,
    },
    /// Store `text` as the transcript of recording `id`, made by `set` in `language`. Answered
    /// with [`Push::HistoryResult`].
    HistorySetText {
        id: String,
        text: Spoken,
        set: SettingsSet,
        language: Language,
    },
    /// Replace the transcript of recording `id` with `text` the user edited; the run that made
    /// it (model, language, settings) stays. The old text is kept as the other version. Answered
    /// with [`Push::HistoryResult`] (`Edit`).
    HistoryEdit {
        id: String,
        text: Spoken,
    },
    /// Swap the transcript of recording `id` with its other version (Restore old, Use new); the
    /// swap undoes itself. Answered with [`Push::HistoryResult`] (`Restore`), `ok` false when no
    /// other version is kept.
    HistoryRestore {
        id: String,
    },
    /// Delete recording `id` (audio, text and meta). Answered with [`Push::HistoryResult`].
    HistoryDelete {
        id: String,
    },
    /// Delete the downloaded model `id` and any partial download of it. Answered with
    /// [`Push::ModelDeleted`].
    DeleteModel {
        id: String,
    },
    Shutdown,
}

/// Dictated text on the wire. `Debug` hides it, so a log line or a failed assert never prints it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Spoken(pub String);

impl std::fmt::Debug for Spoken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spoken({} bytes)", self.0.len())
    }
}

/// One saved recording as the History page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryItem {
    /// `YYYY-MM-DD/HHMMSS-mmm`, the id every history request takes.
    pub id: String,
    pub status: HistoryStatus,
    pub duration_ms: u64,
    /// Registry id of the model that made the text; empty when unknown.
    pub model: String,
    /// `auto` or a language code; empty when unknown.
    pub language: String,
    /// Empty for a cancelled, interrupted or failed recording.
    pub text: Spoken,
}

/// What a history request changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HistoryOp {
    SetText,
    Edit,
    Restore,
    Delete,
}

/// How a model delete ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "r")]
pub enum DeleteResult {
    /// The files are gone; `freed` bytes.
    Done {
        freed: u64,
    },
    /// The model is the one a dictation or test is using now.
    InUse,
    Failed {
        message: String,
    },
}

/// What needs the user's attention, as the daemon sees it. The window shows the first of them
/// ([`attention`] orders them) in its status chip; [`Attention::target`] is the setting that
/// fixes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "k")]
pub enum Attention {
    /// The chosen model is not downloaded.
    NoModel,
    /// The model file has the wrong size, or failed its check or its load.
    ModelDamaged,
    /// The chosen microphone is not plugged in, or there is none.
    NoMicrophone,
    /// The model runs on the CPU although a GPU is chosen; `reason` is the engine's own line.
    CpuFallback { reason: String },
    /// The model did not fit in memory. `cpu`: even the CPU load failed, so there is no model;
    /// else the GPU load failed and the model runs on the CPU.
    OutOfMemory { cpu: bool },
    /// A sound did not play, or a configured sound file is not used; `line` says why. `stop`:
    /// the line is about the stop sound.
    SoundFailed { line: String, stop: bool },
}

/// A page of the settings window that fixes an [`Attention`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsPage {
    Model,
    Audio,
}

/// A tab inside a [`SettingsPage`], in the order of the page's tabs (`pages/audio.slint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsTab {
    Microphone,
    Recording,
    StartSound,
    StopSound,
}

/// Where a click on the status chip goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub page: SettingsPage,
    pub tab: Option<SettingsTab>,
}

impl Attention {
    /// The page and tab that fix it.
    #[must_use]
    pub const fn target(&self) -> Target {
        let (page, tab) = match self {
            Self::NoModel
            | Self::ModelDamaged
            | Self::CpuFallback { .. }
            | Self::OutOfMemory { .. } => (SettingsPage::Model, None),
            Self::NoMicrophone => (SettingsPage::Audio, Some(SettingsTab::Microphone)),
            Self::SoundFailed { stop: true, .. } => {
                (SettingsPage::Audio, Some(SettingsTab::StopSound))
            }
            // shortcut: the output and the done, cancel and error sounds have no tab of their
            // own yet; point them at their own tab once the Audio page has one.
            Self::SoundFailed { stop: false, .. } => {
                (SettingsPage::Audio, Some(SettingsTab::StartSound))
            }
        };
        Target { page, tab }
    }
}

/// What a sound worker warning is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundWarningKind {
    /// No sound plays: the output did not open, or it stopped.
    NotPlayed,
    /// The output was slow to open; the sound played, late.
    SlowOpen,
    /// The previewed sound cannot be used.
    Preview,
    /// Config correction: the file or id set for this sound is not used, its default plays.
    Unused(Sound),
}

/// One sound worker warning: its kind, and the line the tray and `Status` show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundWarning {
    pub kind: SoundWarningKind,
    pub line: String,
}

/// What the daemon looked at to decide its [`Attention`]s.
#[derive(Debug, Clone, Copy, Default)]
pub struct Observed<'a> {
    /// The chosen registry model's files; `None` for a custom file or when unchecked.
    pub model: Option<ModelStatus>,
    /// The chosen microphone is not among the input devices (or there are none).
    pub mic_missing: bool,
    /// The model failed to load on the CPU for lack of memory.
    pub load_oom_on_cpu: bool,
    /// The status warning lines (`StatusReport::warnings`).
    pub warnings: &'a [String],
    /// The sound worker's warnings.
    pub sounds: &'a [SoundWarning],
}

/// Every condition that needs the user, most blocking first: no usable model, then memory,
/// microphone, CPU fallback, sounds.
#[must_use]
pub fn attention(seen: &Observed) -> Vec<Attention> {
    let mut out = Vec::new();
    match seen.model {
        Some(ModelStatus::Missing | ModelStatus::Partial) => out.push(Attention::NoModel),
        Some(ModelStatus::Bad) => out.push(Attention::ModelDamaged),
        Some(ModelStatus::Ready | ModelStatus::NeedsHash) | None => {}
    }
    let cpu_reason = seen
        .warnings
        .iter()
        .find_map(|line| strings::cpu_fallback_reason(line));
    let gpu_oom = cpu_reason.is_some_and(strings::is_out_of_memory);
    if seen.load_oom_on_cpu {
        out.push(Attention::OutOfMemory { cpu: true });
    } else if gpu_oom {
        out.push(Attention::OutOfMemory { cpu: false });
    }
    if seen.mic_missing {
        out.push(Attention::NoMicrophone);
    }
    if let Some(reason) = cpu_reason
        && !gpu_oom
    {
        out.push(Attention::CpuFallback {
            reason: reason.to_owned(),
        });
    }
    // A slow open still played the sound.
    if let Some(w) = seen
        .sounds
        .iter()
        .find(|w| w.kind != SoundWarningKind::SlowOpen)
    {
        out.push(Attention::SoundFailed {
            line: w.line.clone(),
            stop: w.kind == SoundWarningKind::Unused(Sound::Stop),
        });
    }
    out
}

/// How a download ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "r")]
pub enum DownloadResult {
    Ok,
    Cancelled,
    Failed { message: String },
}

/// Why a recognition test run has no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TestFailure {
    /// No recording held (never sent, or refused as bad or too long).
    NoAudio,
    /// A dictation is running, or this window's previous run has not answered yet.
    Busy,
    /// The model is not downloaded.
    ModelMissing,
    /// The model file failed its check or would not load.
    ModelBad,
    /// Whisper or the audio steps failed.
    Failed,
}

/// How a recognition test run ended.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "r")]
pub enum TestOutcome {
    /// `text` is what a dictation would paste, without the trailing space; empty when nothing
    /// was heard. `ms` is the whisper run alone, model load excluded.
    Heard {
        text: String,
        ms: u64,
    },
    Failed {
        why: TestFailure,
    },
}

// By hand: the text is dictated speech and must not reach a log through `{:?}`.
impl std::fmt::Debug for TestOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Heard { text, ms } => write!(f, "Heard({} bytes, {ms} ms)", text.len()),
            Self::Failed { why } => write!(f, "Failed({why:?})"),
        }
    }
}

/// Daemon to settings: replies and pushes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Push {
    HelloAck {
        proto: u32,
        version: String,
    },
    /// Bring the open settings window to the front.
    Raise,
    Status {
        phase: Phase,
        /// `ModelEntry::id`.
        model: String,
        device: DeviceChoice,
        /// Backend in use, e.g. `Vulkan0` or `cpu`.
        backend: String,
        warnings: Vec<String>,
    },
    Devices {
        /// Input device names.
        mics: Vec<String>,
        gpus: Vec<Device>,
    },
    DownloadProgress {
        id: String,
        done: u64,
        total: u64,
    },
    DownloadDone {
        id: String,
        result: DownloadResult,
    },
    /// While `CaptureHotkey { on: true }`.
    Key {
        code: Key,
        down: bool,
    },
    /// A reload of `file` failed; `line` is 1-based.
    ConfigError {
        file: String,
        line: usize,
        message: String,
    },
    /// Counts only, never text (SPEC, History).
    Stats(Summary),
    /// Answer to [`Request::TestTranscribe`] with the same `id`.
    TestResult {
        id: u32,
        outcome: TestOutcome,
    },
    /// Whisper's progress of the run `id` (a test or a history run), 0 to 100; sent while it
    /// runs, before its `TestResult`.
    TestProgress {
        id: u32,
        percent: u8,
    },
    /// Everything that needs the user, sent after each `Status` answer; empty when all is well.
    Attention {
        items: Vec<Attention>,
    },
    /// Answer to [`Request::HistoryList`]: one page, newest first. `more`: older ones exist.
    History {
        items: Vec<HistoryItem>,
        more: bool,
    },
    /// Answer to [`Request::HistorySetText`], [`Request::HistoryEdit`], [`Request::HistoryRestore`]
    /// and [`Request::HistoryDelete`].
    HistoryResult {
        id: String,
        op: HistoryOp,
        ok: bool,
    },
    /// Answer to [`Request::DeleteModel`].
    ModelDeleted {
        id: String,
        result: DeleteResult,
    },
}

/// Windows named pipe for one user: `\\.\pipe\OxiYapper-<SID>`.
pub fn pipe_name(user_sid: &str) -> String {
    format!(r"\\.\pipe\{APP_NAME}-{user_sid}")
}

/// File name of the Unix socket (`OxiYapper.sock`); the platform picks its 0700 directory.
pub fn socket_file() -> String {
    format!("{APP_NAME}.sock")
}

/// One framed line: compact JSON and a closing `\n`.
pub fn encode<T: Serialize>(msg: &T) -> String {
    // Compact JSON has no raw newline, so the frame holds exactly one line.
    let mut line = serde_json::to_string(msg).unwrap_or_else(|e| {
        // Derived Serialize on plain data cannot fail; keep a valid frame if it ever does.
        format!(r#"{{"t":"EncodeError","kind":"{:?}"}}"#, e.classify())
    });
    line.push('\n');
    line
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("line longer than {MAX_LINE} bytes")]
    TooLong,
    #[error("bad message: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<DecodeError> for io::Error {
    fn from(e: DecodeError) -> Self {
        io::Error::new(io::ErrorKind::InvalidData, e)
    }
}

/// Splits a byte stream into lines and decodes them. Feed it with [`Decoder::push`], then
/// call [`Decoder::decode`] until it returns `None`.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
    /// Inside an oversized line: drop bytes up to its newline.
    skipping: bool,
}

impl Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The next complete message. `None` means more bytes are needed. A bad line is an error
    /// and the decoder stays usable for the line after it.
    pub fn decode<T: DeserializeOwned>(&mut self) -> Option<Result<T, DecodeError>> {
        loop {
            let Some(end) = self.buf.iter().position(|&b| b == b'\n') else {
                // A full-length line may end in the `\r` of a CRLF whose `\n` is still to come.
                if self.buf.len() <= MAX_LINE
                    || (self.buf.len() == MAX_LINE + 1 && self.buf.last() == Some(&b'\r'))
                {
                    return None;
                }
                // No newline and already too long: refuse now, never buffer unbounded.
                self.buf.clear();
                return (!std::mem::replace(&mut self.skipping, true))
                    .then_some(Err(DecodeError::TooLong));
            };
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            if std::mem::take(&mut self.skipping) {
                continue;
            }
            let line = &line[..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if line.len() > MAX_LINE {
                return Some(Err(DecodeError::TooLong));
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return Some(serde_json::from_slice(line).map_err(DecodeError::from));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(warnings: &[String]) -> Observed<'_> {
        Observed {
            warnings,
            ..Observed::default()
        }
    }

    fn sounds(sounds: &[SoundWarning]) -> Observed<'_> {
        Observed {
            sounds,
            ..Observed::default()
        }
    }

    fn engine(reason: &str) -> Vec<String> {
        vec![strings::engine_warning(
            &crate::machine::EngineWarning::CpuFallback {
                reason: reason.into(),
            },
        )]
    }

    #[test]
    fn nothing_wrong_needs_no_attention() {
        let ok = Observed {
            model: Some(ModelStatus::Ready),
            ..Observed::default()
        };
        assert_eq!(attention(&ok), []);
        assert_eq!(attention(&Observed::default()), []);
        let hashed = Observed {
            model: Some(ModelStatus::NeedsHash),
            ..Observed::default()
        };
        assert_eq!(attention(&hashed), []);
    }

    type Row<'a> = (Observed<'a>, Attention, SettingsPage, Option<SettingsTab>);

    fn check_rows(table: Vec<Row<'_>>) {
        for (observed, want, page, tab) in table {
            assert_eq!(
                attention(&observed),
                std::slice::from_ref(&want),
                "{want:?}"
            );
            assert_eq!(want.target(), Target { page, tab }, "{want:?}");
        }
    }

    #[test]
    fn each_model_mic_and_engine_condition_maps_to_its_status_and_page() {
        let model = |m| Observed {
            model: Some(m),
            ..Observed::default()
        };
        let gpu_failed = engine("device lost");
        let gpu_oom = engine("out of memory");
        check_rows(vec![
            (
                model(ModelStatus::Missing),
                Attention::NoModel,
                SettingsPage::Model,
                None,
            ),
            (
                model(ModelStatus::Partial),
                Attention::NoModel,
                SettingsPage::Model,
                None,
            ),
            (
                model(ModelStatus::Bad),
                Attention::ModelDamaged,
                SettingsPage::Model,
                None,
            ),
            (
                Observed {
                    mic_missing: true,
                    ..Observed::default()
                },
                Attention::NoMicrophone,
                SettingsPage::Audio,
                Some(SettingsTab::Microphone),
            ),
            (
                seen(&gpu_failed),
                Attention::CpuFallback {
                    reason: "device lost".into(),
                },
                SettingsPage::Model,
                None,
            ),
            (
                seen(&gpu_oom),
                Attention::OutOfMemory { cpu: false },
                SettingsPage::Model,
                None,
            ),
            (
                Observed {
                    load_oom_on_cpu: true,
                    ..Observed::default()
                },
                Attention::OutOfMemory { cpu: true },
                SettingsPage::Model,
                None,
            ),
        ]);
    }

    #[test]
    fn each_sound_condition_maps_to_its_status_and_page() {
        let warn = |kind, line: &str| SoundWarning {
            kind,
            line: line.into(),
        };
        let sound = [warn(
            SoundWarningKind::NotPlayed,
            "sounds: not played (no output device)",
        )];
        let stop = [
            warn(
                SoundWarningKind::SlowOpen,
                "sounds: the output took 900 ms to open",
            ),
            warn(
                SoundWarningKind::Unused(Sound::Stop),
                "sounds.stop: cannot read the file (entity not found), using fall",
            ),
        ];
        let preview = [warn(SoundWarningKind::Preview, "sounds.preview: bad")];
        let slow = [stop[0].clone()];
        // A slow open played the sound: nothing to fix.
        assert_eq!(attention(&sounds(&slow)), []);
        check_rows(vec![
            (
                sounds(&sound),
                Attention::SoundFailed {
                    line: sound[0].line.clone(),
                    stop: false,
                },
                SettingsPage::Audio,
                Some(SettingsTab::StartSound),
            ),
            (
                sounds(&preview),
                Attention::SoundFailed {
                    line: preview[0].line.clone(),
                    stop: false,
                },
                SettingsPage::Audio,
                Some(SettingsTab::StartSound),
            ),
            (
                sounds(&stop),
                Attention::SoundFailed {
                    line: stop[1].line.clone(),
                    stop: true,
                },
                SettingsPage::Audio,
                Some(SettingsTab::StopSound),
            ),
        ]);
    }

    #[test]
    fn gpu_out_of_memory_is_not_also_a_plain_cpu_fallback() {
        let warnings = engine("Out of memory");
        assert_eq!(
            attention(&seen(&warnings)),
            [Attention::OutOfMemory { cpu: false }]
        );
    }

    #[test]
    fn the_most_blocking_condition_comes_first() {
        let warnings = vec![
            "config value adjusted".to_owned(),
            engine("device lost").remove(0),
            "sounds.start: bad, using rise".to_owned(),
        ];
        let sounds = [SoundWarning {
            kind: SoundWarningKind::Unused(Sound::Start),
            line: warnings[2].clone(),
        }];
        let all = Observed {
            model: Some(ModelStatus::Bad),
            mic_missing: true,
            load_oom_on_cpu: false,
            warnings: &warnings,
            sounds: &sounds,
        };
        let kinds: Vec<_> = attention(&all).iter().map(std::mem::discriminant).collect();
        let want = [
            Attention::ModelDamaged,
            Attention::NoMicrophone,
            Attention::CpuFallback {
                reason: String::new(),
            },
            Attention::SoundFailed {
                line: String::new(),
                stop: false,
            },
        ]
        .map(|a| std::mem::discriminant(&a));
        assert_eq!(kinds, want);
    }

    #[test]
    fn the_new_messages_survive_the_wire_and_hide_the_text() {
        let set = SettingsSet::defaults();
        let requests = vec![
            Request::HistoryList { older_than: None },
            Request::HistoryList {
                older_than: Some("2026-11-10/101500-123".into()),
            },
            Request::HistoryRetranscribe {
                run: 4,
                id: "2026-11-10/101500-123".into(),
                set: set.clone(),
                language: Language::RU,
            },
            Request::HistorySetText {
                id: "2026-11-10/101500-123".into(),
                text: Spoken("secret words".into()),
                set,
                language: Language::RU,
            },
            Request::HistoryEdit {
                id: "2026-11-10/101500-123".into(),
                text: Spoken("secret words".into()),
            },
            Request::HistoryRestore {
                id: "2026-11-10/101500-123".into(),
            },
            Request::HistoryDelete {
                id: "2026-11-10/101500-123".into(),
            },
            Request::DeleteModel { id: "turbo".into() },
        ];
        for request in requests {
            let mut d = Decoder::new();
            d.push(encode(&request).as_bytes());
            assert_eq!(d.decode::<Request>().unwrap().unwrap(), request);
            assert!(!format!("{request:?}").contains("secret"));
        }
        let pushes = vec![
            Push::TestProgress { id: 4, percent: 50 },
            Push::Attention {
                items: vec![Attention::NoModel, Attention::OutOfMemory { cpu: true }],
            },
            Push::History {
                items: vec![HistoryItem {
                    id: "2026-11-10/101500-123".into(),
                    status: HistoryStatus::Done,
                    duration_ms: 1500,
                    model: "turbo".into(),
                    language: "auto".into(),
                    text: Spoken("secret words".into()),
                }],
                more: true,
            },
            Push::HistoryResult {
                id: "2026-11-10/101500-123".into(),
                op: HistoryOp::Delete,
                ok: true,
            },
            Push::ModelDeleted {
                id: "turbo".into(),
                result: DeleteResult::Done { freed: 5 },
            },
            Push::ModelDeleted {
                id: "turbo".into(),
                result: DeleteResult::InUse,
            },
        ];
        for push in pushes {
            let mut d = Decoder::new();
            d.push(encode(&push).as_bytes());
            assert_eq!(d.decode::<Push>().unwrap().unwrap(), push);
            assert!(!format!("{push:?}").contains("secret"));
        }
    }
}
