//! DeepFilterNet3 backend via the `deep-filter-rt` crate.
//!
//! Full-band (48 kHz) deep-learning noise suppression. Much better than
//! RNNoise on non-stationary noise (keyboards, clicks, voices in the
//! background) at the cost of ~30 ms extra latency and a few percent of one
//! core. Native hop is 480 samples @ 48 kHz — same as our pipeline frame, so
//! frames go through 1:1.
//!
//! `deep-filter-rt` is the inference half of upstream DeepFilterNet's
//! `libDF`, published on crates.io. Its embedded model files are byte-identical
//! to upstream's `models/DeepFilterNet3_onnx.tar.gz` (v0.5.6). Inference runs
//! through `tract` (pure Rust) — no ONNX Runtime DLL, no network, no build
//! script.

use deep_filter_rt::tract::{DfParams, DfTract, RuntimeParams};
use ndarray17::Array2;

use crate::{Denoiser, DspError, Result, FRAME_SAMPLES};

pub struct DeepFilter {
    df: DfTract,
    noisy: Array2<f32>,
    enhanced: Array2<f32>,
}

impl DeepFilter {
    /// `atten_lim_db` caps how much noise is removed. 100 = unlimited;
    /// lower values (e.g. 12–24) sound more natural and keep more room tone.
    pub fn new(atten_lim_db: f32) -> Result<Self> {
        let rp = RuntimeParams::default_with_ch(1).with_atten_lim(atten_lim_db);
        let df = DfTract::new(DfParams::default(), &rp)
            .map_err(|e| DspError::Load(format!("DeepFilterNet3: {e}")))?;
        if df.hop_size != FRAME_SAMPLES || df.sr != crate::SAMPLE_RATE as usize {
            return Err(DspError::Load(format!(
                "DeepFilterNet3 expects {} Hz / {}-sample hops, pipeline is {} Hz / {}",
                df.sr,
                df.hop_size,
                crate::SAMPLE_RATE,
                FRAME_SAMPLES
            )));
        }
        Ok(Self {
            df,
            noisy: Array2::zeros((1, FRAME_SAMPLES)),
            enhanced: Array2::zeros((1, FRAME_SAMPLES)),
        })
    }
}

impl Denoiser for DeepFilter {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        for (dst, &src) in self.noisy.iter_mut().zip(frame.iter()) {
            *dst = src;
        }
        self.df
            .process(self.noisy.view(), self.enhanced.view_mut())
            .map_err(|e| DspError::Inference(e.to_string()))?;
        for (dst, &src) in frame.iter_mut().zip(self.enhanced.iter()) {
            *dst = src;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "DeepFilterNet3"
    }
}
