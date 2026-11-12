//! whisper.cpp behind one crate, so the GPU features and the slow C++ build stay
//! out of everything else. See docs/ARCHITECTURE.md, "Model lifecycle".
//!
//! One [`Engine`] is one loaded model. Dropping it is the unload.

mod device;

use std::ffi::{CStr, c_char, c_int, c_void};
use std::io::Write;
use std::path::Path;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use oxi_core::config::Advanced;
use oxi_core::device::{DeviceChoice, Picked, pick};
use oxi_core::machine::EngineWarning;
use oxi_core::tuning::{self, RunParams, Sampling};
use oxi_core::types::{Language, Millis};
use whisper_rs::whisper_rs_sys as sys;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
    WhisperState,
};

pub use device::list_devices;

/// whisper.cpp build and CPU/GPU capability line, for `--version` style output.
pub fn system_info() -> &'static str {
    whisper_rs::print_system_info()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The model file could not be loaded (also after the CPU retry).
    #[error("cannot load whisper model")]
    Load(#[source] WhisperError),
    /// The load failed for lack of memory (whisper.cpp said so while loading).
    #[error("out of memory")]
    OutOfMemory,
    /// Decoding or tokenizing failed.
    #[error("whisper failed")]
    Run(#[source] WhisperError),
    /// The caller's cancel flag stopped the run.
    #[error("cancelled")]
    Cancelled,
}

/// A cancel check for [`Engine::transcribe_with`].
pub type Cancel = dyn Fn() -> bool + Sync;

/// Set by the log callback when whisper.cpp or ggml reports a failed allocation.
static ALLOC_FAILED: AtomicBool = AtomicBool::new(false);

/// Does this whisper.cpp/ggml log line say an allocation failed.
fn is_alloc_failure(line: &str) -> bool {
    let line = line.to_lowercase();
    [
        "failed to allocate",
        "out of memory",
        "unable to allocate",
        "bad_alloc",
    ]
    .iter()
    .any(|needle| line.contains(needle))
}

unsafe extern "C" fn on_log(level: sys::ggml_log_level, text: *const c_char, _user: *mut c_void) {
    if text.is_null() {
        return;
    }
    // SAFETY: whisper.cpp passes a NUL-terminated line.
    let line = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    if is_alloc_failure(&line) {
        ALLOC_FAILED.store(true, Ordering::Relaxed);
    }
    echo(&mut std::io::stderr(), level, &line);
}

/// What whisper.cpp's own default callback does: the line to `out`, DEBUG lines skipped. A write
/// error is ignored: `eprint!` panics on a closed stderr, and a panic here aborts the daemon.
fn echo(out: &mut impl Write, level: sys::ggml_log_level, line: &str) {
    if level != sys::ggml_log_level_GGML_LOG_LEVEL_DEBUG {
        let _ = out.write_all(line.as_bytes());
    }
}

/// Hooks the whisper.cpp log once; only loads read what it saw (one load at a time).
fn watch_logs() {
    static ONCE: Once = Once::new();
    // SAFETY: `on_log` is a valid callback for the life of the process; it ignores `user`.
    ONCE.call_once(|| unsafe { sys::whisper_log_set(Some(on_log), std::ptr::null_mut()) });
}

/// The error of a failed load: the memory one when the log said so.
fn load_error(e: WhisperError) -> Error {
    if ALLOC_FAILED.swap(false, Ordering::Relaxed) {
        Error::OutOfMemory
    } else {
        Error::Load(e)
    }
}

/// What a load ended up with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadInfo {
    /// ggml device name whisper runs on, `None` = CPU. `None` after a GPU choice is
    /// the CPU fallback: per ARCHITECTURE 8 the caller then loads with
    /// [`DeviceChoice::Cpu`] until the config or the model changes.
    pub device: Option<String>,
    /// The named GPU was gone, or GPU init failed and CPU runs instead.
    pub warning: Option<EngineWarning>,
}

/// The named GPU was not in the list and `pick` chose automatically.
fn not_found(choice: &DeviceChoice, picked: &Picked) -> Option<EngineWarning> {
    match choice {
        DeviceChoice::Named(name) if picked.missing => {
            Some(EngineWarning::GpuNotFound { name: name.clone() })
        }
        _ => None,
    }
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
    /// Per-run parameters, resolved for this load's device; see [`Engine::set_advanced`].
    run: RunParams,
    on_gpu: bool,
    /// Bench only (`bench --ctx fit`): cut `audio_ctx` to the clip. Never set from config: the
    /// accuracy guard failed (repeat loops, CER 0.44 against 0.02), so the setting `Fit` stays a no-op.
    bench_fit_ctx: bool,
}

