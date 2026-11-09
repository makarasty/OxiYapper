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
use crate::hotkey::Key;
use crate::machine::Phase;
use crate::stats::Summary;
use crate::tuning::SettingsSet;
use crate::types::Language;

/// Protocol version. `Hello`, `HelloAck` and `Shutdown` never change shape. 2: `Stats`. 3: `PreviewSound`.
/// 4: `TestAudio`, `TestTranscribe`.
pub const PROTO: u32 = 4;

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
    Shutdown,
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
