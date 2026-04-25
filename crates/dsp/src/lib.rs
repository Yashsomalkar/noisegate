//! DSP layer for NoiseGate.
//!
//! Defines a [`Denoiser`] trait so the pipeline doesn't care which model is
//! running underneath. Today it's DeepFilterNet3; tomorrow you could swap in
//! an ONNX-exported HF model behind the `onnx` feature without changing the
//! audio pipeline.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub const FRAME_SAMPLES: usize = 480;
pub const SAMPLE_RATE: u32 = 48_000;

#[derive(Debug, thiserror::Error)]
pub enum DspError {
    #[error("model load failed: {0}")]
    Load(String),
    #[error("inference failed: {0}")]
    Inference(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, DspError>;

/// Generic denoiser interface. Implementations operate strictly on
/// 480-sample mono f32 frames @ 48 kHz, in-place, single-threaded.
pub trait Denoiser: Send {
    fn process_frame(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()>;
    fn name(&self) -> &'static str;
}

/// Bypass mode + latency stats wrapper. The pipeline thread holds this and
/// flips the bypass flag from the UI thread.
pub struct DenoiserHost {
    inner: Box<dyn Denoiser>,
    bypass: Arc<AtomicBool>,
    stats: Arc<Stats>,
}

#[derive(Default)]
pub struct Stats {
    /// Total frames processed (incl. bypassed).
    pub frames: AtomicU64,
    /// Cumulative DSP time in nanoseconds (excludes bypassed frames).
    pub dsp_ns: AtomicU64,
    /// Peak per-frame time in ns (sticky high-water mark, useful for
    /// surfacing glitch headroom in the tray tooltip).
    pub peak_frame_ns: AtomicU64,
}

impl DenoiserHost {
    pub fn new(inner: Box<dyn Denoiser>) -> (Self, Arc<AtomicBool>, Arc<Stats>) {
        let bypass = Arc::new(AtomicBool::new(false));
        let stats = Arc::new(Stats::default());
        let host = Self {
            inner,
            bypass: bypass.clone(),
            stats: stats.clone(),
        };
        (host, bypass, stats)
    }

    pub fn process(&mut self, frame: &mut [f32; FRAME_SAMPLES]) -> Result<()> {
        self.stats.frames.fetch_add(1, Ordering::Relaxed);
        if self.bypass.load(Ordering::Relaxed) {
            return Ok(());
        }
        let start = std::time::Instant::now();
        let r = self.inner.process_frame(frame);
        let elapsed = start.elapsed().as_nanos() as u64;
        self.stats.dsp_ns.fetch_add(elapsed, Ordering::Relaxed);
        // Atomic max via compare-exchange loop.
        let mut peak = self.stats.peak_frame_ns.load(Ordering::Relaxed);
        while elapsed > peak {
            match self.stats.peak_frame_ns.compare_exchange_weak(
                peak,
                elapsed,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => peak = actual,
            }
        }
        r
    }

    pub fn name(&self) -> &'static str {
        self.inner.name()
    }
}

#[cfg(feature = "dfnet3")]
mod dfnet;
#[cfg(feature = "onnx")]
mod onnx;

#[cfg(feature = "dfnet3")]
pub use dfnet::DeepFilterNet;
#[cfg(feature = "onnx")]
pub use onnx::OnnxDenoiser;

/// Build the default denoiser based on enabled features. The pipeline calls
/// this once at startup.
pub fn default_denoiser() -> Result<Box<dyn Denoiser>> {
    #[cfg(feature = "dfnet3")]
    {
        return Ok(Box::new(DeepFilterNet::new()?));
    }
    #[allow(unreachable_code)]
    Err(DspError::Load(
        "no denoiser backend compiled in (enable feature `dfnet3`)".into(),
    ))
}