fn logical_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

fn open(
    path: &Path,
    gpu: Option<usize>,
    flash_attn: bool,
) -> Result<(WhisperContext, WhisperState), WhisperError> {
    let mut params = WhisperContextParameters::new();
    params.use_gpu(gpu.is_some());
    params.flash_attn(flash_attn);
    if let Some(i) = gpu {
        params.gpu_device(i32::try_from(i).unwrap_or(0));
    }
    let ctx = WhisperContext::new_with_params(path, params)?;
    // The GPU backend, KV cache and compute buffers come up here, not in new_with_params.
    let state = ctx.create_state()?;
    Ok((ctx, state))
}

/// The GPU open: flash attention as asked, then one plain try when the device rejects it.
fn open_gpu(
    path: &Path,
    i: usize,
    adv: &Advanced,
) -> Result<(WhisperContext, WhisperState), WhisperError> {
    let flash = tuning::flash_attn(adv, true).unwrap_or(true);
    let opened = open(path, Some(i), flash);
    if flash && opened.is_err() {
        return open(path, Some(i), false);
    }
    opened
}

impl Engine {
    /// Load a model on the chosen device. A GPU failure (backend start, weights,
    /// KV cache or compute buffers) retries once on CPU; see [`LoadInfo::device`].
    pub fn load(path: &Path, choice: &DeviceChoice) -> Result<Self, Error> {
        Self::load_with(path, choice, &Advanced::default())
    }

    /// [`Self::load`] with the context parameters of `adv` (flash attention) and its per-run ones.
    pub fn load_with(path: &Path, choice: &DeviceChoice, adv: &Advanced) -> Result<Self, Error> {
        watch_logs();
        ALLOC_FAILED.store(false, Ordering::Relaxed);
        let devices = list_devices();
        let picked = pick(&devices, choice);
        let warning = not_found(choice, &picked);
        let gpu_err = match picked.index {
            None => {
                let flash = tuning::flash_attn(adv, false).unwrap_or(false);
                let (ctx, state) = open(path, None, flash).map_err(load_error)?;
                let info = LoadInfo {
                    device: None,
                    warning,
                };
                return Ok(Self::new(ctx, state, info, adv));
            }
            Some(i) if !device::gpu_starts(i) => format!("cannot start {}", devices[i].name),
            // Flash attention as asked (on by default with a GPU); a device that rejects it gets one
            // plain GPU try before the CPU retry.
            Some(i) => match open_gpu(path, i, adv) {
                Ok((ctx, state)) => {
                    let info = LoadInfo {
                        device: Some(devices[i].name.clone()),
                        warning,
                    };
                    return Ok(Self::new(ctx, state, info, adv));
                }
                // "out of memory" is the words `strings::is_out_of_memory` looks for.
                Err(_) if ALLOC_FAILED.swap(false, Ordering::Relaxed) => "out of memory".to_owned(),
                Err(e) => e.to_string(),
            },
        };
        let flash = tuning::flash_attn(adv, false).unwrap_or(false);
        let (ctx, state) = open(path, None, flash).map_err(load_error)?;
        let info = LoadInfo {
            device: None,
            warning: Some(EngineWarning::CpuFallback { reason: gpu_err }),
        };
        Ok(Self::new(ctx, state, info, adv))
    }

    fn new(ctx: WhisperContext, state: WhisperState, info: LoadInfo, adv: &Advanced) -> Self {
        let on_gpu = info.device.is_some();
        Self {
            ctx,
            state,
            info,
            run: tuning::resolve(adv, on_gpu, logical_threads()),
            on_gpu,
            bench_fit_ctx: false,
        }
    }

    /// Takes the per-run parameters of `adv` for the next jobs (flash attention is a load
    /// parameter: a change of it needs a new load).
    pub fn set_advanced(&mut self, adv: &Advanced) {
        self.run = tuning::resolve(adv, self.on_gpu, logical_threads());
    }

    /// Bench only: run the next jobs with [`fit_audio_ctx`] (see the field). The config's
    /// `audio_ctx` never reaches the decoder.
    pub fn set_bench_fit_ctx(&mut self, on: bool) {
        self.bench_fit_ctx = on;
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
        self.transcribe_with(audio, language, prompt, None, None)
    }

