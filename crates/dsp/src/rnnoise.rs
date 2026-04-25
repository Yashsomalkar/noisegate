//! RNNoise backend via the `nnnoiseless` crate.
//!
//! Pure-Rust port of Xiph's RNNoise. The model weights (~85 KB) are
//! embedded in the crate, so there's nothing to download or install at
//! runtime. Frame size is 480 samples @ 48 kHz — same as our pipeline,
//! so we feed frames through 1:1 without reblocking.
//!
//! ## Sample scaling caveat
//! RNNoise (and nnnoiseless) operate on `f32` samples in the **i16 range**
//! (roughly ±32_768), not the normalized ±1.0 that WASAPI gives us. We
//! scale up on the way in and scale down on the way out. The scratch
//! buffers are reused per call so we don't allocate in the hot path.

use nnnoiseless::DenoiseState;

use crate::{Denoiser, DspError, Result, FRAME_SAMPLES};

/// Conversion factor between WASAPI's normalized f32 and RNNoise's i16-scale f32.
const I16_SCALE: f32 = 32_768.0;

pub struct RnNoise {
    state: Box<DenoiseState<'static>>,
    scratch_in: [f32; FRAME_SAMPLES],
    scratch_out: [f32; FRAME_SAMPLES],
}

impl RnNoise {
    pub fn new() -> Result<Self> {
        // DenoiseState::new() bakes in the bundled model weights.
        Ok(Self {
            state: DenoiseState::new(),
            scratch_in: [0.0; FRAME_SAMPLES],
            scratch_out: [0.0; FRAME_SAMPLES],
        })
    }
}

impl Denoiser for RnNoise {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        // Scale [-1, 1] → [-32768, 32767] for the model.
        for (dst, &src) in self.scratch_in.iter_mut().zip(frame.iter()) {
            *dst = src * I16_SCALE;
        }

        // process_frame returns voice-activity probability in [0, 1]; we
        // ignore it for now but it's a useful hook for future "VAD-gated
        // suppression" tweaks (e.g. apply more attenuation when VAD says
        // there's no speech).
        let _voice_prob = self
            .state
            .process_frame(&mut self.scratch_out, &self.scratch_in);

        // Scale back to normalized.
        for (dst, &src) in frame.iter_mut().zip(self.scratch_out.iter()) {
            *dst = src / I16_SCALE;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "RNNoise (nnnoiseless)"
    }
}
