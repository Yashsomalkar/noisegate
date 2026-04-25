//! WASAPI shared low-latency render to the VB-Cable Input endpoint.
//!
//! Pulls 480-sample mono f32 frames from a `FrameSource` and writes them
//! to the chosen render device. If the device's mix format isn't mono
//! 48 kHz f32 we up-mix and (linear) resample inline, mirroring the
//! capture path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use windows::core::PCWSTR;
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::error::{AudioError, Result};
use crate::wasapi_capture::Frame;
use crate::{FRAME_SAMPLES, SAMPLE_RATE};

#[allow(non_upper_case_globals)]
const CLSID_MMDeviceEnumerator: windows::core::GUID =
    windows::core::GUID::from_u128(0xBCDE0395_E52F_467C_8E3D_C4579291692E);

/// Source of frames for the render thread. Must be lock-free / wait-free
/// since it's polled from the audio engine's tick. Returning `None`
/// renders silence for one period.
pub trait FrameSource: Send {
    fn next_frame(&mut self) -> Option<Frame>;
    fn on_underrun(&mut self) {}
}

pub struct WasapiRender {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiRender {
    pub fn start(device_id: &str, mut source: Box<dyn FrameSource>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let device_id = device_id.to_string();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        let thread = std::thread::Builder::new()
            .name("noisegate-render".into())
            .spawn(move || {
                if let Err(e) = render_loop(&device_id, &mut *source, &stop_thread, &ready_tx) {
                    tracing::error!(error = %e, "render loop exited with error");
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| AudioError::Other(anyhow::anyhow!("spawn render thread: {e}")))?;

        ready_rx.recv().map_err(|_| AudioError::ThreadDied)??;
        Ok(Self { stop, thread: Some(thread) })
    }
}

impl Drop for WasapiRender {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn render_loop(
    device_id: &str,
    source: &mut dyn FrameSource,
    stop: &AtomicBool,
    ready_tx: &std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&CLSID_MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| AudioError::wasapi("CoCreateInstance(MMDeviceEnumerator)", e))?;

        let device = if device_id.is_empty() {
            enumerator
                .GetDefaultAudioEndpoint(eRender, eCommunications)
                .map_err(|e| AudioError::wasapi("GetDefaultAudioEndpoint", e))?
        } else {
            let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
            enumerator
                .GetDevice(PCWSTR::from_raw(wide.as_ptr()))
                .map_err(|e| AudioError::wasapi("GetDevice", e))?
        };

        let client: IAudioClient3 = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| AudioError::wasapi("IMMDevice::Activate", e))?;

        // Honour the device's preferred mix format for compatibility — VB-Cable
        // typically negotiates stereo 48 kHz f32, which we up-mix into.
        let ptr = client
            .GetMixFormat()
            .map_err(|e| AudioError::wasapi("GetMixFormat", e))?;
        let mix_format = *ptr;
        windows::Win32::System::Com::CoTaskMemFree(Some(ptr as _));

        // Legacy Initialize for portability across device formats — see the
        // matching note in wasapi_capture.rs.
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                0,                  // hnsBufferDuration: engine default
                0,                  // hnsPeriodicity: must be 0 in shared mode
                &mix_format,
                None,
            )
            .map_err(|e| AudioError::wasapi("IAudioClient::Initialize", e))?;

        let event = CreateEventW(None, false, false, PCWSTR::null())
            .map_err(|e| AudioError::wasapi("CreateEventW", e))?;
        client
            .SetEventHandle(event)
            .map_err(|e| AudioError::wasapi("SetEventHandle", e))?;

        let render_client: IAudioRenderClient = client
            .GetService()
            .map_err(|e| AudioError::wasapi("GetService(IAudioRenderClient)", e))?;

        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| AudioError::wasapi("GetBufferSize", e))?;

        let _mmcss = crate::mmcss::ProAudio::set_for_current_thread();

        // Pre-fill with silence so the engine doesn't underrun on the first tick.
        let prefill = render_client
            .GetBuffer(buffer_frames)
            .map_err(|e| AudioError::wasapi("GetBuffer(prefill)", e))?;
        std::ptr::write_bytes(
            prefill,
            0,
            (buffer_frames * mix_format.nBlockAlign as u32) as usize,
        );
        render_client
            .ReleaseBuffer(buffer_frames, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)
            .map_err(|e| AudioError::wasapi("ReleaseBuffer(prefill)", e))?;

