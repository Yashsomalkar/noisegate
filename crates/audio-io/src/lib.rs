//! Audio I/O for NoiseGate.
//!
//! Wraps WASAPI capture (microphone) and render (VB-Cable Input) endpoints
//! with a minimal trait-based API. Windows-only; building on other platforms
//! gives stubs that panic at runtime so the workspace stays buildable for
//! `cargo check` on dev machines.

#![cfg_attr(not(windows), allow(dead_code, unused_variables))]

pub mod devices;
pub mod error;
pub mod format;

#[cfg(windows)]
pub mod mmcss;

/// Convenience: promote the calling thread to MMCSS "Pro Audio" priority.
/// Returns a guard; drop reverts. Used by the pipeline crate's DSP thread.
#[cfg(windows)]
pub fn mmcss_pro_audio_for_current_thread() -> Option<mmcss::ProAudio> {
    mmcss::ProAudio::set_for_current_thread()
}
#[cfg(windows)]
pub mod wasapi_capture;
#[cfg(windows)]
pub mod wasapi_render;

#[cfg(windows)]
pub use wasapi_capture::WasapiCapture;
#[cfg(windows)]
pub use wasapi_render::WasapiRender;

pub use devices::{Device, DeviceDirection, DeviceList};
pub use error::AudioError;
pub use format::StreamFormat;

/// One audio frame is 480 samples @ 48 kHz mono = 10 ms.
/// DeepFilterNet's native frame size; aligning the whole pipeline here
/// avoids any reblocking inside the DSP path.
pub const FRAME_SAMPLES: usize = 480;
pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME_PERIOD_MS: u32 = 10;