    /// [`Self::transcribe`] with whisper's progress (0 to 100, from the run's own thread) and a
    /// cancel check polled between decode steps (from ggml's threads, hence `Sync`); a cancelled
    /// run answers [`Error::Cancelled`].
    /// Raw callbacks on purpose: the `_safe` ones leak their closure on every call.
    pub fn transcribe_with(
        &mut self,
        audio: &[f32],
        language: Language,
        prompt: &str,
        mut progress: Option<&mut dyn FnMut(u8)>,
        cancel: Option<&Cancel>,
    ) -> Result<Transcript, Error> {
        let run = &self.run;
        let mut params = FullParams::new(match run.sampling {
            Sampling::Greedy { best_of } => SamplingStrategy::Greedy {
                best_of: c_int::from(best_of),
            },
            Sampling::Beam { beam_size } => SamplingStrategy::BeamSearch {
                beam_size: c_int::from(beam_size),
                patience: -1.0,
            },
        });
        params.set_language(Some(language.as_str()));
        // set_initial_prompt panics on NUL.
        let prompt = prompt.replace('\0', "");
        if !prompt.is_empty() {
            params.set_initial_prompt(&prompt);
        }
        params.set_n_threads(c_int::from(run.threads));
        params.set_temperature(run.temperature);
        params.set_temperature_inc(if run.temperature_fallback { 0.2 } else { 0.0 });
        params.set_no_speech_thold(run.no_speech_threshold);
        params.set_logprob_thold(run.logprob_threshold);
        params.set_entropy_thold(run.entropy_threshold);
        params.set_suppress_nst(run.suppress_non_speech);
        if let Some(ctx) = audio_ctx_override(self.bench_fit_ctx, audio.len()) {
            params.set_audio_ctx(ctx);
        }
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);

