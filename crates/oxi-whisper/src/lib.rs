//! whisper.cpp behind one crate, so the GPU features and the slow C++ build stay
//! out of everything else. See docs/ARCHITECTURE.md, "Model lifecycle".
//!
//! One [`Engine`] is one loaded model. Dropping it is the unload.

mod device;

use std::path::Path;

use oxi_core::types::{Language, Millis};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState,
};

pub use device::{Device, DeviceChoice, Picked, list_devices, pick};

/// whisper.cpp build and CPU/GPU capability line, for `--version` style output.
pub fn system_info() -> &'static str {
    whisper_rs::print_system_info()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The model file could not be loaded (also after the CPU retry).
    #[error("cannot load whisper model: {0}")]
    Load(#[source] WhisperError),
    /// Decoding or tokenizing failed.
    #[error("whisper failed: {0}")]
    Run(#[source] WhisperError),
}

/// What a load ended up with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadInfo {
    /// ggml device name whisper runs on, `None` = CPU. `None` after a GPU choice is
    /// the CPU fallback: per ARCHITECTURE 8 the caller then loads with
    /// [`DeviceChoice::Cpu`] until the config or the model changes.
    pub device: Option<String>,
    /// Tray text: the named GPU was gone, or GPU init failed and CPU runs instead.
    pub warning: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub text: String,
    pub t0: Millis,
    pub t1: Millis,
    pub no_speech_prob: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Transcript {
    pub segments: Vec<Segment>,
}

pub struct Engine {
    ctx: WhisperContext,
    /// Made once per load and reused by every job.
    state: WhisperState,
    info: LoadInfo,
}

fn open(path: &Path, gpu: Option<usize>) -> Result<(WhisperContext, WhisperState), WhisperError> {
    let mut params = WhisperContextParameters::new();
    params.use_gpu(gpu.is_some());
    if let Some(i) = gpu {
        params.gpu_device(i32::try_from(i).unwrap_or(0));
    }
    let ctx = WhisperContext::new_with_params(path, params)?;
    // The GPU backend, KV cache and compute buffers come up here, not in new_with_params.
    let state = ctx.create_state()?;
    Ok((ctx, state))
}

impl Engine {
    /// Load a model on the chosen device. A GPU failure (backend start, weights,
    /// KV cache or compute buffers) retries once on CPU; see [`LoadInfo::device`].
    pub fn load(path: &Path, choice: &DeviceChoice) -> Result<Self, Error> {
        let devices = list_devices();
        let picked = pick(&devices, choice);
        let gpu_err = match picked.index {
            None => {
                let (ctx, state) = open(path, None).map_err(Error::Load)?;
                let info = LoadInfo {
                    device: None,
                    warning: picked.warning,
                };
                return Ok(Self { ctx, state, info });
            }
            Some(i) if !device::gpu_starts(i) => format!("cannot start {}", devices[i].name),
            Some(i) => match open(path, Some(i)) {
                Ok((ctx, state)) => {
                    let info = LoadInfo {
                        device: Some(devices[i].name.clone()),
                        warning: picked.warning,
                    };
                    return Ok(Self { ctx, state, info });
                }
                Err(e) => e.to_string(),
            },
        };
        let (ctx, state) = open(path, None).map_err(Error::Load)?;
        let info = LoadInfo {
            device: None,
            warning: Some(format!("running on CPU: {gpu_err}")),
        };
        Ok(Self { ctx, state, info })
    }

    pub fn info(&self) -> &LoadInfo {
        &self.info
    }

    /// Transcribe 16 kHz mono f32 samples. `prompt` may be empty.
    pub fn transcribe(
        &mut self,
        audio: &[f32],
        language: Language,
        prompt: &str,
    ) -> Result<Transcript, Error> {
        let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        params.set_language(Some(match language {
            Language::Ru => "ru",
            Language::En => "en",
            Language::Auto => "auto",
        }));
        // set_initial_prompt panics on NUL.
        let prompt = prompt.replace('\0', "");
        if !prompt.is_empty() {
            params.set_initial_prompt(&prompt);
        }
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        self.state.full(params, audio).map_err(Error::Run)?;
        let segments = self
            .state
            .as_iter()
            .map(|s| {
                Ok(Segment {
                    text: s.to_str_lossy()?.into_owned(),
                    t0: centis(s.start_timestamp()),
                    t1: centis(s.end_timestamp()),
                    no_speech_prob: s.no_speech_probability(),
                })
            })
            .collect::<Result<_, WhisperError>>()
            .map_err(Error::Run)?;
        Ok(Transcript { segments })
    }

    /// Tokens `text` takes in the model's vocabulary, as prompt budgeting needs.
    pub fn token_count(&self, text: &str) -> Result<usize, Error> {
        // BPE never yields more tokens than bytes.
        self.ctx
            .tokenize(text, text.len() + 1)
            .map(|t| t.len())
            .map_err(Error::Run)
    }
}

/// whisper.cpp timestamps are in 10 ms units.
fn centis(t: i64) -> Millis {
    Millis(u64::try_from(t).unwrap_or(0) * 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centis_to_millis() {
        assert_eq!(centis(150), Millis(1500));
        assert_eq!(centis(-3), Millis(0));
    }
}
