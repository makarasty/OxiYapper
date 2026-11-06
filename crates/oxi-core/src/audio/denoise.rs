use super::DENOISE_FRAME;
use nnnoiseless::DenoiseState;

const _: () = assert!(DenoiseState::FRAME_SIZE == DENOISE_FRAME);
/// `RNNoise` works on samples scaled like i16.
const SCALE: f32 = 32768.0;

/// `RNNoise` on 48 kHz mono, any push size; whole [`DENOISE_FRAME`] frames are processed.
pub struct Denoiser {
    state: Box<DenoiseState<'static>>,
    pending: Vec<f32>,
}

impl Default for Denoiser {
    fn default() -> Self {
        Self::new()
    }
}

impl Denoiser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            pending: Vec::new(),
        }
    }

    /// Denoise `samples` (48 kHz mono); returns the frames completed so far.
    pub fn push(&mut self, samples: &[f32]) -> Vec<f32> {
        self.pending.extend_from_slice(samples);
        let mut out = Vec::with_capacity(self.pending.len());
        let mut frame = [0.0; DENOISE_FRAME];
        let mut done = 0;
        for chunk in self.pending.as_chunks::<DENOISE_FRAME>().0 {
            let scaled: Vec<f32> = chunk.iter().map(|s| s * SCALE).collect();
            self.state.process_frame(&mut frame, &scaled);
            out.extend(frame.iter().map(|s| s / SCALE));
            done += DENOISE_FRAME;
        }
        self.pending.drain(..done);
        out
    }

    /// Process the last partial frame (zero padded), keeping only its real length.
    #[must_use]
    pub fn finish(mut self) -> Vec<f32> {
        let tail = self.pending.len();
        if tail == 0 {
            return Vec::new();
        }
        let mut out = self.push(&vec![0.0; DENOISE_FRAME - tail]);
        out.truncate(tail);
        out
    }
}
