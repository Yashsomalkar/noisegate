//! DSP layer for NoiseGate.
//!
//! Defines a [`Denoiser`] trait so the pipeline doesn't care which model is
//! running underneath. The default backend is RNNoise (via `nnnoiseless`)
//! — published, mature, pure Rust, ships everywhere. The optional `onnx`
//! feature swaps in any ONNX-exported denoise model (e.g. DeepFilterNet3
//! from Hugging Face) for higher quality at the cost of a runtime DLL.

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

#[cfg(feature = "rnnoise")]
mod rnnoise;
#[cfg(feature = "deepfilter")]
mod deepfilter;
#[cfg(feature = "onnx")]
mod onnx;

#[cfg(feature = "rnnoise")]
pub use rnnoise::RnNoise;
#[cfg(feature = "deepfilter")]
pub use deepfilter::DeepFilter;
#[cfg(feature = "onnx")]
pub use onnx::OnnxDenoiser;

/// A selectable denoiser, as stored in config and shown in the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backend {
    /// Stable config key.
    pub key: &'static str,
    /// Menu label.
    pub label: &'static str,
}

/// Backends compiled into this build, best quality first.
pub const BACKENDS: &[Backend] = &[
    #[cfg(feature = "deepfilter")]
    Backend { key: "deepfilter", label: "DeepFilterNet3 (high quality)" },
    #[cfg(feature = "rnnoise")]
    Backend { key: "rnnoise", label: "RNNoise (lightweight)" },
];

/// Build the denoiser named by `key` (see [`BACKENDS`]). Unknown or empty
/// keys fall back to the first compiled-in backend. `atten_lim_db` only
/// affects backends that support limiting suppression (DeepFilterNet3).
pub fn denoiser_by_key(key: &str, atten_lim_db: f32) -> Result<Box<dyn Denoiser>> {
    let key = BACKENDS
        .iter()
        .find(|b| b.key == key)
        .or(BACKENDS.first())
        .map(|b| b.key)
        .unwrap_or("");
    let _ = atten_lim_db;
    match key {
        #[cfg(feature = "deepfilter")]
        "deepfilter" => Ok(Box::new(DeepFilter::new(atten_lim_db)?)),
        #[cfg(feature = "rnnoise")]
        "rnnoise" => Ok(Box::new(RnNoise::new()?)),
        _ => Err(DspError::Load(
            "no denoiser backend compiled in (enable `deepfilter`, `rnnoise` or `onnx`)".into(),
        )),
    }
}

/// Build the default denoiser based on enabled features.
pub fn default_denoiser() -> Result<Box<dyn Denoiser>> {
    denoiser_by_key("", 100.0)
}