        client.Start().map_err(|e| AudioError::wasapi("Start", e))?;
        let _ = ready_tx.send(Ok(()));

        let mut upconverter = UpConverter::new(
            SAMPLE_RATE,
            mix_format.nSamplesPerSec,
            mix_format.nChannels as usize,
        );
        let mut pending: Vec<f32> = Vec::with_capacity(FRAME_SAMPLES * 2);

        while !stop.load(Ordering::Acquire) {
            let wait = WaitForSingleObject(event, 200);
            if wait != WAIT_OBJECT_0 {
                continue;
            }

            let padding = client
                .GetCurrentPadding()
                .map_err(|e| AudioError::wasapi("GetCurrentPadding", e))?;
            let frames_writable = buffer_frames.saturating_sub(padding);
            if frames_writable == 0 {
                continue;
            }

            // Pull mono frames until we have enough pre-converted samples to
            // fill `frames_writable` frames at the device rate.
            let needed_src = ((frames_writable as u64 * SAMPLE_RATE as u64
                + mix_format.nSamplesPerSec as u64 - 1)
                / mix_format.nSamplesPerSec as u64) as usize;

            while pending.len() < needed_src {
                match source.next_frame() {
                    Some(f) => pending.extend_from_slice(&f),
                    None => {
                        source.on_underrun();
                        // Pad with silence so we don't busy-loop or stall.
                        pending.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES));
                    }
                }
            }

            let buf = render_client
                .GetBuffer(frames_writable)
                .map_err(|e| AudioError::wasapi("GetBuffer", e))?;

            let consumed = upconverter.write_into(
                &pending[..needed_src.min(pending.len())],
                buf as *mut f32,
                frames_writable as usize,
            );
            // Drop the source samples we used.
            pending.drain(..consumed);

            render_client
                .ReleaseBuffer(frames_writable, 0)
                .map_err(|e| AudioError::wasapi("ReleaseBuffer", e))?;
        }

        let _ = client.Stop();
        Ok(())
    }
}

/// Mono 48 kHz → multi-channel device-rate f32 interleaved.
struct UpConverter {
    src_rate: u32,
    dst_rate: u32,
    dst_channels: usize,
    phase: f64,
    last: f32,
}

impl UpConverter {
    fn new(src_rate: u32, dst_rate: u32, dst_channels: usize) -> Self {
        Self { src_rate, dst_rate, dst_channels, phase: 0.0, last: 0.0 }
    }

    /// Linearly resample mono `src` (48 kHz) into `frames` device-rate
    /// frames written to `dst` (interleaved, dst_channels). Returns the
    /// number of source samples consumed so the caller can advance.
    unsafe fn write_into(&mut self, src: &[f32], dst: *mut f32, frames: usize) -> usize {
        if src.is_empty() {
            std::ptr::write_bytes(dst, 0, frames * self.dst_channels);
            return 0;
        }
        let ratio = self.src_rate as f64 / self.dst_rate as f64;
        let mut consumed_max = 0usize;
        for f in 0..frames {
            let pos = self.phase + f as f64 * ratio;
            let idx = pos as usize;
            let frac = pos - idx as f64;
            let a = if idx == 0 { self.last } else { src.get(idx - 1).copied().unwrap_or(self.last) };
            let b = src.get(idx).copied().unwrap_or(self.last);
            let s = (a as f64 + (b as f64 - a as f64) * frac) as f32;
            for c in 0..self.dst_channels {
                *dst.add(f * self.dst_channels + c) = s;
            }
            consumed_max = consumed_max.max(idx);
        }
        // Advance phase past the samples we used; keep the fractional part
        // so we don't drift across calls.
        let advance = frames as f64 * ratio + self.phase;
        let consumed = advance as usize;
        self.phase = advance - consumed as f64;
        if consumed > 0 {
            self.last = src[(consumed - 1).min(src.len() - 1)];
        }
        consumed.min(src.len())
    }
}
