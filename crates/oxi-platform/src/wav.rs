//! WAV reading and writing for recordings and bench input.
//!
//! Recordings are 16 kHz mono 16-bit PCM: half the size of f32 and plays everywhere. A file is
//! written as `<name>.wav.part`, finalized and synced, then renamed, so a crash never leaves a
//! half-written `.wav` (ARCHITECTURE section 9, History).

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use hound::{SampleFormat, WavReader, WavSpec, WavWriter};
use oxi_core::audio::{SAMPLE_RATE, downmix};
use oxi_core::paths::part_path;
use thiserror::Error;

use crate::fs::write_via;

#[derive(Debug, Error)]
pub enum WavError {
    #[error("wav io")]
    Io(#[from] std::io::Error),
    #[error("wav format")]
    Hound(#[from] hound::Error),
    #[error("unsupported wav: {0}")]
    Unsupported(String),
}

/// Write 16 kHz mono `samples` (f32 in `[-1, 1]`, clamped) to `path`, which must end in `.wav`.
pub fn write_wav(path: &Path, samples: &[f32]) -> Result<(), WavError> {
    if path.extension().is_none_or(|e| e != "wav") {
        return Err(WavError::Unsupported("path must end in .wav".into()));
    }
    write_via(&part_path(path), path, |file| write_into(file, samples))
}

fn write_into(file: &mut File, samples: &[f32]) -> Result<(), WavError> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };
    let mut writer = WavWriter::new(BufWriter::new(file), spec)?;
    for s in samples {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "clamped to the i16 range first"
        )]
        writer.write_sample((s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)?;
    }
    writer.finalize()?;
    Ok(())
}

/// Read any int (8-32 bit) or f32 wav as mono f32 in `[-1, 1]` (channels averaged) plus its rate.
pub fn read_wav_mono_f32(path: &Path) -> Result<(Vec<f32>, u32), WavError> {
    let mut reader = WavReader::open(path)?;
    let spec = reader.spec();
    let interleaved: Vec<f32> = match (spec.sample_format, spec.bits_per_sample) {
        (SampleFormat::Float, 32) => reader.samples::<f32>().collect::<Result<_, _>>()?,
        (SampleFormat::Int, bits @ 8..=32) => {
            let scale = 2.0_f32.powi(i32::from(bits) - 1);
            reader
                .samples::<i32>()
                .map(|s| s.map(|v| v as f32 / scale))
                .collect::<Result<_, _>>()?
        }
        (format, bits) => {
            return Err(WavError::Unsupported(format!("{format:?} {bits} bit")));
        }
    };
    Ok((
        downmix(&interleaved, usize::from(spec.channels)),
        spec.sample_rate,
    ))
}
