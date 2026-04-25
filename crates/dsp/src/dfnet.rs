//! DeepFilterNet3 wrapper.
//!
//! The `df` crate handles model loading (weights are bundled when the
//! `default-model` feature is on), STFT/iSTFT, and the actual inference via
//! the `tract` runtime — no Python, no ONNX Runtime, no GPU dependency,
//! no network at runtime.
//!
//! The crate's processing API is "give me N samples, get N samples back",
//! frame-aligned to its internal hop size (480 @ 48 kHz). Our pipeline is
//! already aligned to that hop, so each `process_frame` call is one model
//! step.

use crate::{Denoiser, DspError, Result, FRAME_SAMPLES};

pub struct DeepFilterNet {
    inner: df::tract::DfTract,
    /// Output of the last inference, kept around so we don't reallocate per
    /// frame. The model is causal — output of frame N depends on frame N
    /// and the model's internal state.
    scratch_in: Vec<f32>,
    scratch_out: Vec<f32>,
}

impl DeepFilterNet {
    pub fn new() -> Result<Self> {
        // DfParams::default() loads the bundled model. Configure attenuation
        // limit (max dB suppression) — 100 dB ~= "remove anything that isn't
        // speech". Lower this if you want more residual ambiance.
        let params = df::DfParams::default();
        let mut model = df::tract::DfTract::new(params, &df::RuntimeParams::default_with_ch(1))
            .map_err(|e| DspError::Load(format!("DfTract::new: {e}")))?;
        model.set_atten_lim(100.0);
        Ok(Self {
            inner: model,
            scratch_in: vec![0.0; FRAME_SAMPLES],
            scratch_out: vec![0.0; FRAME_SAMPLES],
        })
    }

    /// Override the post-filter attenuation limit (dB). Higher values =
    /// more aggressive noise removal at the cost of more speech distortion.
    /// Range: roughly 6.0 - 100.0; default 100.0.
    pub fn set_attenuation_db(&mut self, db: f32) {
        self.inner.set_atten_lim(db);
    }
}

impl Denoiser for DeepFilterNet {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.scratch_in.copy_from_slice(frame);
        // The crate expects a 2D view: channels × samples. We're mono.
        let input = ndarray::ArrayView2::from_shape((1, FRAME_SAMPLES), &self.scratch_in)
            .map_err(|e| DspError::Inference(format!("input shape: {e}")))?;
        let mut output = ndarray::ArrayViewMut2::from_shape((1, FRAME_SAMPLES), &mut self.scratch_out)
            .map_err(|e| DspError::Inference(format!("output shape: {e}")))?;
        self.inner
            .process(input, output.view_mut())
            .map_err(|e| DspError::Inference(format!("DfTract::process: {e}")))?;
        frame.copy_from_slice(&self.scratch_out);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "DeepFilterNet3"
    }
}
