use super::{AudioError, CHUNK, samples_for};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};

/// Mono streaming resampler: feeds rubato in [`CHUNK`]-sized blocks, so `push` can be called with
/// whatever the capture callback delivers. The filter delay is removed from the start of the output.
pub struct StreamResampler {
    inner: Fft<f32>,
    in_rate: u32,
    out_rate: u32,
    chunk_in: usize,
    pending: Vec<f32>,
    delay_left: usize,
    total_in: u64,
    total_out: u64,
}

impl StreamResampler {
    /// # Errors
    /// [`AudioError::Resample`] for a rate of zero.
    pub fn new(in_rate: u32, out_rate: u32) -> Result<Self, AudioError> {
        let chunk_in = samples_for(CHUNK, in_rate).max(1);
        let inner = Fft::<f32>::new(
            in_rate as usize,
            out_rate as usize,
            chunk_in,
            1,
            FixedSync::Input,
        )
        .map_err(|e| AudioError::Resample(e.to_string()))?;
        Ok(Self {
            delay_left: inner.output_delay(),
            inner,
            in_rate,
            out_rate,
            chunk_in,
            pending: Vec::new(),
            total_in: 0,
            total_out: 0,
        })
    }

    /// Add input samples; returns all output that is ready (whole chunks only).
    ///
    /// # Errors
    /// [`AudioError::Resample`] if rubato rejects a buffer.
    pub fn push(&mut self, samples: &[f32]) -> Result<Vec<f32>, AudioError> {
        self.total_in += samples.len() as u64;
        self.pending.extend_from_slice(samples);
        let mut out = Vec::new();
        while self.pending.len() >= self.chunk_in {
            let chunk: Vec<f32> = self.pending.drain(..self.chunk_in).collect();
            self.run(&chunk, &mut out)?;
        }
        Ok(out)
    }

    /// Flush the tail so the whole input is out: total output is `in_len * out_rate / in_rate`.
    ///
    /// # Errors
    /// [`AudioError::Resample`] if rubato rejects a buffer.
    pub fn finish(mut self) -> Result<Vec<f32>, AudioError> {
        let expected = self.total_in * u64::from(self.out_rate) / u64::from(self.in_rate);
        let mut out = Vec::new();
        let mut chunk = std::mem::take(&mut self.pending);
        while self.total_out < expected {
            chunk.resize(self.chunk_in, 0.0);
            self.run(&chunk, &mut out)?;
            chunk.clear();
        }
        let extra = usize::try_from(self.total_out - expected).unwrap_or(usize::MAX);
        out.truncate(out.len().saturating_sub(extra));
        Ok(out)
    }

    fn run(&mut self, chunk: &[f32], out: &mut Vec<f32>) -> Result<(), AudioError> {
        let err = |e: &dyn std::fmt::Display| AudioError::Resample(e.to_string());
        let mut block = vec![0.0; self.inner.output_frames_next()];
        let frames = block.len();
        let input = InterleavedSlice::new(chunk, 1, chunk.len()).map_err(|e| err(&e))?;
        let mut output = InterleavedSlice::new_mut(&mut block, 1, frames).map_err(|e| err(&e))?;
        let (_, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&Indexing::new()))
            .map_err(|e| err(&e))?;
        let skip = self.delay_left.min(produced);
        self.delay_left -= skip;
        out.extend_from_slice(&block[skip..produced]);
        self.total_out += (produced - skip) as u64;
        Ok(())
    }
}