        // SAFETY: both user pointers outlive `full` below. The progress one is used on this
        // thread only (`&mut`); the abort one is a shared `&Cancel`, which is `Sync`.
        unsafe {
            if let Some(progress) = progress.as_mut() {
                params.set_progress_callback(Some(on_progress));
                params.set_progress_callback_user_data(std::ptr::from_mut(progress).cast());
            }
            if let Some(cancel) = &cancel {
                params.set_abort_callback(Some(on_abort));
                params.set_abort_callback_user_data(std::ptr::from_ref(cancel).cast_mut().cast());
            }
        }
        let ran = self.state.full(params, audio);
        if cancel.is_some_and(|c| c()) {
            return Err(Error::Cancelled);
        }
        ran.map_err(Error::Run)?;
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

unsafe extern "C" fn on_progress(
    _ctx: *mut sys::whisper_context,
    _state: *mut sys::whisper_state,
    percent: c_int,
    user: *mut c_void,
) {
    // SAFETY: `user` is the `&mut &mut dyn FnMut(u8)` of the running `transcribe_with`.
    let progress = unsafe { &mut *user.cast::<&mut dyn FnMut(u8)>() };
    progress(u8::try_from(percent.clamp(0, 100)).unwrap_or(100));
}

unsafe extern "C" fn on_abort(user: *mut c_void) -> bool {
    // SAFETY: `user` is the `&&Cancel` of the running `transcribe_with`.
    let cancel = unsafe { &*user.cast::<&Cancel>() };
    cancel()
}

/// The `audio_ctx` to force: only for the bench's Fit, never from config.
fn audio_ctx_override(bench_fit: bool, samples: usize) -> Option<c_int> {
    bench_fit.then(|| fit_audio_ctx(samples)).flatten()
}

/// Encoder frames for a clip of `samples` (16 kHz) under the bench's Fit: 50 frames a second
/// plus 96 of headroom, up to a multiple of 64, at least 256. `None` = the full 1500-frame window
/// (from 1280 up there is nothing left to save, and a clip over 30 s needs the whole window).
pub fn fit_audio_ctx(samples: usize) -> Option<c_int> {
    let frames = samples.div_ceil(320) + 96;
    let ctx = frames.next_multiple_of(64).max(256);
    if ctx >= 1280 {
        None
    } else {
        c_int::try_from(ctx).ok()
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
    fn only_a_missing_named_gpu_warns() {
        let picked = |warns: bool| Picked {
            index: None,
            missing: warns,
        };
        let named = DeviceChoice::Named("Vulkan3".into());
        assert_eq!(
            not_found(&named, &picked(true)),
            Some(EngineWarning::GpuNotFound {
                name: "Vulkan3".into()
            })
        );
        assert_eq!(not_found(&named, &picked(false)), None);
        assert_eq!(not_found(&DeviceChoice::Auto, &picked(false)), None);
    }

    #[test]
    fn allocation_failures_are_told_from_other_log_lines() {
        for (line, want) in [
            (
                "ggml_backend_cpu_buffer_type_alloc_buffer: failed to allocate buffer of size 3000000000",
                true,
            ),
            ("Vulkan: Out of Memory", true),
            ("whisper_init_state: kv self size = 83.89 MB", false),
        ] {
            assert_eq!(is_alloc_failure(line), want, "{line}");
        }
    }

    /// A stderr that is gone (closed pipe) must not panic the log callback.
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn log_echo_skips_debug_and_survives_a_closed_stderr() {
        echo(
            &mut Closed,
            sys::ggml_log_level_GGML_LOG_LEVEL_INFO,
            "info\n",
        );
        let mut out = Vec::new();
        echo(
            &mut out,
            sys::ggml_log_level_GGML_LOG_LEVEL_DEBUG,
            "debug\n",
        );
        echo(&mut out, sys::ggml_log_level_GGML_LOG_LEVEL_WARN, "warn\n");
        assert_eq!(out, b"warn\n");
    }

    /// Needs `OXI_TINY_MODEL` and `OXI_JFK_WAV` (the transcribe tests' inputs); skips without.
    /// Also the timing check: three plain and three hooked runs, interleaved, printed.
    #[test]
    fn progress_and_cancel_on_a_real_model_and_what_they_cost() {
        let (Some(model), Some(wav)) = (
            std::env::var_os("OXI_TINY_MODEL"),
            std::env::var_os("OXI_JFK_WAV"),
        ) else {
            eprintln!("skipped: set OXI_TINY_MODEL and OXI_JFK_WAV");
            return;
        };
        let (audio, _) = oxi_platform::wav::read_wav_mono_f32(Path::new(&wav)).unwrap();
        let mut engine = Engine::load(Path::new(&model), &DeviceChoice::Cpu).unwrap();
        let ms = |f: &mut dyn FnMut()| {
            let t = std::time::Instant::now();
            f();
            t.elapsed().as_millis()
        };
        let (mut plain, mut hooked) = (Vec::new(), Vec::new());
        let mut seen = Vec::new();
        let never = || false;
        for _ in 0..3 {
            plain.push(ms(&mut || {
                engine.transcribe(&audio, Language::EN, "").unwrap();
            }));
            seen.clear();
            hooked.push(ms(&mut || {
                let mut on = |p| seen.push(p);
                engine
                    .transcribe_with(&audio, Language::EN, "", Some(&mut on), Some(&never))
                    .unwrap();
            }));
        }
        eprintln!("plain {plain:?} ms, with progress+abort {hooked:?} ms, progress {seen:?}");
        assert!(
            !seen.is_empty()
                && seen.windows(2).all(|w| w[0] <= w[1])
                && seen[seen.len() - 1] <= 100
        );
        let always = || true;
        let r = engine.transcribe_with(&audio, Language::EN, "", None, Some(&always));
        assert!(matches!(r, Err(Error::Cancelled)));
        // The engine still works after a cancel.
        engine.transcribe(&audio, Language::EN, "").unwrap();
    }

    #[test]
    fn fit_audio_ctx_table() {
        let secs = |s: usize| s * 16_000;
        for (samples, want) in [
            (0, Some(256)),
            (secs(1), Some(256)),
            (secs(5), Some(384)),
            (secs(10), Some(640)),
            (secs(20), Some(1152)),
            (secs(24), None),
            (secs(31), None),
        ] {
            assert_eq!(fit_audio_ctx(samples), want, "{samples} samples");
        }
    }

    #[test]
    fn only_the_bench_forces_audio_ctx() {
        assert_eq!(audio_ctx_override(false, 16_000), None);
        assert_eq!(audio_ctx_override(true, 16_000), Some(256));
        assert_eq!(audio_ctx_override(true, 31 * 16_000), None);
    }

    #[test]
    fn centis_to_millis() {
        assert_eq!(centis(150), Millis(1500));
        assert_eq!(centis(-3), Millis(0));
    }
}
